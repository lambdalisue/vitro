//! The TOML configuration, and the merge of `[defaults]` into `[images.*]`.
//!
//! Every error raised here names the image it came from and, where a value was
//! rejected, shows the value. Configuration mistakes are the first thing a new
//! user hits, and a parse error that only says "invalid type" costs more than
//! the code that avoids it.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::units::{ByteSize, Resolution};

/// Defaults that apply when neither `[defaults]` nor the image sets a value.
/// The boot timeout is deliberately far above the measured 14–15 s a Linux
/// guest takes: the value only matters when something is wrong, and a
/// too-eager timeout turns a slow host into a mysterious failure.
const DEFAULT_CPUS: u32 = 4;
const DEFAULT_MEMORY: ByteSize = ByteSize::from_gib(8);
const DEFAULT_BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// A Windows guest takes minutes where a Linux one takes seconds, and its first
/// boot from a fresh golden image spends longer still working out what hardware
/// it is on now. Timing out on a guest that was going to answer is the most
/// annoying way for this to fail, so the ceiling is generous.
const WINDOWS_BOOT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A guest lume runs has two waits in front of it, not one, and the ceiling
/// covers both: the address arrives the better part of a minute after the VM
/// starts, and sshd is listening some time after that again. A Linux guest's
/// ceiling is sized for neither.
const LUME_BOOT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const DEFAULT_BUILD_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);
const DEFAULT_RESOLUTION: Resolution = Resolution::new(1280, 800);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    Qemu,
    Lume,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Qemu => write!(f, "qemu"),
            Backend::Lume => write!(f, "lume"),
        }
    }
}

/// Which family of guest is inside the image.
///
/// vitro needs this for the handful of places where the guest answers to
/// something different: how it is asked to shut down, and what a command runs
/// under. Everything else is the same for both.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Guest {
    #[default]
    Unix,
    Windows,
}

impl fmt::Display for Guest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Guest::Unix => write!(f, "unix"),
            Guest::Windows => write!(f, "windows"),
        }
    }
}

/// What a golden image is depends on who runs it: a file for QEMU, a name for
/// Lume, which keeps its VMs in its own store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Golden {
    Image(PathBuf),
    VmName(String),
}

impl fmt::Display for Golden {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Golden::Image(path) => write!(f, "{}", path.display()),
            Golden::VmName(name) => write!(f, "{name}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildKind {
    /// A ready-made cloud image: download, resize, let cloud-init configure it.
    CloudImage,
    /// An installer ISO driven by an answer file.
    UnattendedInstall,
    /// macOS from an Apple restore image, via Lume.
    LumeIpsw,
}

impl fmt::Display for BuildKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            BuildKind::CloudImage => "cloud-image",
            BuildKind::UnattendedInstall => "unattended-install",
            BuildKind::LumeIpsw => "lume-ipsw",
        };
        f.write_str(text)
    }
}

/// Where installation media comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Ask the backend what the current release is. Only Lume offers this.
    Latest,
    Url(String),
    LocalPath(PathBuf),
}

/// One `[images.<key>]` after `[defaults]` has been merged in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub key: String,
    pub backend: Backend,
    pub guest: Guest,
    pub golden: Golden,
    pub firmware: Option<PathBuf>,
    pub cpus: u32,
    pub memory: ByteSize,
    pub ssh_user: String,
    pub ssh_key: PathBuf,
    pub boot_timeout: Duration,
    pub resolution: Resolution,
    pub extra_args: Vec<String>,
    pub build: Option<Build>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub kind: BuildKind,
    pub source: Source,
    pub sha256: Option<String>,
    pub disk_size: Option<ByteSize>,
    pub provision: Option<PathBuf>,
    /// Replaces the built-in answer file wholesale. Windows releases differ
    /// enough that a template which works everywhere is not worth chasing.
    pub unattend: Option<PathBuf>,
    /// The virtio-win ISO the ARM64 drivers are lifted from.
    pub virtio_iso: Option<PathBuf>,
    /// Lume's own first-boot preset name, such as `tahoe`.
    pub unattended: Option<String>,
    pub build_timeout: Duration,
}

