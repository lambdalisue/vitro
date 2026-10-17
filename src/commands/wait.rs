//! `vitro wait` and `vitro restart` — block until a VM answers, and
//! optionally until it has a desktop.
//!
//! `run` already waits for SSH before it returns, but a guest can stop
//! answering afterwards: it restarts for an update, or a script asks it to. And
//! a GUI program can only be launched once auto-logon has reached the desktop,
//! which is later than SSH. Sessions wrote their own loops for both.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::commands::exec;
use crate::{process, ssh, Backend, Config, Guest, Paths, SysinfoProbe, VmRecord};

/// How long to wait when the image sets no boot timeout of its own.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// One probe's connection attempt while the guest may be down: short, because
/// a guest that is not there should fail fast and be asked again.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Options {
    /// Also wait for somebody to be logged in to the desktop.
    pub desktop: bool,
    /// The image's boot timeout when `None`.
    pub timeout: Option<Duration>,
}

pub fn wait(paths: &Paths, config: &Config, name: &str, options: &Options) -> Result<()> {
    let (record, target, ssh_binary) = exec::prepare(paths, config, name)?;
    // Refused before waiting at all: a guest with no desktop would only be
    // found to have none when the time ran out.
    let desktop = desktop_for(&record, options)?;
    let timeout = timeout_for(config, &record, options);

    let started = Instant::now();
    let (pid, start) = (record.pid, record.pid_start_time);
    let alive = move || process::is_running(&SysinfoProbe::new(), pid, start);
    ssh::wait_until_reachable(&ssh_binary, &target, timeout, alive)?;

    match desktop {
        Some(probe) => wait_for_desktop(&ssh_binary, &target, &record, probe, started, timeout),
        None => Ok(()),
    }
}

