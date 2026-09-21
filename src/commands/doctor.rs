//! `vitro doctor` — answer "why will it not start?" without reading a log.
//!
//! Every check here corresponds to something that makes `run` or `build` fail,
//! and every failure says what to do about it. This is the command an agent
//! should reach for first, which is why it reports on what it found rather than
//! only on what is broken: knowing which `qemu-system-*` was picked up, and
//! from where, settles most questions on its own.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;

use crate::{qemu, tools, Backend, BuildKind, Config, Paths, Source};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

impl fmt::Display for Level {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        })
    }
}

#[derive(Debug, Clone)]
pub struct Check {
    pub name: String,
    pub level: Level,
    pub detail: String,
    /// What to do about it. Present whenever the level is not `Ok`.
    pub fix: Option<String>,
}

impl Check {
    fn ok(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Ok,
            detail: detail.into(),
            fix: None,
        }
    }

    fn warn(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Warn,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }

    fn fail(name: impl Into<String>, detail: impl Into<String>, fix: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            level: Level::Fail,
            detail: detail.into(),
            fix: Some(fix.into()),
        }
    }
}

pub fn run(paths: &Paths, config: &Config) -> Vec<Check> {
    let mut checks = Vec::new();

    let host = qemu::HostTarget::detect();
    match &host {
        Ok(host) => checks.push(Check::ok(
            "host",
            format!(
                "{} on {}, machine {}, accelerator {}",
                std::env::consts::ARCH,
                std::env::consts::OS,
                host.machine,
                host.accel
            ),
        )),
        Err(e) => checks.push(Check::fail(
            "host",
            format!("{e:#}"),
            "vitro has no QEMU configuration for this platform yet",
        )),
    }

    let cfg = config.tools();
    // Only what the configuration actually asks for: a machine set up for
    // macOS guests alone has no reason to have QEMU, and reporting its absence
    // as a failure would bury the thing that is really missing.
    let backends: Vec<Backend> = config.images().map(|image| image.backend).collect();
    let wants_qemu = backends.is_empty() || backends.contains(&Backend::Qemu);
    let wants_lume = backends.contains(&Backend::Lume);

    if wants_qemu {
        let qemu_binary = host
            .as_ref()
            .map(|host| host.binary.clone())
            .unwrap_or_else(|_| "qemu-system-x86_64".into());
        checks.push(binary(&qemu_binary, cfg.qemu.as_deref(), "tools.qemu"));
        checks.push(binary(
            "qemu-img",
            cfg.qemu_img.as_deref(),
            "tools.qemu_img",
        ));
        if let Ok(host) = &host {
            checks.push(accelerator(&qemu_binary, cfg.qemu.as_deref(), &host.accel));
        }
    }
    if wants_lume {
        checks.push(binary("lume", cfg.lume.as_deref(), "tools.lume"));
    }
    checks.push(binary("ssh", cfg.ssh.as_deref(), "tools.ssh"));
    checks.push(binary("scp", cfg.scp.as_deref(), "tools.scp"));

    checks.push(ssh_key(paths));
    checks.push(include_line(paths));
    checks.push(state_dir(paths));
    checks.extend(images(paths, config, host.as_ref().ok()));
    checks
}

/// Resolve a program the way the real commands do, and report its version.
fn binary(name: &str, configured: Option<&Path>, setting: &str) -> Check {
    match tools::resolve(name, configured, setting) {
        Err(e) => Check::fail(
            name.to_string(),
            format!("{e:#}"),
            format!("install it, or set `{setting}` in config.toml"),
        ),
        Ok(found) => {
            let origin = match found.origin {
                tools::Origin::Configured => setting,
                tools::Origin::Path => "PATH",
            };
            let detail = match version_flag(name).and_then(|flag| version_of(&found.path, flag)) {
                Some(version) => format!("{} (from {origin}) — {version}", found.path.display()),
                None => format!("{} (from {origin})", found.path.display()),
            };
            Check::ok(name.to_string(), detail)
        }
    }
}

