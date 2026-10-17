//! Putting a host directory into a guest before running something there.
//!
//! The tree goes as one tar archive rather than as a recursive copy. A copy
//! would carry build output and `.git` along — gigabytes for a Rust project —
//! and has no way to leave them out, while an archive is built from what
//! `.gitignore` keeps. The guest unpacks it with the `tar` that Windows 10 and
//! later, macOS and every Linux image ship.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};

use crate::{ssh, Guest};

/// The archive's name in the guest's home while it is being unpacked.
const ARCHIVE: &str = ".vitro-sync.tar";

/// Where a synced directory lands, relative to the guest account's home.
pub fn guest_dir(dir: &Path) -> Result<String> {
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("cannot sync {}", dir.display()))?;
    if !canonical.is_dir() {
        bail!("cannot sync {}: it is not a directory", dir.display());
    }
    let Some(name) = canonical.file_name() else {
        bail!(
            "cannot sync {}: it has no name to give the guest directory",
            dir.display()
        );
    };
    Ok(name.to_string_lossy().into_owned())
}

/// Pack `dir` into a tar archive at `out`, every entry under `<root>/`.
///
/// What `.gitignore` (and `.ignore`, and `.git/info/exclude`) leave out stays
/// out, whether or not the directory is a Git checkout. Hidden files go in:
/// `.cargo/config.toml` is part of a build. `.git` itself does not.
///
/// A symbolic link to something inside the directory goes in as a copy of
/// what it points at, under the link's own name, so a build that reaches a
/// file through the link still finds it. A link to anything outside is left
/// out and returned, because the guest runs whatever the project's command
/// does, and a link to `~/.ssh` in a cloned repository must not carry the key
/// into it. So is a link into `.git`, one whose target is gone, and one that
/// would contain itself.
/// Links are never sent as links, since unpacking one on Windows needs a
/// privilege the guest account does not have.
///
/// Links are followed by the walker itself rather than by hand, so a link to
/// a directory meets `.gitignore` as the directory it is: `/cache/` leaves out
/// a `cache` link too.
pub fn archive(dir: &Path, root: &str, out: &Path) -> Result<Vec<PathBuf>> {
    let file = File::create(out).with_context(|| format!("cannot create {}", out.display()))?;
    let mut builder = tar::Builder::new(file);
    // Not `Deterministic`: it stamps every file with one fixed date in the
    // past, which makes edited sources look older than an earlier build's
    // outputs in the guest.
    builder.mode(tar::HeaderMode::Complete);
    let inside = dir
        .canonicalize()
        .with_context(|| format!("cannot read {}", dir.display()))?;
    let refused = Arc::new(Mutex::new(Vec::new()));

    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .follow_links(true)
        .filter_entry({
            let refused = Arc::clone(&refused);
            move |entry| {
                if entry.file_name() == ".git" {
                    return false;
                }
                if !entry.path_is_symlink() || stays_inside(entry.path(), &inside) {
                    return true;
                }
                if let Ok(mut refused) = refused.lock() {
                    refused.push(entry.path().to_path_buf());
                }
                false
            }
        })
        .build();
    let mut skipped = Vec::new();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => match unfollowable_link(&e) {
                Some(link) => {
                    skipped.push(link);
                    continue;
                }
                None => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
            },
        };
        let path = entry.path();
        let relative = path.strip_prefix(dir).unwrap_or(path);
        let name = archive_name(root, relative);
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            builder
                .append_dir(&name, path)
                .with_context(|| format!("cannot archive {}", path.display()))?;
        } else if kind.is_file() {
            builder
                .append_path_with_name(path, &name)
                .with_context(|| format!("cannot archive {}", path.display()))?;
        }
    }
    builder
        .into_inner()
        .and_then(|file| file.sync_all())
        .with_context(|| format!("cannot write {}", out.display()))?;

    skipped.extend(refused.lock().map(|r| r.clone()).unwrap_or_default());
    skipped.sort();
    Ok(skipped)
}

/// Whether a link leads somewhere inside the project and outside its `.git`.
fn stays_inside(link: &Path, inside: &Path) -> bool {
    link.canonicalize().is_ok_and(|target| {
        target
            .strip_prefix(inside)
            .is_ok_and(|rest| !rest.components().any(|part| part.as_os_str() == ".git"))
    })
}

