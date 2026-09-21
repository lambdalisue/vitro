//! Where vitro keeps its configuration, images and running state.
//!
//! The layout is XDG-shaped on every platform rather than whatever the host
//! calls native. The `directories` crate's macOS defaults cannot express it:
//! there, `config_dir()` and `data_dir()` are the same directory and
//! `state_dir()` does not exist at all, so configuration, golden images and
//! per-VM state would land in one pile.
//!
//! The home directory comes from the environment, not from the platform's user
//! database. `directories` asks macOS directly and so ignores `$HOME`, which
//! makes the tool impossible to point at a different home — for a test, for a
//! scripted run, or for a user who keeps their dotfiles elsewhere.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

/// Every directory vitro reads or writes, resolved once at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    home: PathBuf,
    config_dir: PathBuf,
    data_dir: PathBuf,
    cache_dir: PathBuf,
    state_dir: PathBuf,
}

impl Paths {
    /// Resolve from the real environment.
    pub fn resolve() -> Result<Self> {
        let env: HashMap<OsString, OsString> = std::env::vars_os().collect();
        let home = home_dir(&env)?;
        Ok(Self::from_env(&env, &home))
    }

    /// The resolution rule itself, with the environment passed in so it can be
    /// tested without touching the process's own.
    pub fn from_env(env: &HashMap<OsString, OsString>, home: &Path) -> Self {
        let base = |var: &str, fallback: &str| -> PathBuf {
            match env.get(&OsString::from(var)) {
                // An XDG variable is only honoured when it is an absolute
                // path; the specification says a relative one is invalid, and
                // silently resolving it against the cwd would put state
                // somewhere different on every invocation.
                Some(value) if Path::new(value).is_absolute() => PathBuf::from(value),
                _ => home.join(fallback),
            }
        };

        Self {
            home: home.to_path_buf(),
            config_dir: base("XDG_CONFIG_HOME", ".config").join("vitro"),
            data_dir: base("XDG_DATA_HOME", ".local/share").join("vitro"),
            cache_dir: base("XDG_CACHE_HOME", ".cache").join("vitro"),
            state_dir: base("XDG_STATE_HOME", ".local/state").join("vitro"),
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    /// The tool's own SSH key, generated on first use.
    pub fn ssh_key(&self) -> PathBuf {
        self.config_dir.join("id_ed25519")
    }

    /// The include file `ssh-config --write` owns. The user's own
    /// `~/.ssh/config` is never edited.
    pub fn ssh_config(&self) -> PathBuf {
        self.config_dir.join("ssh_config")
    }

    /// Golden images. Read-only once built; the one directory worth backing up.
    pub fn golden_dir(&self) -> PathBuf {
        self.data_dir.join("golden")
    }

    /// Downloaded installation media. Safe to delete at any time.
    pub fn media_cache_dir(&self) -> PathBuf {
        self.cache_dir.join("media")
    }

    /// Installation media the user supplies, for the files vitro is not allowed
    /// to fetch.
    ///
    /// Data rather than cache, and a place vitro names rather than one it goes
    /// looking for: a seven-gigabyte ISO that cannot be downloaded again
    /// unattended does not belong somewhere a cleaner may remove, and "put it
    /// here" is a shorter instruction than "tell me where you put it".
    pub fn media_dir(&self) -> PathBuf {
        self.data_dir.join("media")
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// One directory per running VM, holding its record, overlay and logs.
    pub fn vms_dir(&self) -> PathBuf {
        self.state_dir.join("vms")
    }

    pub fn vm_dir(&self, name: impl AsRef<str>) -> PathBuf {
        self.vms_dir().join(name.as_ref())
    }

    /// The home directory `~` in the configuration expands to. The same one
    /// the rest of this layout was derived from, so a `Paths` built for a
    /// different root stays self-consistent.
    pub fn home(&self) -> &Path {
        &self.home
    }

    /// Scratch space for a build in progress, kept apart from finished goldens
    /// so a failed build cannot leave a half-written image where `run` looks.
    pub fn builds_dir(&self) -> PathBuf {
        self.state_dir.join("builds")
    }
}

/// `$HOME`, or `%USERPROFILE%` on Windows.
pub fn home_dir(env: &HashMap<OsString, OsString>) -> Result<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    match env.get(&OsString::from(key)) {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => bail!("{key} is not set, so vitro cannot tell where its files belong"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> HashMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(k, v)| (OsString::from(*k), OsString::from(*v)))
            .collect()
    }

    #[cfg(not(windows))]
    #[test]
    fn the_home_directory_comes_from_the_environment() {
        // Asking the platform instead would ignore a HOME the caller set, and
        // make every path here impossible to redirect.
        assert_eq!(
            home_dir(&env(&[("HOME", "/home/dev")])).unwrap(),
            Path::new("/home/dev")
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn an_unset_or_empty_home_is_an_error_that_names_the_variable() {
        for pairs in [vec![], vec![("HOME", "")]] {
            let err = home_dir(&env(&pairs)).unwrap_err().to_string();
            assert!(err.contains("HOME"), "{err}");
        }
    }

    #[test]
    fn falls_back_to_xdg_shaped_paths_under_home() {
        let paths = Paths::from_env(&env(&[]), Path::new("/home/dev"));

        assert_eq!(
            paths.config_file(),
            Path::new("/home/dev/.config/vitro/config.toml")
        );
        assert_eq!(
            paths.golden_dir(),
            Path::new("/home/dev/.local/share/vitro/golden")
        );
        assert_eq!(
            paths.media_cache_dir(),
            Path::new("/home/dev/.cache/vitro/media")
        );
        assert_eq!(
            paths.vms_dir(),
            Path::new("/home/dev/.local/state/vitro/vms")
        );
    }

    /// An absolute path on whichever host the test is running on.
    ///
    /// `/cfg` is not absolute on Windows — a path needs a drive or a UNC prefix
    /// there — so a Unix-shaped fixture exercises the relative branch instead of
    /// the one it means to, and fails for a reason unrelated to the rule under
    /// test.
    fn absolute(name: &str) -> String {
        if cfg!(windows) {
            format!(r"C:\{name}")
        } else {
            format!("/{name}")
        }
    }

    #[test]
    fn honours_the_xdg_variables_when_set() {
        let (cfg, data, cache, state) = (
            absolute("cfg"),
            absolute("data"),
            absolute("cache"),
            absolute("state"),
        );
        let paths = Paths::from_env(
            &env(&[
                ("XDG_CONFIG_HOME", &cfg),
                ("XDG_DATA_HOME", &data),
                ("XDG_CACHE_HOME", &cache),
                ("XDG_STATE_HOME", &state),
            ]),
            Path::new(&absolute("home/dev")),
        );

        assert_eq!(paths.config_dir(), Path::new(&cfg).join("vitro"));
        assert_eq!(
            paths.golden_dir(),
            Path::new(&data).join("vitro").join("golden")
        );
        assert_eq!(
            paths.media_cache_dir(),
            Path::new(&cache).join("vitro").join("media")
        );
        assert_eq!(paths.state_dir(), Path::new(&state).join("vitro"));
    }

    #[test]
    fn ignores_a_relative_xdg_variable() {
        // Honouring it would move the state directory with the cwd.
        let paths = Paths::from_env(
            &env(&[("XDG_STATE_HOME", "relative/state")]),
            Path::new("/home/dev"),
        );

        assert_eq!(paths.state_dir(), Path::new("/home/dev/.local/state/vitro"));
    }

    #[test]
    fn a_vm_gets_its_own_directory_under_the_state_dir() {
        let paths = Paths::from_env(&env(&[]), Path::new("/home/dev"));

        assert_eq!(
            paths.vm_dir("linux-7f3a2c"),
            Path::new("/home/dev/.local/state/vitro/vms/linux-7f3a2c")
        );
    }
}
