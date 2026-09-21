//! `vitro keygen` — make the key pair every guest is authorised with.
//!
//! Every guest vitro builds gets this key's public half in its
//! `authorized_keys`, and every `exec`, `ssh` and `scp` presents the private
//! half. It is the one prerequisite a user has to create by hand, so the path
//! is vitro's to know rather than theirs to type — spelling it out was the
//! first instruction in the documentation and the first thing to get wrong.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::{tools, Config, Paths};

/// ed25519 rather than RSA: every OpenSSH that vitro can talk to has had it for
/// a decade, and the key ends up quoted in places where its length shows.
const KEY_TYPE: &str = "ed25519";

pub struct Generated {
    pub path: PathBuf,
    /// The key was already there and left alone.
    pub existed: bool,
}

pub fn keygen(paths: &Paths, config: &Config, force: bool) -> Result<Generated> {
    let path = paths.ssh_key();

    if path.exists() {
        if !force {
            // Not an error: the usual reason to run this twice is not knowing
            // whether it was run once, and replacing the key would lock the
            // user out of every golden image built with the old one.
            return Ok(Generated {
                path,
                existed: true,
            });
        }
        // `ssh-keygen` will not overwrite; it prompts, and a prompt here would
        // hang whatever is driving vitro.
        remove_pair(&path)?;
    }

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)
        .with_context(|| format!("cannot create {}", parent.display()))?;

    let binary = tools::resolve(
        "ssh-keygen",
        config.tools().ssh_keygen.as_deref(),
        "tools.ssh_keygen",
    )?;

    let status = Command::new(&binary.path)
        // Quiet: the fingerprint and randomart are noise inside a guided
        // setup, and vitro reports the path itself.
        .arg("-q")
        .args(["-t", KEY_TYPE])
        // No passphrase: nothing is there to type one, and the key's whole job
        // is to let an unattended `exec` through.
        .args(["-N", ""])
        .args(["-C", "vitro"])
        .arg("-f")
        .arg(&path)
        .status()
        .with_context(|| format!("cannot run {}", binary.path.display()))?;

    if !status.success() {
        bail!("{} exited with {status}", binary.path.display());
    }

    restrict_to_owner(&path);

    Ok(Generated {
        path,
        existed: false,
    })
}

/// Both halves, because `ssh-keygen` refuses to write over either one.
fn remove_pair(path: &Path) -> Result<()> {
    for victim in [path.to_path_buf(), public_half(path)] {
        match std::fs::remove_file(&victim) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(e).with_context(|| format!("cannot replace {}", victim.display()))
            }
        }
    }
    Ok(())
}

/// vitro looks for `<key>.pub` beside the private half, so the name has to be
/// built by appending rather than by replacing an extension.
pub fn public_half(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".pub");
    PathBuf::from(name)
}

/// `ssh` refuses a private key anybody else can read, and the error names
/// permissions rather than the key, which sends people looking in the wrong
/// place.
fn restrict_to_owner(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_public_half_sits_beside_the_private_one() {
        // `with_extension` would turn `id_ed25519` into `id.pub`, and vitro
        // would then report a key pair as incomplete.
        assert_eq!(
            public_half(Path::new("/c/id_ed25519")),
            Path::new("/c/id_ed25519.pub")
        );
        assert_eq!(
            public_half(Path::new("/c/vitro.key")),
            Path::new("/c/vitro.key.pub")
        );
    }

    #[test]
    fn an_existing_key_is_reported_rather_than_replaced() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        std::fs::create_dir_all(paths.ssh_key().parent().unwrap()).unwrap();
        std::fs::write(paths.ssh_key(), "not really a key").unwrap();

        let generated = keygen(&paths, &Config::default(), false).unwrap();

        assert!(generated.existed);
        // Replacing it would lock the user out of every golden image already
        // built with the old one.
        assert_eq!(
            std::fs::read_to_string(paths.ssh_key()).unwrap(),
            "not really a key"
        );
    }
}