/// Ask the guest to restart and return once it has come back from it.
///
/// "Answers over SSH" is not enough on its own: for a few seconds after the
/// restart is asked for, the old session still answers. So the guest's boot is
/// read before, and the wait ends only on a different one.
pub fn restart(paths: &Paths, config: &Config, name: &str, options: &Options) -> Result<()> {
    let (record, target, ssh_binary) = exec::prepare(paths, config, name)?;
    let desktop = desktop_for(&record, options)?;
    let timeout = timeout_for(config, &record, options);
    let marker = ssh::SshOptions {
        command: boot_marker(record.guest),
        quoting: record.guest.into(),
        connect_timeout: Some(PROBE_CONNECT_TIMEOUT),
        ..ssh::SshOptions::default()
    };

    let before = match ssh::run_output(&ssh_binary, &target, &marker)? {
        (0, out) if !out.trim().is_empty() => out.trim().to_string(),
        (code, _) => bail!(
            "cannot read how long {} has been up (exit {code})",
            record.name
        ),
    };

    let request = ssh::SshOptions {
        command: restart_command(record.guest),
        quoting: record.guest.into(),
        ..ssh::SshOptions::default()
    };
    // 255 is ssh losing the connection, which a guest going down may well do
    // before the command has reported back.
    match ssh::run_quietly(&ssh_binary, &target, &request)? {
        0 | 255 => {}
        code => bail!(
            "{} refused to restart (exit {code}); a Unix guest needs passwordless sudo for it",
            record.name
        ),
    }

    let started = Instant::now();
    loop {
        crate::signals::check()?;
        if !process::is_running(&SysinfoProbe::new(), record.pid, record.pid_start_time) {
            bail!("{} stopped instead of restarting", record.name);
        }
        if let Ok((0, out)) = ssh::run_output(&ssh_binary, &target, &marker) {
            let now = out.trim();
            if !now.is_empty() && now != before {
                break;
            }
        }
        if started.elapsed() >= timeout {
            bail!(
                "{} did not come back from its restart within {}s",
                record.name,
                timeout.as_secs()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    match desktop {
        Some(probe) => wait_for_desktop(&ssh_binary, &target, &record, probe, started, timeout),
        None => Ok(()),
    }
}

fn desktop_for(record: &VmRecord, options: &Options) -> Result<Option<Vec<String>>> {
    options
        .desktop
        .then(|| desktop_probe(record.backend, record.guest))
        .transpose()
}

fn timeout_for(config: &Config, record: &VmRecord, options: &Options) -> Duration {
    options.timeout.unwrap_or_else(|| {
        config
            .image(&record.image)
            .map(|image| image.boot_timeout)
            .unwrap_or(DEFAULT_TIMEOUT)
    })
}

fn wait_for_desktop(
    ssh_binary: &Path,
    target: &ssh::SshTarget,
    record: &VmRecord,
    probe: Vec<String>,
    started: Instant,
    timeout: Duration,
) -> Result<()> {
    let options = ssh::SshOptions {
        command: probe,
        quoting: record.guest.into(),
        connect_timeout: Some(PROBE_CONNECT_TIMEOUT),
        ..ssh::SshOptions::default()
    };
    loop {
        crate::signals::check()?;
        if ssh::run_quietly(ssh_binary, target, &options)? == 0 {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            bail!(
                "{} answers but nobody was logged in to its desktop after {}s",
                record.name,
                timeout.as_secs()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// A command that succeeds once a desktop session exists.
///
/// The shell that draws the desktop is what a launched program needs, so its
/// process is the signal: Explorer on Windows, the Dock on macOS. A Linux
/// guest from a cloud image has no desktop at all.
fn desktop_probe(backend: Backend, guest: Guest) -> Result<Vec<String>> {
    match (backend, guest) {
        (_, Guest::Windows) => Ok(vec![
            "if (Get-Process explorer -ErrorAction SilentlyContinue) { exit 0 } else { exit 1 }"
                .into(),
        ]),
        (Backend::Lume, _) => Ok(vec!["pgrep".into(), "-x".into(), "Dock".into()]),
        (Backend::Qemu, Guest::Unix) => {
            bail!("a Linux guest has no desktop for vitro to wait for; wait without --desktop")
        }
    }
}

/// A command that prints something unique to the current boot.
fn boot_marker(guest: Guest) -> Vec<String> {
    match guest {
        Guest::Windows => vec![
            "(Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')"
                .into(),
        ],
        // Linux keeps a fresh id per boot; macOS has its boot time.
        Guest::Unix => vec![
            "sh".into(),
            "-c".into(),
            "cat /proc/sys/kernel/random/boot_id 2>/dev/null || sysctl -n kern.boottime".into(),
        ],
    }
}

/// A command that asks the guest to restart, as a restart from inside it, so
/// the operating system shuts down the way it would for a person.
fn restart_command(guest: Guest) -> Vec<String> {
    match guest {
        Guest::Windows => vec!["Restart-Computer -Force".into()],
        Guest::Unix => vec![
            "sh".into(),
            "-c".into(),
            "sudo -n shutdown -r now || sudo -n reboot".into(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_windows_desktop_is_there_once_explorer_runs() {
        let probe = desktop_probe(Backend::Qemu, Guest::Windows).unwrap();

        assert_eq!(probe.len(), 1);
        assert!(probe[0].contains("Get-Process explorer"), "{probe:?}");
        assert!(probe[0].contains("exit 1"), "{probe:?}");
    }

    #[test]
    fn a_macos_desktop_is_there_once_the_dock_runs() {
        assert_eq!(
            desktop_probe(Backend::Lume, Guest::Unix).unwrap(),
            ["pgrep", "-x", "Dock"]
        );
    }

    #[test]
    fn a_linux_guest_is_refused_rather_than_waited_on_forever() {
        let err = desktop_probe(Backend::Qemu, Guest::Unix)
            .unwrap_err()
            .to_string();

        assert!(err.contains("--desktop"), "{err}");
    }

    #[test]
    fn a_boot_is_told_apart_by_what_each_guest_keeps_per_boot() {
        assert!(boot_marker(Guest::Windows)[0].contains("LastBootUpTime"));
        let unix = boot_marker(Guest::Unix).join(" ");
        assert!(unix.contains("/proc/sys/kernel/random/boot_id"), "{unix}");
        assert!(unix.contains("kern.boottime"), "{unix}");
    }

    #[test]
    fn a_restart_is_asked_of_the_guest_without_waiting_for_a_password() {
        assert_eq!(restart_command(Guest::Windows), ["Restart-Computer -Force"]);
        let unix = restart_command(Guest::Unix).join(" ");
        assert!(unix.contains("sudo -n"), "{unix}");
    }
}
