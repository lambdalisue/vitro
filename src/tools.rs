//! Locating the external programs vitro drives.
//!
//! Configuration first, then PATH, then an error that names the program and
//! how to supply it. There is deliberately no search of likely install
//! directories: guessing turns "I have not installed QEMU" into "vitro found
//! the wrong QEMU", which is far harder to diagnose.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// Every external program a command might need, resolved once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tools {
    pub qemu: PathBuf,
    pub qemu_img: PathBuf,
    pub ssh: PathBuf,
    pub scp: PathBuf,
}

/// Where a program was found, so `doctor` can show it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Configured,
    Path,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub origin: Origin,
}

/// Resolve one program. `setting` is the configuration key to name when it
/// cannot be found.
pub fn resolve(name: &str, configured: Option<&Path>, setting: &str) -> Result<Found> {
    if let Some(path) = configured {
        if !path.exists() {
            bail!(
                "{setting} points at {}, which does not exist",
                path.display()
            );
        }
        return Ok(Found {
            path: path.to_path_buf(),
            origin: Origin::Configured,
        });
    }

    match which::which(name) {
        Ok(path) => Ok(Found {
            path,
            origin: Origin::Path,
        }),
        Err(_) => {
            bail!("cannot find {name} on PATH; install it or set `{setting}` in the configuration")
        }
    }
}

impl Tools {
    /// Everything the QEMU backend needs to start and reach a guest.
    pub fn for_qemu(
        qemu_binary: &str,
        qemu: Option<&Path>,
        qemu_img: Option<&Path>,
        ssh: Option<&Path>,
        scp: Option<&Path>,
    ) -> Result<Self> {
        Ok(Self {
            qemu: resolve(qemu_binary, qemu, "tools.qemu")?.path,
            qemu_img: resolve("qemu-img", qemu_img, "tools.qemu_img")?.path,
            ssh: resolve("ssh", ssh, "tools.ssh")?.path,
            scp: resolve("scp", scp, "tools.scp")?.path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_configured_path_wins_over_path() {
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("my-qemu");
        std::fs::write(&fake, "").unwrap();

        let found = resolve("sh", Some(&fake), "tools.qemu").unwrap();

        assert_eq!(found.path, fake);
        assert_eq!(found.origin, Origin::Configured);
    }

    #[test]
    fn a_configured_path_that_does_not_exist_names_the_setting_and_the_path() {
        let err = resolve("sh", Some(Path::new("/nope/qemu")), "tools.qemu")
            .unwrap_err()
            .to_string();

        assert!(err.contains("tools.qemu"), "{err}");
        assert!(err.contains("/nope/qemu"), "{err}");
    }

    #[test]
    fn falling_back_to_path_records_where_it_came_from() {
        // A program every host has, because what is under test is the lookup
        // rather than the program. `sh` is not one of those on Windows.
        let program = if cfg!(windows) { "cmd" } else { "sh" };

        let found = resolve(program, None, "tools.shell").unwrap();

        assert_eq!(found.origin, Origin::Path);
        assert!(found.path.is_absolute(), "{:?}", found.path);
    }

    #[test]
    fn a_program_that_is_nowhere_says_how_to_supply_it() {
        let err = resolve("vitro-definitely-not-a-real-program", None, "tools.qemu")
            .unwrap_err()
            .to_string();

        assert!(err.contains("vitro-definitely-not-a-real-program"), "{err}");
        assert!(err.contains("tools.qemu"), "{err}");
    }
}