/// How a program is asked for its version, or `None` for one that cannot be
/// asked at all.
///
/// Every program here was being sent `--version`, which OpenSSH does not have.
/// On Unix it exits with a usage message, so `doctor` reported "version
/// unknown" for `ssh` and `scp` and nobody noticed; the same call on Windows
/// never returns, which hangs the one command a user runs when nothing works.
///
/// `ssh` answers `-V`. `scp` answers nothing — it ships with OpenSSH and has no
/// version flag, so asking produces an error message, and an error message
/// printed where a version belongs is worse than admitting there is none.
///
/// Takes the program's name, never a resolved path: the caller has the name
/// because it is what it looked the program up by, and a path would drag in the
/// question of whose separator to split on.
fn version_flag(name: &str) -> Option<&'static str> {
    match name {
        "ssh" => Some("-V"),
        "scp" => None,
        _ => Some("--version"),
    }
}

/// stderr as well as stdout, because `ssh -V` writes its version to stderr and
/// a version read from the wrong stream is indistinguishable from none.
fn version_of(path: &Path, flag: &str) -> Option<String> {
    let output = Command::new(path).arg(flag).output().ok()?;
    let streams = [&output.stdout, &output.stderr];
    streams
        .iter()
        .filter_map(|stream| {
            let text = String::from_utf8_lossy(stream);
            text.lines()
                .find(|line| !line.trim().is_empty())
                .map(|line| line.trim().to_string())
        })
        .next()
}

/// Ask QEMU itself which accelerators it has, rather than guessing from the
/// platform: a QEMU built without HVF looks exactly like one with it until a
/// guest crawls along at emulation speed.
fn accelerator(binary_name: &str, configured: Option<&Path>, wanted: &str) -> Check {
    let Ok(found) = tools::resolve(binary_name, configured, "tools.qemu") else {
        return Check::warn(
            "accelerator",
            "not checked: QEMU could not be resolved",
            "fix the QEMU check first",
        );
    };
    let listed = Command::new(&found.path)
        .args(["-accel", "help"])
        .output()
        .ok()
        .map(|out| {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            text
        });
    match listed {
        None => Check::warn(
            "accelerator",
            "QEMU would not report its accelerators",
            "run `qemu-system-* -accel help` by hand",
        ),
        Some(text) if text.split_whitespace().any(|word| word == wanted) => {
            // Compiled in is not the same as usable. KVM in particular needs
            // /dev/kvm to be readable and writable by this user, and the whole
            // point of `doctor` is to say so here rather than let `run` fail.
            match accelerator_device(wanted) {
                Err(reason) => Check::fail(
                    "accelerator",
                    reason,
                    "add yourself to the `kvm` group, or log in again if you already have",
                ),
                Ok(()) => Check::ok("accelerator", format!("{wanted} is available")),
            }
        }
        Some(_) => Check::fail(
            "accelerator",
            format!("{wanted} is not in this QEMU's accelerator list"),
            match wanted {
                "kvm" => "check that /dev/kvm exists and you can read and write it",
                "hvf" => "install a QEMU built with Hypervisor.framework support",
                _ => "install a QEMU with hardware acceleration for this platform",
            },
        ),
    }
}

/// Whether the device this accelerator needs can actually be opened.
///
/// Only KVM has one. HVF is reached through a Hypervisor.framework entitlement
/// rather than a device node, so a QEMU that lists it can use it.
fn accelerator_device(wanted: &str) -> Result<(), String> {
    if wanted != "kvm" {
        return Ok(());
    }
    let device = Path::new("/dev/kvm");
    if !device.exists() {
        return Err("/dev/kvm does not exist".into());
    }
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)
        .map(|_| ())
        .map_err(|e| format!("/dev/kvm cannot be opened for reading and writing: {e}"))
}

