//! Putting a host directory into a guest before running something there.
//!
//! The tree goes as one tar archive rather than as a recursive copy. A copy
//! would carry build output and `.git` along — gigabytes for a Rust project —
//! and has no way to leave them out, while an archive is built from what
//! `.gitignore` keeps. The guest unpacks it with the `tar` that Windows 10 and
//! later, macOS and every Linux image ship.

use std::fs::File;
use std::path::{Path, PathBuf};

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
pub fn archive(dir: &Path, root: &str, out: &Path) -> Result<()> {
    let file = File::create(out).with_context(|| format!("cannot create {}", out.display()))?;
    let mut builder = tar::Builder::new(file);
    builder.mode(tar::HeaderMode::Deterministic);

    let walker = ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();
    for entry in walker {
        let entry = entry.with_context(|| format!("cannot read {}", dir.display()))?;
        let path = entry.path();
        let relative = path.strip_prefix(dir).unwrap_or(path);
        let name = archive_name(root, relative);
        let kind = entry.file_type();
        if kind.is_some_and(|k| k.is_dir()) {
            builder
                .append_dir(&name, path)
                .with_context(|| format!("cannot archive {}", path.display()))?;
        } else if kind.is_some_and(|k| k.is_file()) {
            builder
                .append_path_with_name(path, &name)
                .with_context(|| format!("cannot archive {}", path.display()))?;
        }
    }
    builder
        .into_inner()
        .and_then(|file| file.sync_all())
        .with_context(|| format!("cannot write {}", out.display()))
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
    archive(dir, &root, &local)?;

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
