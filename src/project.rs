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
    /// The command every image runs unless it has its own.
    command: Option<Vec<String>>,
    /// What differs for one image, by its key.
    per_image: BTreeMap<String, ImageSettings>,
    /// The directory sent to the guest before the command, or `None` when the
    /// command needs nothing from the host.
    pub sync: Option<PathBuf>,
    pub env: Vec<(String, String)>,
    pub cpus: Option<u32>,
    pub memory: Option<ByteSize>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ImageSettings {
    command: Option<Vec<String>>,
    env: Vec<(String, String)>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectFile {
    #[serde(default)]
    images: Vec<String>,
    command: Option<Vec<String>>,
    sync: Option<SyncSetting>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    cpus: Option<u32>,
    memory: Option<ByteSize>,
    #[serde(default)]
    image: BTreeMap<String, ImageSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageSection {
    command: Option<Vec<String>>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn variables(env: BTreeMap<String, String>) -> Result<Vec<(String, String)>> {
    env.into_iter()
        .map(|(name, value)| ssh::parse_env(&format!("{name}={value}")))
        .collect()
}

fn non_empty(command: Option<Vec<String>>, place: &str) -> Result<Option<Vec<String>>> {
    match command {
        Some(words) if words.is_empty() => bail!(
            "`command` in {place} is empty; it is the command `vitro test` runs, one word per element"
        ),
        other => Ok(other),
    }
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
        let command = non_empty(file.command, "the file")?;
        let mut per_image = BTreeMap::new();
        for (key, section) in file.image {
            let place = format!("[image.{key}]");
            let settings = ImageSettings {
                command: non_empty(section.command, &place)?,
                env: variables(section.env).with_context(|| format!("in {place}"))?,
            };
            per_image.insert(key, settings);
        }
        // Said here rather than when `vitro test` reaches the image, which may
        // be after the images before it have run for an hour.
        let has_command = |image: &str| {
            command.is_some() || per_image.get(image).is_some_and(|s| s.command.is_some())
        };
        if let Some(image) = file.images.iter().find(|image| !has_command(image)) {
            bail!("image {image:?} has no command: set `command`, or `command` in [image.{image}]");
        }
        if command.is_none() && per_image.values().all(|s| s.command.is_none()) {
            bail!(
                "there is no `command`; it is the command `vitro test` runs, one word per element"
            );
        }
        if file.cpus == Some(0) {
            bail!("`cpus` has to be at least 1");
        }
        let sync = match file.sync {
            None | Some(SyncSetting::Enabled(true)) => Some(dir.to_path_buf()),
            Some(SyncSetting::Enabled(false)) => None,
            Some(SyncSetting::Dir(relative)) => Some(dir.join(relative)),
        };
        let env = variables(file.env).context("in [env]")?;
        Ok(Self {
            dir: dir.to_path_buf(),
            images: file.images,
            command,
            per_image,
            sync,
            env,
            cpus: file.cpus,
            memory: file.memory,
        })
    }

    /// The command `image` runs: its own, or the shared one.
    pub fn command_for(&self, image: &str) -> Option<&[String]> {
        self.per_image
            .get(image)
            .and_then(|settings| settings.command.as_deref())
            .or(self.command.as_deref())
    }

    /// The shared variables with `image`'s own laid over them, sorted by name.
    pub fn env_for(&self, image: &str) -> Vec<(String, String)> {
        let mut env: BTreeMap<String, String> = self.env.iter().cloned().collect();
        if let Some(settings) = self.per_image.get(image) {
            env.extend(settings.env.iter().cloned());
        }
        env.into_iter().collect()
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
        assert_eq!(
            project.command_for("windows").unwrap(),
            ["cargo", "test", "--workspace"]
        );
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
        assert!(parse("").is_err());
        let err = format!("{:#}", parse("command = []").unwrap_err());
        assert!(err.contains("command"), "{err}");
    }

    #[test]
    fn an_image_can_replace_the_command_and_add_to_the_variables() {
        let project = parse(
            r#"
            images = ["windows", "linux"]
            command = ["cargo", "test"]

            [env]
            RUST_BACKTRACE = "1"
            LEVEL = "info"

            [image.windows]
            command = ["cargo", "test", "--features", "tsf"]
            env = { LEVEL = "debug", WINDOWS_ONLY = "yes" }
            "#,
        )
        .unwrap();

        assert_eq!(
            project.command_for("windows").unwrap(),
            ["cargo", "test", "--features", "tsf"]
        );
        assert_eq!(project.command_for("linux").unwrap(), ["cargo", "test"]);
        assert_eq!(
            project.env_for("windows"),
            [
                ("LEVEL".to_string(), "debug".to_string()),
                ("RUST_BACKTRACE".into(), "1".into()),
                ("WINDOWS_ONLY".into(), "yes".into()),
            ]
        );
        assert_eq!(
            project.env_for("linux"),
            [
                ("LEVEL".to_string(), "info".to_string()),
                ("RUST_BACKTRACE".into(), "1".into()),
            ]
        );
    }

    #[test]
    fn the_shared_command_can_be_left_out_when_every_listed_image_has_its_own() {
        let project = parse(
            r#"
            images = ["windows", "linux"]
            [image.windows]
            command = ["pwsh", "-File", "test.ps1"]
            [image.linux]
            command = ["make", "check"]
            "#,
        )
        .unwrap();

        assert_eq!(project.command_for("linux").unwrap(), ["make", "check"]);
        assert_eq!(project.command_for("macos"), None);
    }

    #[test]
    fn a_listed_image_left_without_a_command_is_refused() {
        let err = format!(
            "{:#}",
            parse(
                r#"
                images = ["windows", "linux"]
                [image.windows]
                command = ["x"]
                "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("linux"), "{err}");
    }

    #[test]
    fn a_misspelt_key_is_named_rather_than_ignored() {
        let err = format!(
            "{:#}",
            parse("command = [\"x\"]\nimgaes = [\"linux\"]").unwrap_err()
        );
        assert!(err.contains("imgaes"), "{err}");

        let err = format!(
            "{:#}",
            parse("command = [\"x\"]\n[image.linux]\ncomand = [\"y\"]").unwrap_err()
        );
        assert!(err.contains("comand"), "{err}");
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