fn ssh_key(paths: &Paths) -> Check {
    let key = paths.ssh_key();
    if !key.exists() {
        return Check::fail(
            "ssh key",
            format!("{} does not exist", key.display()),
            "vitro keygen".to_string(),
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&key) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                return Check::fail(
                    "ssh key",
                    format!("{} is mode {mode:o}", key.display()),
                    format!("chmod 600 {}", key.display()),
                );
            }
        }
    }
    Check::ok("ssh key", key.display().to_string())
}

/// `ssh-config --write` is useless until `~/.ssh/config` includes the file it
/// writes, and the failure mode — a host alias that simply does not resolve —
/// gives no hint that this is why.
fn include_line(paths: &Paths) -> Check {
    let include = paths.ssh_config();
    if !include.exists() {
        return Check::ok(
            "ssh_config include",
            "not in use; `vitro ssh-config --write` starts it",
        );
    }
    let user_config = crate::commands::sshconfig::user_config(paths);
    let wanted = include.display().to_string();
    if crate::commands::sshconfig::is_included(paths) {
        Check::ok(
            "ssh_config include",
            format!("{} includes it", user_config.display()),
        )
    } else {
        Check::warn(
            "ssh_config include",
            format!("{} is written but nothing includes it", include.display()),
            format!(
                "add `Include {wanted}` near the top of {}",
                user_config.display()
            ),
        )
    }
}

fn state_dir(paths: &Paths) -> Check {
    let dir = paths.vms_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Check::fail(
            "state directory",
            format!("cannot create {}: {e}", dir.display()),
            "check the permissions on the parent directory".to_string(),
        );
    }
    match tempfile::NamedTempFile::new_in(&dir) {
        Ok(_) => Check::ok("state directory", dir.display().to_string()),
        Err(e) => Check::fail(
            "state directory",
            format!("cannot write in {}: {e}", dir.display()),
            "check the permissions on it".to_string(),
        ),
    }
}

/// Where the UEFI firmware most likely is, worked out from the QEMU that was
/// found.
///
/// edk2 ships inside the QEMU distribution rather than separately, so the file
/// is almost always beside the binary that needs it. Naming the path beats
/// telling somebody to go and find "the edk2 code file for this architecture",
/// which is the one prerequisite with no obvious place to look.
fn firmware_hint(config: &Config, binary_name: &str, arch: &str) -> Option<PathBuf> {
    let found = tools::resolve(binary_name, config.tools().qemu.as_deref(), "tools.qemu").ok()?;
    let prefix = found.path.parent()?.parent()?;
    let candidate = prefix
        .join("share")
        .join("qemu")
        .join(format!("edk2-{arch}-code.fd"));
    candidate.exists().then_some(candidate)
}

/// What to tell somebody whose firmware is not set or not there.
fn firmware_fix(hint: Option<&Path>) -> String {
    match hint {
        Some(path) => format!("set `firmware` to {}", path.display()),
        None => "point `firmware` at the edk2 code file for this architecture".to_string(),
    }
}

fn images(paths: &Paths, config: &Config, host: Option<&qemu::HostTarget>) -> Vec<Check> {
    if config.is_empty() {
        return vec![Check::warn(
            "images",
            format!("no [images.*] section in {}", paths.config_file().display()),
            "add one; `vitro run` needs an image key to start from",
        )];
    }

    config
        .images()
        .map(|image| {
            let name = format!("image {}", image.key);
            // A machine with no legacy BIOS cannot start without one, and
            // leaving it unset used to pass here and fail in `run` — which is
            // the opposite of what `doctor` is for.
            let needs_firmware =
                image.backend == Backend::Qemu && host.is_some_and(|host| host.requires_firmware());
            let hint =
                host.and_then(|host| firmware_hint(config, &host.binary, std::env::consts::ARCH));
            match &image.firmware {
                Some(firmware) if !firmware.exists() => {
                    return Check::fail(
                        name,
                        format!("firmware {} does not exist", firmware.display()),
                        firmware_fix(hint.as_deref()),
                    );
                }
                None if needs_firmware => {
                    return Check::fail(
                        name,
                        format!(
                            "machine {} needs a UEFI firmware image and `firmware` is not set",
                            host.map(|h| h.machine.as_str()).unwrap_or("virt")
                        ),
                        firmware_fix(hint.as_deref()),
                    );
                }
                _ => {}
            }

            let crate::Golden::VmName(vm) = &image.golden else {
                let path = match &image.golden {
                    crate::Golden::Image(path) => path.clone(),
                    // Unset means the newest build, so what to report on is the
                    // file that would answer a `run` right now — or, with
                    // nothing built, where one would land, so the report names
                    // something concrete either way.
                    _ => crate::golden::newest(&paths.golden_dir(), &image.key)
                        .unwrap_or_else(|| {
                            paths.golden_dir().join(format!("{}-*.qcow2", image.key))
                        }),
                };
                return if path.exists() {
                    Check::ok(name, format!("golden {}", path.display()))
                } else {
                    match &image.build {
                        Some(build) => media(name, &path, build, paths),
                        None => Check::fail(
                            name,
                            format!("golden {} is missing", path.display()),
                            "add an [images.*.build] section, or point `golden` at an existing image",
                        ),
                    }
                };
            };
            Check::ok(name, format!("delegated to the backend as {vm}"))
        })
        .collect()
}