/// The link at fault when the walker stopped at one it cannot follow: a link
/// back into a directory it is already inside, or one whose target is gone.
/// Either is left out of the archive rather than failing the whole sync.
fn unfollowable_link(error: &ignore::Error) -> Option<PathBuf> {
    fn find(error: &ignore::Error, path: Option<&Path>) -> Option<PathBuf> {
        match error {
            ignore::Error::Loop { child, .. } => Some(child.clone()),
            ignore::Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => {
                path.filter(|path| path.is_symlink()).map(Path::to_path_buf)
            }
            ignore::Error::WithPath { path, err } => find(err, Some(path)),
            ignore::Error::WithDepth { err, .. } | ignore::Error::WithLineNumber { err, .. } => {
                find(err, path)
            }
            _ => None,
        }
    }
    find(error, None)
}

/// `/`-separated whatever the host, because that is what tar means.
fn archive_name(root: &str, relative: &Path) -> PathBuf {
    let mut name = root.to_string();
    for part in relative.components() {
        name.push('/');
        name.push_str(&part.as_os_str().to_string_lossy());
    }
    PathBuf::from(name)
}

/// The command that unpacks the archive in the guest's home and removes it.
///
/// A failed unpack has to fail the sync, or the command after it runs against
/// whatever an earlier sync left behind.
pub fn unpack_command(guest: Guest) -> Vec<String> {
    match guest {
        Guest::Unix => vec![
            "sh".into(),
            "-c".into(),
            format!("tar -xf {ARCHIVE} && rm -f {ARCHIVE}"),
        ],
        Guest::Windows => vec![format!(
            "tar -xf {ARCHIVE}; if ($LASTEXITCODE) {{ exit $LASTEXITCODE }}; Remove-Item {ARCHIVE}"
        )],
    }
}