/// Explicit paths to the external programs vitro drives. Anything left unset
/// is looked up on PATH.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPaths {
    pub qemu: Option<PathBuf>,
    pub qemu_img: Option<PathBuf>,
    pub ssh: Option<PathBuf>,
    pub scp: Option<PathBuf>,
    pub lume: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    images: BTreeMap<String, Image>,
    tools: ToolPaths,
}

impl Config {
    pub fn load(path: &Path, home: &Path, default_ssh_key: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        Self::parse(&text, home, default_ssh_key).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str, home: &Path, default_ssh_key: &Path) -> Result<Self> {
        let file: ConfigFile = toml::from_str(text)?;
        let mut images = BTreeMap::new();
        for (key, section) in file.images {
            let image = resolve(&key, section, &file.defaults, home, default_ssh_key)
                .with_context(|| format!("image {key:?}"))?;
            images.insert(key, image);
        }
        let tools = ToolPaths {
            qemu: file.tools.qemu.map(|p| expand(&p, home)),
            qemu_img: file.tools.qemu_img.map(|p| expand(&p, home)),
            ssh: file.tools.ssh.map(|p| expand(&p, home)),
            scp: file.tools.scp.map(|p| expand(&p, home)),
            lume: file.tools.lume.map(|p| expand(&p, home)),
        };
        Ok(Self { images, tools })
    }

    pub fn tools(&self) -> &ToolPaths {
        &self.tools
    }

    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    pub fn images(&self) -> impl Iterator<Item = &Image> {
        self.images.values()
    }

    /// Naming the images that do exist turns a typo into a one-step fix.
    pub fn image(&self, key: impl AsRef<str>) -> Result<&Image> {
        let key = key.as_ref();
        self.images.get(key).ok_or_else(|| {
            if self.images.is_empty() {
                anyhow!("no image named {key:?}: the configuration defines no images")
            } else {
                let known: Vec<&str> = self.images.keys().map(String::as_str).collect();
                anyhow!(
                    "no image named {key:?}: known images are {}",
                    known.join(", ")
                )
            }
        })
    }

    /// An image without a `[build]` section is a bring-your-own-image entry,
    /// which is a configuration choice rather than a mistake — so say which it
    /// is instead of reporting a missing section.
    pub fn buildable_image(&self, key: impl AsRef<str>) -> Result<(&Image, &Build)> {
        let key = key.as_ref();
        let image = self.image(key)?;
        let build = image.build.as_ref().ok_or_else(|| {
            anyhow!(
                "image {key:?} has no [images.{key}.build] section, so it cannot be built; \
                 point `golden` at an image you already have, or add a build section"
            )
        })?;
        Ok((image, build))
    }
}