/// Whether vitro's own media directory holds anything it could install from.
fn has_media(paths: &Paths) -> bool {
    std::fs::read_dir(paths.media_dir())
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
        })
}

/// A missing golden is only a problem if it cannot be built, so report on the
/// media instead of on the absence.
fn media(name: String, golden: &Path, build: &crate::Build, paths: &Paths) -> Check {
    match (&build.kind, &build.source) {
        (_, Some(Source::LocalPath(path))) if !path.exists() => Check::fail(
            name,
            format!("build media {} does not exist", path.display()),
            "point `source` at the installation media",
        ),
        // No `source` at all is only legal for a Windows install, where it
        // means "whatever is in vitro's media directory". Saying that beats
        // reporting a golden image as missing and leaving out the reason.
        (BuildKind::UnattendedInstall, None) if !has_media(paths) => Check::fail(
            name,
            format!("no installation media in {}", paths.media_dir().display()),
            crate::media::windows_media_instructions(&paths.media_dir(), std::env::consts::ARCH),
        ),
        (BuildKind::CloudImage | BuildKind::UnattendedInstall | BuildKind::LumeIpsw, _) => {
            Check::warn(
                name,
                format!("golden {} is missing", golden.display()),
                "run `vitro build` for this image",
            )
        }
    }
}

pub fn render(checks: &[Check]) -> String {
    let width = checks.iter().map(|c| c.name.len()).max().unwrap_or(0);
    let mut out = String::new();
    for check in checks {
        out.push_str(&format!(
            "{:<5} {:<width$}  {}\n",
            check.level.to_string(),
            check.name,
            check.detail
        ));
        if let Some(fix) = &check.fix {
            out.push_str(&format!("{:<5} {:<width$}  → {fix}\n", "", "",));
        }
    }
    out
}

/// Whether anything is broken enough to stop `run` or `build`.
pub fn worst(checks: &[Check]) -> Level {
    checks
        .iter()
        .map(|check| check.level)
        .max_by_key(|level| match level {
            Level::Ok => 0,
            Level::Warn => 1,
            Level::Fail => 2,
        })
        .unwrap_or(Level::Ok)
}