/// Copy `dir` into the guest and return where it landed.
pub fn sync(
    ssh_binary: &Path,
    scp_binary: &Path,
    target: &ssh::SshTarget,
    guest: Guest,
    dir: &Path,
) -> Result<String> {
    let root = guest_dir(dir)?;
    let scratch = tempfile::tempdir().context("cannot create a temporary directory")?;
    let local = scratch.path().join(ARCHIVE);
    for link in archive(dir, &root, &local)? {
        eprintln!(
            "vitro: not sending {}: it links to something outside {}",
            link.display(),
            dir.display()
        );
    }

    let status = std::process::Command::new(scp_binary)
        .args(ssh::scp_args(target, &local, ARCHIVE))
        .status()
        .with_context(|| format!("cannot run {}", scp_binary.display()))?;
    if !status.success() {
        bail!("could not copy {} into the guest", dir.display());
    }

    let code = ssh::run_passthrough(
        ssh_binary,
        target,
        &ssh::SshOptions {
            command: unpack_command(guest),
            quoting: guest.into(),
            ..ssh::SshOptions::default()
        },
    )?;
    if code != 0 {
        bail!(
            "could not unpack {} in the guest (exit code {code})",
            dir.display()
        );
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn entries(archive: &Path) -> Vec<String> {
        let mut names: Vec<String> = tar::Archive::new(File::open(archive).unwrap())
            .entries()
            .unwrap()
            .map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned())
            .map(|name| name.trim_end_matches('/').to_string())
            .collect();
        names.sort();
        names
    }

    fn project() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        for (path, text) in [
            ("src/main.rs", "fn main() {}"),
            (".cargo/config.toml", "[build]"),
            (".gitignore", "/target\n*.log\n"),
            ("target/debug/app", "binary"),
            ("build.log", "noise"),
            (".git/HEAD", "ref: refs/heads/main"),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        temp
    }

    #[test]
    fn the_archive_holds_what_gitignore_keeps_under_one_root() {
        let project = project();
        let out = tempfile::tempdir().unwrap();
        let archive_path = out.path().join("a.tar");

        archive(project.path(), "proj", &archive_path).unwrap();

        assert_eq!(
            entries(&archive_path),
            [
                "proj",
                "proj/.cargo",
                "proj/.cargo/config.toml",
                "proj/.gitignore",
                "proj/src",
                "proj/src/main.rs",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_sent_as_what_it_points_at_only_when_that_stays_inside_the_project() {
        use std::os::unix::fs::symlink;

        let project = project();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("id_ed25519"), "secret").unwrap();
        let root = project.path();
        symlink(root.join("src/main.rs"), root.join("main-link.rs")).unwrap();
        symlink(outside.path().join("id_ed25519"), root.join("key")).unwrap();
        symlink(root.join("src"), root.join("src-link")).unwrap();
        symlink(root, root.join("src/up")).unwrap();
        let out = tempfile::tempdir().unwrap();
        let archive_path = out.path().join("a.tar");

        let mut skipped = archive(root, "proj", &archive_path).unwrap();

        let names = entries(&archive_path);
        for wanted in ["proj/main-link.rs", "proj/src-link/main.rs"] {
            assert!(names.contains(&wanted.to_string()), "{wanted}: {names:?}");
        }
        assert!(
            !names.iter().any(|n| n.starts_with("proj/key")),
            "{names:?}"
        );
        assert!(!names.iter().any(|n| n.ends_with("/up")), "{names:?}");
        skipped.sort();
        assert_eq!(
            skipped,
            [
                root.join("key"),
                root.join("src/up"),
                root.join("src-link/up")
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_directory_meets_gitignore_as_a_directory_and_never_reaches_into_git() {
        use std::os::unix::fs::symlink;

        let project = project();
        let root = project.path();
        fs::write(root.join(".gitignore"), "/target\n/cache/\n").unwrap();
        symlink(root.join("src"), root.join("cache")).unwrap();
        symlink(root.join(".git"), root.join("history")).unwrap();
        let out = tempfile::tempdir().unwrap();
        let archive_path = out.path().join("a.tar");

        let skipped = archive(root, "proj", &archive_path).unwrap();

        let names = entries(&archive_path);
        assert!(
            !names.iter().any(|n| n.starts_with("proj/cache")),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|n| n.starts_with("proj/history")),
            "{names:?}"
        );
        assert_eq!(skipped, [root.join("history")]);
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_is_left_out_rather_than_failing_the_sync() {
        let project = project();
        let root = project.path();
        std::os::unix::fs::symlink(root.join("gone"), root.join("dangling")).unwrap();
        let out = tempfile::tempdir().unwrap();

        let skipped = archive(root, "proj", &out.path().join("a.tar")).unwrap();

        assert_eq!(skipped, [root.join("dangling")]);
    }

    #[test]
    fn each_file_keeps_its_own_modification_time() {
        // A fixed date in the past would make sources look older than the
        // outputs of an earlier build in the guest, and the build would skip
        // them.
        let project = project();
        let edited = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_790_000_000);
        File::options()
            .write(true)
            .open(project.path().join("src/main.rs"))
            .unwrap()
            .set_modified(edited)
            .unwrap();
        let out = tempfile::tempdir().unwrap();
        let archive_path = out.path().join("a.tar");

        archive(project.path(), "proj", &archive_path).unwrap();

        let mtime = tar::Archive::new(File::open(&archive_path).unwrap())
            .entries()
            .unwrap()
            .map(|e| e.unwrap())
            .find(|e| e.path().unwrap().ends_with("src/main.rs"))
            .unwrap()
            .header()
            .mtime()
            .unwrap();
        assert_eq!(mtime, 1_790_000_000);
    }

    #[test]
    fn a_synced_directory_lands_under_its_own_name() {
        let project = project();
        let expected = project
            .path()
            .canonicalize()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();

        assert_eq!(guest_dir(project.path()).unwrap(), expected);
        assert_eq!(guest_dir(&project.path().join("src/..")).unwrap(), expected);
    }

    #[test]
    fn a_file_or_a_missing_path_cannot_be_synced() {
        let project = project();

        assert!(guest_dir(&project.path().join("src/main.rs")).is_err());
        assert!(guest_dir(&project.path().join("nope")).is_err());
    }

    #[test]
    fn a_unix_guest_unpacks_and_cleans_up_only_if_unpacking_worked() {
        assert_eq!(
            unpack_command(Guest::Unix),
            [
                "sh",
                "-c",
                "tar -xf .vitro-sync.tar && rm -f .vitro-sync.tar"
            ]
        );
    }

    #[test]
    fn a_windows_guest_stops_on_a_failed_unpack_because_powershell_would_carry_on() {
        let command = unpack_command(Guest::Windows).join(" ");

        assert!(command.starts_with("tar -xf .vitro-sync.tar;"), "{command}");
        assert!(command.contains("exit $LASTEXITCODE"), "{command}");
    }
}
