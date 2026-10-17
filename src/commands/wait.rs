//! `vitro wait` — block until a VM answers, and optionally until it has a
//! desktop.
//!
//! `run` already waits for SSH before it returns, but a guest can stop
//! answering afterwards: it restarts for an update, or a script asks it to. And
//! a GUI program can only be launched once auto-logon has reached the desktop,
//! which is later than SSH. Sessions wrote their own loops for both.

use std::time::{Duration, Instant};

use anyhow::{bail, Result};

use crate::commands::exec;
use crate::{process, ssh, Backend, Config, Guest, Paths, SysinfoProbe};

/// How long to wait when the image sets no boot timeout of its own.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

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
    let desktop = options
        .desktop
        .then(|| desktop_probe(record.backend, record.guest))
        .transpose()?;
    let timeout = options.timeout.unwrap_or_else(|| {
        config
            .image(&record.image)
            .map(|image| image.boot_timeout)
            .unwrap_or(DEFAULT_TIMEOUT)
    });

    let started = Instant::now();
    let (pid, start) = (record.pid, record.pid_start_time);
    let alive = move || process::is_running(&SysinfoProbe::new(), pid, start);
    ssh::wait_until_reachable(&ssh_binary, &target, timeout, alive)?;

    let Some(probe) = desktop else {
        return Ok(());
    };
    let options = ssh::SshOptions {
        command: probe,
        quoting: record.guest.into(),
        ..ssh::SshOptions::default()
    };
    loop {
        crate::signals::check()?;
        if ssh::run_quietly(&ssh_binary, &target, &options)? == 0 {
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
}