pub fn report(paths: &Paths, config: &Config) -> Result<i32> {
    let checks = run(paths, config);
    print!("{}", render(&checks));
    Ok(match worst(&checks) {
        Level::Fail => 1,
        _ => 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_check_that_is_not_ok_says_what_to_do() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        for check in run(&paths, &Config::default()) {
            assert_eq!(
                check.level != Level::Ok,
                check.fix.is_some(),
                "{}: {:?}",
                check.name,
                check
            );
        }
    }

    #[test]
    fn openssh_is_asked_for_its_version_the_only_way_it_answers() {
        // `ssh --version` is not a flag OpenSSH has. On Unix it exits with a
        // usage message, so the version simply came back unknown; on Windows
        // the same call never returns and `doctor` hangs — the one command a
        // user runs when nothing else works.
        assert_eq!(version_flag("ssh"), Some("-V"));
        assert_eq!(
            version_flag("scp"),
            None,
            "scp has no version flag to ask with"
        );

        assert_eq!(version_flag("qemu-system-aarch64"), Some("--version"));
        assert_eq!(version_flag("qemu-img"), Some("--version"));
        assert_eq!(version_flag("lume"), Some("--version"));
    }

    #[test]
    fn firmware_that_is_not_set_is_a_failure_when_the_machine_needs_one() {
        // `virt` has no legacy BIOS. Leaving this unset used to pass `doctor`
        // and fail in `run`, which is the one order of events `doctor` exists
        // to prevent.
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        let config = Config::parse(
            "[images.linux]\ngolden = \"/srv/linux.qcow2\"\nssh_user = \"dev\"\n",
            temp.path(),
            &paths.ssh_key(),
        )
        .unwrap();
        let host = qemu::HostTarget::for_platform("macos", "aarch64").unwrap();

        let checks = images(&paths, &config, Some(&host));

        let check = checks.iter().find(|c| c.name == "image linux").unwrap();
        assert_eq!(check.level, Level::Fail, "{check:?}");
        assert!(check.detail.contains("firmware"), "{check:?}");
        assert!(
            check.fix.as_ref().unwrap().contains("firmware"),
            "{check:?}"
        );
    }

    #[test]
    fn a_machine_without_firmware_of_its_own_does_not_demand_one() {
        // q35 boots from a BIOS, so the same missing setting is not a problem
        // there and saying otherwise would send people configuring nothing.
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        let config = Config::parse(
            "[images.linux]\ngolden = \"/srv/linux.qcow2\"\nssh_user = \"dev\"\n",
            temp.path(),
            &paths.ssh_key(),
        )
        .unwrap();
        let host = qemu::HostTarget::for_platform("linux", "x86_64").unwrap();

        let checks = images(&paths, &config, Some(&host));

        let check = checks.iter().find(|c| c.name == "image linux").unwrap();
        assert!(!check.detail.contains("firmware"), "{check:?}");
    }

    #[test]
    fn a_missing_ssh_key_is_reported_with_the_command_that_makes_one() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        let check = ssh_key(&paths);

        assert_eq!(check.level, Level::Fail);
        // The command vitro has, not one the reader has to assemble out of an
        // `ssh-keygen` invocation with the right path in it.
        assert_eq!(check.fix.unwrap(), "vitro keygen");
    }

    #[cfg(unix)]
    #[test]
    fn a_world_readable_ssh_key_is_a_failure_because_ssh_refuses_it() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        let key = paths.ssh_key();
        std::fs::create_dir_all(key.parent().unwrap()).unwrap();
        std::fs::write(&key, "key").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();

        let check = ssh_key(&paths);

        assert_eq!(check.level, Level::Fail);
        assert!(check.fix.unwrap().contains("chmod 600"));
    }

    #[test]
    fn an_accelerator_without_a_device_node_is_judged_on_the_qemu_listing_alone() {
        assert!(accelerator_device("hvf").is_ok());
        assert!(accelerator_device("tcg").is_ok());
    }

    #[test]
    fn the_worst_level_decides_the_exit_code() {
        let ok = Check::ok("a", "b");
        let warn = Check::warn("a", "b", "c");
        let fail = Check::fail("a", "b", "c");

        assert_eq!(worst(&[]), Level::Ok);
        assert_eq!(worst(&[ok.clone(), warn.clone()]), Level::Warn);
        assert_eq!(worst(&[ok, warn, fail]), Level::Fail);
    }

    #[test]
    fn an_empty_configuration_points_at_the_file_that_should_have_images() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        let checks = images(&paths, &Config::default(), None);

        assert_eq!(checks.len(), 1);
        assert!(checks[0].detail.contains("config.toml"), "{:?}", checks[0]);
    }
}