// The raw shapes, kept private so the rest of the program only sees resolved
// values. `deny_unknown_fields` turns a typo into a named error instead of a
// setting that silently does nothing.

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    images: BTreeMap<String, ImageSection>,
    #[serde(default)]
    tools: ToolsSection,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsSection {
    qemu: Option<String>,
    qemu_img: Option<String>,
    ssh: Option<String>,
    scp: Option<String>,
    lume: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Defaults {
    cpus: Option<u32>,
    memory: Option<ByteSize>,
    ssh_user: Option<String>,
    ssh_key: Option<String>,
    #[serde(default, with = "humantime_serde::option")]
    boot_timeout: Option<Duration>,
    resolution: Option<Resolution>,
    firmware: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImageSection {
    backend: Option<Backend>,
    guest: Option<Guest>,
    golden: Option<String>,
    firmware: Option<String>,
    cpus: Option<u32>,
    memory: Option<ByteSize>,
    ssh_user: Option<String>,
    ssh_key: Option<String>,
    #[serde(default, with = "humantime_serde::option")]
    boot_timeout: Option<Duration>,
    resolution: Option<Resolution>,
    #[serde(default)]
    extra_args: Vec<String>,
    build: Option<BuildSection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildSection {
    kind: BuildKind,
    source: String,
    sha256: Option<String>,
    disk_size: Option<ByteSize>,
    provision: Option<String>,
    unattend: Option<String>,
    virtio_iso: Option<String>,
    unattended: Option<String>,
    #[serde(default, with = "humantime_serde::option")]
    build_timeout: Option<Duration>,
}

/// Which family of guest an image holds.
///
/// Inferred from the build where there is one, because an unattended install is
/// a Windows install and nothing else vitro builds is. `guest` is for an image
/// that arrives as a ready-made golden, with no build section to infer from —
/// and contradicting what the build says is refused rather than obeyed, because
/// the two disagreeing means one of them stops the guest the wrong way.
fn resolve_guest(given: Option<Guest>, backend: Backend, build: Option<&Build>) -> Result<Guest> {
    let implied = match (backend, build.map(|b| b.kind)) {
        (Backend::Lume, _) => Some(Guest::Unix),
        (_, Some(BuildKind::UnattendedInstall)) => Some(Guest::Windows),
        (_, Some(BuildKind::CloudImage)) => Some(Guest::Unix),
        _ => None,
    };
    match (given, implied) {
        (Some(given), Some(implied)) if given != implied => bail!(
            "`guest` says {given} but this image builds a {implied} guest; \
             remove `guest` or change the build"
        ),
        (Some(given), _) => Ok(given),
        (None, Some(implied)) => Ok(implied),
        (None, None) => Ok(Guest::Unix),
    }
}

fn resolve(
    key: &str,
    section: ImageSection,
    defaults: &Defaults,
    home: &Path,
    default_ssh_key: &Path,
) -> Result<Image> {
    let backend = section.backend.unwrap_or(Backend::Qemu);

    let golden_raw = section
        .golden
        .ok_or_else(|| anyhow!("`golden` is required"))?;
    let golden = match backend {
        Backend::Qemu => Golden::Image(expand(&golden_raw, home)),
        // Lume addresses its VMs by name, so a path here is almost certainly a
        // leftover from a QEMU entry rather than something that would work.
        Backend::Lume => {
            if golden_raw.contains('/') {
                bail!(
                    "`golden` must be a VM name for the lume backend, not a path ({golden_raw:?})"
                );
            }
            Golden::VmName(golden_raw)
        }
    };

    let ssh_user = section
        .ssh_user
        .or_else(|| defaults.ssh_user.clone())
        .ok_or_else(|| anyhow!("`ssh_user` is required, either here or in [defaults]"))?;

    let build = section
        .build
        .map(|b| resolve_build(b, backend, home))
        .transpose()?;
    let guest = resolve_guest(section.guest, backend, build.as_ref())?;

    Ok(Image {
        key: key.to_string(),
        backend,
        guest,
        golden,
        firmware: section
            .firmware
            .or_else(|| defaults.firmware.clone())
            .map(|p| expand(&p, home)),
        cpus: section.cpus.or(defaults.cpus).unwrap_or(DEFAULT_CPUS),
        memory: section.memory.or(defaults.memory).unwrap_or(DEFAULT_MEMORY),
        ssh_user,
        ssh_key: section
            .ssh_key
            .or_else(|| defaults.ssh_key.clone())
            .map(|p| expand(&p, home))
            .unwrap_or_else(|| default_ssh_key.to_path_buf()),
        boot_timeout: section.boot_timeout.or(defaults.boot_timeout).unwrap_or(
            match (backend, guest) {
                (Backend::Lume, _) => LUME_BOOT_TIMEOUT,
                (_, Guest::Windows) => WINDOWS_BOOT_TIMEOUT,
                (_, Guest::Unix) => DEFAULT_BOOT_TIMEOUT,
            },
        ),
        resolution: section
            .resolution
            .or(defaults.resolution)
            .unwrap_or(DEFAULT_RESOLUTION),
        extra_args: section.extra_args,
        build,
    })
}

fn resolve_build(section: BuildSection, backend: Backend, home: &Path) -> Result<Build> {
    let source = classify_source(&section.source, home);

    // A build kind belongs to exactly one backend. Catching the mismatch here
    // means the error arrives before anything is downloaded.
    let expected = match section.kind {
        BuildKind::CloudImage | BuildKind::UnattendedInstall => Backend::Qemu,
        BuildKind::LumeIpsw => Backend::Lume,
    };
    if backend != expected {
        bail!(
            "build kind {} needs backend {expected}, but this image uses {backend}",
            section.kind
        );
    }

    // vitro never downloads a Windows ISO: the licence is the user's to accept,
    // and the distribution page hands out links that cannot be shared.
    if section.kind == BuildKind::UnattendedInstall && !matches!(source, Source::LocalPath(_)) {
        bail!(
            "build kind {} needs `source` to be a local path to installation media \
             you obtained yourself, not {:?}",
            section.kind,
            section.source
        );
    }
    if section.kind != BuildKind::LumeIpsw && source == Source::Latest {
        bail!(
            "`source = \"latest\"` is only meaningful for build kind {}",
            BuildKind::LumeIpsw
        );
    }

    Ok(Build {
        kind: section.kind,
        source,
        sha256: section.sha256,
        disk_size: section.disk_size,
        provision: section.provision.map(|p| expand(&p, home)),
        unattend: section.unattend.map(|p| expand(&p, home)),
        virtio_iso: section.virtio_iso.map(|p| expand(&p, home)),
        unattended: section.unattended,
        build_timeout: section.build_timeout.unwrap_or(DEFAULT_BUILD_TIMEOUT),
    })
}

fn classify_source(raw: &str, home: &Path) -> Source {
    if raw == "latest" {
        Source::Latest
    } else if raw.starts_with("http://") || raw.starts_with("https://") {
        Source::Url(raw.to_string())
    } else {
        Source::LocalPath(expand(raw, home))
    }
}

/// Expand a leading `~`. Nothing else: `$VAR` in a path is the shell's job,
/// and doing it here would make the file mean different things in different
/// environments.
fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_path_buf(),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/home/dev";
    const KEY: &str = "/home/dev/.config/vitro/id_ed25519";

    fn parse(text: &str) -> Result<Config> {
        Config::parse(text, Path::new(HOME), Path::new(KEY))
    }

    fn parse_ok(text: &str) -> Config {
        parse(text).expect("configuration should parse")
    }

    #[test]
    fn an_empty_file_is_a_valid_configuration_with_no_images() {
        let config = parse_ok("");
        assert!(config.is_empty());
    }

    #[test]
    fn an_image_inherits_defaults_and_overrides_them_by_name() {
        let config = parse_ok(
            r#"
            [defaults]
            cpus = 2
            memory = "4G"
            ssh_user = "dev"

            [images.linux]
            golden = "~/images/linux.qcow2"

            [images.windows]
            golden = "/srv/windows.qcow2"
            memory = "12G"
            ssh_user = "admin"
            "#,
        );

        let linux = config.image("linux").unwrap();
        assert_eq!(linux.cpus, 2);
        assert_eq!(linux.memory, ByteSize::from_gib(4));
        assert_eq!(linux.ssh_user, "dev");
        assert_eq!(
            linux.golden,
            Golden::Image(PathBuf::from("/home/dev/images/linux.qcow2"))
        );

        let windows = config.image("windows").unwrap();
        assert_eq!(windows.cpus, 2, "inherited");
        assert_eq!(windows.memory, ByteSize::from_gib(12), "overridden");
        assert_eq!(windows.ssh_user, "admin", "overridden");
    }

    #[test]
    fn built_in_defaults_apply_when_nothing_sets_a_value() {
        let config = parse_ok(
            r#"
            [images.linux]
            golden = "/srv/linux.qcow2"
            ssh_user = "dev"
            "#,
        );

        let linux = config.image("linux").unwrap();
        assert_eq!(linux.cpus, DEFAULT_CPUS);
        assert_eq!(linux.memory, DEFAULT_MEMORY);
        assert_eq!(linux.boot_timeout, DEFAULT_BOOT_TIMEOUT);
        assert_eq!(linux.resolution, DEFAULT_RESOLUTION);
        assert_eq!(linux.backend, Backend::Qemu);
        assert_eq!(linux.ssh_key, Path::new(KEY));
    }

    #[test]
    fn a_missing_image_names_the_ones_that_exist() {
        let config = parse_ok(
            r#"
            [images.linux]
            golden = "/srv/linux.qcow2"
            ssh_user = "dev"

            [images.windows]
            golden = "/srv/windows.qcow2"
            ssh_user = "dev"
            "#,
        );

        let err = config.image("linuz").unwrap_err().to_string();
        assert!(err.contains("linuz"), "{err}");
        assert!(err.contains("linux, windows"), "{err}");
    }

    #[test]
    fn a_missing_image_says_so_plainly_when_there_are_none() {
        let err = parse_ok("").image("linux").unwrap_err().to_string();
        assert!(err.contains("defines no images"), "{err}");
    }

    #[test]
    fn a_misspelled_key_is_reported_rather_than_ignored() {
        let err = parse(
            r#"
            [images.linux]
            golden = "/srv/linux.qcow2"
            ssh_user = "dev"
            momery = "8G"
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("momery"), "{err}");
    }

    #[test]
    fn a_bad_size_names_the_value() {
        let err = parse(
            r#"
            [images.linux]
            golden = "/srv/linux.qcow2"
            ssh_user = "dev"
            memory = "lots"
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("lots"), "{err}");
    }

    #[test]
    fn a_missing_required_value_names_the_image() {
        let err = format!(
            "{:#}",
            parse(
                r#"
                [images.linux]
                ssh_user = "dev"
                "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("linux"), "{err}");
        assert!(err.contains("golden"), "{err}");
    }

    #[test]
    fn ssh_user_may_come_from_either_place_but_must_come_from_one() {
        let err = format!(
            "{:#}",
            parse(
                r#"
                [images.linux]
                golden = "/srv/linux.qcow2"
                "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("ssh_user"), "{err}");
        assert!(err.contains("[defaults]"), "{err}");
    }

    #[test]
    fn durations_are_written_the_way_people_say_them() {
        let config = parse_ok(
            r#"
            [defaults]
            ssh_user = "dev"
            boot_timeout = "3min"

            [images.linux]
            golden = "/srv/linux.qcow2"
            "#,
        );

        assert_eq!(
            config.image("linux").unwrap().boot_timeout,
            Duration::from_secs(180)
        );
    }

    #[test]
    fn a_lume_golden_is_a_vm_name_not_a_path() {
        let config = parse_ok(
            r#"
            [images.macos]
            backend = "lume"
            golden = "tahoe-base"
            ssh_user = "lume"
            "#,
        );

        assert_eq!(
            config.image("macos").unwrap().golden,
            Golden::VmName("tahoe-base".into())
        );
    }

    #[test]
    fn a_path_given_as_a_lume_golden_is_rejected() {
        let err = format!(
            "{:#}",
            parse(
                r#"
                [images.macos]
                backend = "lume"
                golden = "~/images/macos.qcow2"
                ssh_user = "lume"
                "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("VM name"), "{err}");
    }

    #[test]
    fn an_image_without_a_build_section_says_why_it_cannot_be_built() {
        let config = parse_ok(
            r#"
            [images.linux]
            golden = "/srv/linux.qcow2"
            ssh_user = "dev"
            "#,
        );

        let err = config.buildable_image("linux").unwrap_err().to_string();
        assert!(err.contains("[images.linux.build]"), "{err}");
    }

    #[test]
    fn a_cloud_image_build_takes_a_url_and_a_checksum() {
        let config = parse_ok(
            r#"
            [images.linux]
            golden = "/srv/linux.qcow2"
            ssh_user = "dev"

            [images.linux.build]
            kind = "cloud-image"
            source = "https://cloud.example/noble-arm64.img"
            sha256 = "abc123"
            disk_size = "64G"
            provision = "~/provision/linux.sh"
            "#,
        );

        let (_, build) = config.buildable_image("linux").unwrap();
        assert_eq!(build.kind, BuildKind::CloudImage);
        assert_eq!(
            build.source,
            Source::Url("https://cloud.example/noble-arm64.img".into())
        );
        assert_eq!(build.disk_size, Some(ByteSize::from_gib(64)));
        assert_eq!(
            build.provision,
            Some(PathBuf::from("/home/dev/provision/linux.sh"))
        );
        assert_eq!(build.build_timeout, DEFAULT_BUILD_TIMEOUT);
    }

    #[test]
    fn an_unattended_install_refuses_to_download_its_media() {
        let err = format!(
            "{:#}",
            parse(
                r#"
                [images.windows]
                golden = "/srv/windows.qcow2"
                ssh_user = "dev"

                [images.windows.build]
                kind = "unattended-install"
                source = "https://example.com/windows.iso"
                "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("local path"), "{err}");
    }

    #[test]
    fn a_build_kind_must_match_its_backend() {
        let err = format!(
            "{:#}",
            parse(
                r#"
                [images.macos]
                backend = "qemu"
                golden = "/srv/macos.qcow2"
                ssh_user = "lume"

                [images.macos.build]
                kind = "lume-ipsw"
                source = "latest"
                "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("needs backend lume"), "{err}");
    }

    #[test]
    fn latest_is_only_meaningful_for_lume() {
        let err = format!(
            "{:#}",
            parse(
                r#"
                [images.linux]
                golden = "/srv/linux.qcow2"
                ssh_user = "dev"

                [images.linux.build]
                kind = "cloud-image"
                source = "latest"
                "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("latest"), "{err}");
    }

    #[test]
    fn a_windows_image_is_given_longer_to_boot_than_a_linux_one() {
        // A Windows guest takes minutes; timing out on one that was going to
        // answer is the most annoying way for `run` to fail.
        let config = parse_ok(
            r#"
            [images.linux]
            golden = "/srv/linux.qcow2"
            ssh_user = "dev"

            [images.windows]
            golden = "/srv/windows.qcow2"
            ssh_user = "vitro"
            guest = "windows"
            "#,
        );

        assert_eq!(
            config.image("linux").unwrap().boot_timeout,
            DEFAULT_BOOT_TIMEOUT
        );
        assert_eq!(
            config.image("windows").unwrap().boot_timeout,
            WINDOWS_BOOT_TIMEOUT
        );
    }

    #[test]
    fn a_lume_image_is_not_held_to_a_linux_guests_ceiling() {
        // It reads as a Unix guest, but vitro waits for lume to report both an
        // address and sshd, one after the other, inside this one ceiling.
        let config = parse_ok(
            r#"
            [images.macos]
            backend = "lume"
            golden = "vitro-macos"
            ssh_user = "lume"
            "#,
        );

        assert_eq!(
            config.image("macos").unwrap().boot_timeout,
            LUME_BOOT_TIMEOUT
        );
    }

    #[test]
    fn a_guest_that_contradicts_its_build_is_refused_where_it_is_written() {
        let err = format!(
            "{:#}",
            parse(
                r#"
            [images.windows]
            golden = "/srv/windows.qcow2"
            ssh_user = "vitro"
            guest = "unix"

            [images.windows.build]
            kind = "unattended-install"
            source = "/media/windows.iso"
            "#,
            )
            .unwrap_err()
        );

        assert!(err.contains("guest"), "{err}");
    }

    #[test]
    fn a_lume_build_carries_the_preset_that_makes_setup_unattended() {
        let config = parse_ok(
            r#"
            [images.macos]
            backend = "lume"
            golden = "tahoe-base"
            ssh_user = "lume"

            [images.macos.build]
            kind = "lume-ipsw"
            source = "latest"
            unattended = "tahoe"
            disk_size = "60G"
            "#,
        );

        let (_, build) = config.buildable_image("macos").unwrap();
        assert_eq!(build.source, Source::Latest);
        assert_eq!(build.unattended.as_deref(), Some("tahoe"));
    }

    #[test]
    fn a_local_ipsw_path_is_expanded() {
        let config = parse_ok(
            r#"
            [images.macos]
            backend = "lume"
            golden = "tahoe-base"
            ssh_user = "lume"

            [images.macos.build]
            kind = "lume-ipsw"
            source = "~/Downloads/restore.ipsw"
            "#,
        );

        let (_, build) = config.buildable_image("macos").unwrap();
        assert_eq!(
            build.source,
            Source::LocalPath(PathBuf::from("/home/dev/Downloads/restore.ipsw"))
        );
    }
}
