//! `vitro.toml`: what a project is checked on, and with what command.
//!
//! The machine-wide `config.toml` says what images exist. This file lives in a
//! repository and says which of them the project cares about, so everyone who
//! checks it out checks it the same way.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::{ssh, ByteSize};

pub const PROJECT_FILE: &str = "vitro.toml";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The directory holding `vitro.toml`.
    pub dir: PathBuf,
    /// The images `vitro test` runs on when none is named, in order.
    pub images: Vec<String>,
    pub command: Vec<String>,
    /// The directory sent to the guest before the command, or `None` when the
    /// command needs nothing from the host.
    pub sync: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cpus: Option<u32>,
    pub memory: Option<ByteSize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    #[serde(default)]
    images: Vec<String>,
    command: Vec<String>,
    sync: Option<SyncSetting>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cpus: Option<u32>,
    memory: Option<ByteSize>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SyncSetting {
    Enabled(bool),
    Dir(String),
}

/// The nearest `vitro.toml` at or above `start`, the way Cargo finds its
/// manifest, so the command works from any subdirectory of the project.
pub fn find_project(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .map(|dir| dir.join(PROJECT_FILE))
        .find(|candidate| candidate.is_file())
}

impl Project {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let dir = path.parent().unwrap_or(Path::new("."));
        Self::parse(&text, dir).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str, dir: &Path) -> Result<Self> {
        let file: ProjectFile = toml::from_str(text)?;
        if file.command.is_empty() {
            bail!("`command` is empty; it is the command `vitro test` runs, one word per element");
        }
        if file.cpus == Some(0) {
            bail!("`cpus` has to be at least 1");
        }
        let sync = match file.sync {
            None | Some(SyncSetting::Enabled(true)) => Some(dir.to_path_buf()),
            Some(SyncSetting::Enabled(false)) => None,
            Some(SyncSetting::Dir(relative)) => Some(dir.join(relative)),
        };
        let env = file
            .env
            .into_iter()
            .map(|(name, value)| ssh::parse_env(&format!("{name}={value}")))
            .collect::<Result<_>>()
            .context("in [env]")?;
        Ok(Self {
            dir: dir.to_path_buf(),
            images: file.images,
            command: file.command,
            sync,
            env,
            cpus: file.cpus,
            memory: file.memory,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Project> {
        Project::parse(text, Path::new("/work/app"))
    }

    #[test]
    fn a_full_file_reads_as_written() {
        let project = parse(
            r#"
            images = ["windows", "macos"]
            command = ["cargo", "test", "--workspace"]
            sync = "crates"
            cpus = 8
            memory = "16G"

            [env]
            RUST_BACKTRACE = "1"
            "#,
        )
        .unwrap();

        assert_eq!(project.images, ["windows", "macos"]);
        assert_eq!(project.command, ["cargo", "test", "--workspace"]);
        assert_eq!(project.sync, Some(PathBuf::from("/work/app/crates")));
        assert_eq!(project.cpus, Some(8));
        assert_eq!(project.memory, Some("16G".parse().unwrap()));
        assert_eq!(project.env, [("RUST_BACKTRACE".into(), "1".into())]);
    }

    #[test]
    fn a_command_alone_is_enough_and_sends_the_whole_project() {
        let project = parse(r#"command = ["make", "check"]"#).unwrap();

        assert!(project.images.is_empty());
        assert_eq!(project.sync, Some(PathBuf::from("/work/app")));
        assert_eq!(project.cpus, None);
    }

    #[test]
    fn sync_false_sends_nothing() {
        let project = parse("command = [\"ver\"]\nsync = false").unwrap();

        assert_eq!(project.sync, None);
    }

    #[test]
    fn a_missing_or_empty_command_is_refused() {
        assert!(parse(r#"images = ["linux"]"#).is_err());
        let err = format!("{:#}", parse("command = []").unwrap_err());
        assert!(err.contains("command"), "{err}");
    }

    #[test]
    fn a_misspelt_key_is_named_rather_than_ignored() {
        let err = format!(
            "{:#}",
            parse("command = [\"x\"]\nimage = \"linux\"").unwrap_err()
        );

        assert!(err.contains("image"), "{err}");
    }

    #[test]
    fn a_variable_name_no_shell_could_assign_is_refused() {
        let err = format!(
            "{:#}",
            parse("command = [\"x\"]\n[env]\n\"A-B\" = \"1\"").unwrap_err()
        );

        assert!(err.contains("A-B"), "{err}");
    }

    #[test]
    fn zero_cpus_is_refused() {
        assert!(parse("command = [\"x\"]\ncpus = 0").is_err());
    }

    #[test]
    fn the_nearest_project_file_above_is_found() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        std::fs::write(root.join(PROJECT_FILE), "command = [\"x\"]").unwrap();
        let deep = root.join("src/nested");
        std::fs::create_dir_all(&deep).unwrap();

        assert_eq!(find_project(&deep), Some(root.join(PROJECT_FILE)));
    }

    #[test]
    fn no_project_file_anywhere_finds_nothing() {
        let temp = tempfile::tempdir().unwrap();

        // The temporary directory's own ancestors are outside the test's
        // control, so only a miss below them can be asserted.
        let found = find_project(temp.path());
        assert!(found.is_none_or(|path| !path.starts_with(temp.path())));
    }
}
