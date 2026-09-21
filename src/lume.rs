//! Driving `lume` for macOS guests.
//!
//! macOS may only be virtualised on Apple hardware through Virtualization.
//! framework, so vitro delegates rather than reimplements: `lume` owns the VM,
//! and vitro asks it to create, start, clone, stop and delete one. Everything
//! after that is the same as any other guest — vitro makes its own SSH
//! connection to the address `lume` reports, with its own key, rather than
//! using `lume ssh`, which authenticates with a password by default.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::ByteSize;

/// Lume reports telemetry unless told not to. vitro tells it not to on every
/// call rather than relying on the user having run `lume config telemetry
/// disable`, because vitro is what is making the call.
const TELEMETRY_OFF: (&str, &str) = ("LUME_TELEMETRY_ENABLED", "false");

/// How often the VM is asked whether it is reachable yet.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmInfo {
    pub name: String,
    pub status: String,
    pub ip_address: Option<String>,
    pub ssh_available: bool,
}

impl VmInfo {
    pub fn is_running(&self) -> bool {
        self.status.eq_ignore_ascii_case("running")
    }

    /// Where the guest can be connected to, once it can be at all.
    ///
    /// Both halves have to be there. An address on its own means only that the
    /// guest has been given one; `lume` probes port 22 separately and refuses
    /// `lume ssh` until that probe answers.
    pub fn reachable_address(&self) -> Option<&str> {
        if !self.ssh_available {
            return None;
        }
        self.ip_address.as_deref()
    }
}

pub struct CreateSpec<'a> {
    pub name: &'a str,
    pub ipsw: &'a Path,
    pub cpus: u32,
    pub memory: ByteSize,
    pub disk_size: Option<ByteSize>,
    /// A built-in preset name such as `tahoe`, or a path to a YAML file.
    ///
    /// This is what makes a macOS install unattended: it creates the account,
    /// turns on SSH and automatic logon, and stops the machine sleeping or
    /// locking, none of which can be done through the setup assistant without
    /// somebody clicking. vitro has no answer-file equivalent for macOS and
    /// does not need one.
    pub unattended: Option<&'a str>,
}

pub fn create_args(spec: &CreateSpec<'_>) -> Vec<String> {
    let mut args = vec![
        "create".to_string(),
        spec.name.to_string(),
        "--os".into(),
        "macOS".into(),
        "--ipsw".into(),
        spec.ipsw.display().to_string(),
        "--cpu".into(),
        spec.cpus.to_string(),
        "--memory".into(),
        // Lume reads a bare number as GB, so the unit is always given.
        format!("{}MB", spec.memory.mib()),
    ];
    if let Some(size) = spec.disk_size {
        args.push("--disk-size".into());
        args.push(format!("{}MB", size.mib()));
    }
    if let Some(preset) = spec.unattended {
        args.push("--unattended".into());
        args.push(preset.to_string());
    }
    args
}

pub fn run_args(name: &str) -> Vec<String> {
    vec![
        "run".into(),
        name.into(),
        // No viewer and no foreground: vitro owns the lifetime, and the screen
        // is reached over VNC when it is wanted at all.
        "--display".into(),
        "none".into(),
        "--detach".into(),
    ]
}

pub fn clone_args(from: &str, to: &str) -> Vec<String> {
    vec!["clone".into(), from.into(), to.into()]
}

pub fn stop_args(name: &str) -> Vec<String> {
    vec!["stop".into(), name.into()]
}

pub fn delete_args(name: &str) -> Vec<String> {
    vec!["delete".into(), name.into(), "--force".into()]
}

/// A graceful stop. `lume` does it from inside the guest over SSH, so it needs
/// the account the preset made.
pub fn shutdown_args(name: &str, user: &str, password: &str) -> Vec<String> {
    vec![
        "shutdown".into(),
        name.into(),
        "--user".into(),
        user.into(),
        "--password".into(),
        password.into(),
    ]
}

/// The account `--unattended` creates, and the password it gives it.
///
/// Published by lume and the same on every VM its presets make, which is
/// exactly why vitro replaces it with a key before the image becomes a golden.
pub const PRESET_USER: &str = "lume";
pub const PRESET_PASSWORD: &str = "lume";

/// Run a command in the guest through `lume`, which knows the preset's
/// password.
///
/// Only used once per build, to put vitro's key in place. Everything after that
/// is vitro's own SSH, with the key — `lume ssh` authenticates with a password
/// by default, and a password shared by every VM lume creates is not something
/// to keep reaching for.
pub fn ssh_args(name: &str, command: &str) -> Vec<String> {
    vec![
        "ssh".into(),
        name.into(),
        command.into(),
        "--user".into(),
        PRESET_USER.into(),
        "--password".into(),
        PRESET_PASSWORD.into(),
    ]
}

pub fn get_args(name: &str) -> Vec<String> {
    vec!["get".into(), name.into(), "--format".into(), "json".into()]
}

/// The URL of the newest restore image `lume` will install from.
///
/// Asked of `lume` rather than worked out here, because which macOS builds are
/// installable on which host is Apple's business and changes without notice.
pub fn latest_ipsw(binary: &Path) -> Result<String> {
    let output = run(binary, &["ipsw".into()])?;
    parse_ipsw_url(&output)
}

fn parse_ipsw_url(output: &str) -> Result<String> {
    // `lume` mixes progress into its output; the URL is the last thing that
    // looks like one.
    let url = output
        .lines()
        .map(str::trim)
        .rfind(|line| line.starts_with("https://"));
    match url {
        Some(url) => Ok(url.to_string()),
        None => bail!("lume ipsw reported no download URL"),
    }
}

/// Run a `lume` subcommand, returning its standard output.
pub fn run(binary: &Path, args: &[String]) -> Result<String> {
    let output = Command::new(binary)
        .args(args)
        .env(TELEMETRY_OFF.0, TELEMETRY_OFF.1)
        .output()
        .with_context(|| format!("cannot run {}", binary.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        bail!(
            "lume {} failed: {detail}",
            args.first().map(String::as_str).unwrap_or("")
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a `lume` subcommand that takes minutes, letting it report as it goes.
///
/// Installing macOS runs for several minutes. Capturing its output would mean
/// showing nothing at all until it finished, which is indistinguishable from
/// vitro having hung.
pub fn run_visibly(binary: &Path, args: &[String]) -> Result<()> {
    let status = Command::new(binary)
        .args(args)
        .env(TELEMETRY_OFF.0, TELEMETRY_OFF.1)
        .stdin(std::process::Stdio::null())
        .status()
        .with_context(|| format!("cannot run {}", binary.display()))?;

    if !status.success() {
        bail!(
            "lume {} failed with {status}",
            args.first().map(String::as_str).unwrap_or("")
        );
    }
    Ok(())
}

pub fn get(binary: &Path, name: &str) -> Result<VmInfo> {
    let text = run(binary, &get_args(name))?;
    parse_info(name, &text)
}

/// Read what vitro needs out of `lume get --format json`.
///
/// Field by field rather than into a struct: `lume` reports a great deal more
/// than this, and a shape that has to match exactly would break on a release
/// that adds a field.
pub fn parse_info(name: &str, json: &str) -> Result<VmInfo> {
    let parsed: serde_json::Value = serde_json::from_str(json.trim())
        .with_context(|| format!("cannot read what lume said about {name}"))?;

    // `lume get` answers with a list holding the one VM asked about. Taking
    // the first entry rather than insisting on a list keeps this working if
    // that ever becomes the bare object it reads as.
    let value = match parsed.as_array() {
        Some(entries) => match entries.first() {
            Some(first) => first.clone(),
            None => bail!("lume knows nothing about {name}"),
        },
        None => parsed,
    };

    let string = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty())
    };

    Ok(VmInfo {
        name: string("name").unwrap_or_else(|| name.to_string()),
        status: string("status").unwrap_or_else(|| "unknown".into()),
        ip_address: string("ipAddress"),
        ssh_available: value
            .get("sshAvailable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// Wait until the guest is ready to be connected to, and say where.
///
/// Both halves matter and they do not arrive together: a macOS guest takes the
/// better part of a minute to get an address, and sshd is listening some time
/// after that. `lume` tracks the second separately and refuses `lume ssh`
/// until it is true, so waiting only for the address means arriving early and
/// being told SSH is not available.
pub fn wait_until_ready(binary: &Path, name: &str, timeout: Duration) -> Result<String> {
    let started = Instant::now();
    loop {
        crate::signals::check()?;
        let info = get(binary, name)?;
        if let Some(address) = info.reachable_address() {
            return Ok(address.to_string());
        }
        if !info.is_running() && started.elapsed() > POLL_INTERVAL {
            bail!(
                "{name} stopped before it was reachable (lume says {})",
                info.status
            );
        }
        if started.elapsed() >= timeout {
            bail!(
                "lume did not report {name} as reachable within {}s (address {}, ssh {})",
                timeout.as_secs(),
                info.ip_address.as_deref().unwrap_or("none"),
                info.ssh_available
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec() -> CreateSpec<'static> {
        CreateSpec {
            name: "macos",
            ipsw: Path::new("/media/restore.ipsw"),
            cpus: 4,
            memory: ByteSize::from_gib(8),
            disk_size: None,
            unattended: Some("tahoe"),
        }
    }

    #[test]
    fn a_size_always_carries_its_unit() {
        // A bare number means gigabytes to lume, so 8192 would ask for 8 TB.
        let args = create_args(&CreateSpec {
            disk_size: Some(ByteSize::from_gib(100)),
            ..spec()
        })
        .join(" ");

        assert!(args.contains("--memory 8192MB"), "{args}");
        assert!(args.contains("--disk-size 102400MB"), "{args}");
    }

    #[test]
    fn the_preset_is_what_makes_the_install_unattended() {
        let args = create_args(&spec()).join(" ");

        assert!(args.contains("--unattended tahoe"), "{args}");
        assert!(args.contains("--ipsw /media/restore.ipsw"), "{args}");
    }

    #[test]
    fn a_created_vm_without_a_preset_asks_for_no_setup_at_all() {
        let args = create_args(&CreateSpec {
            unattended: None,
            ..spec()
        })
        .join(" ");

        assert!(!args.contains("--unattended"), "{args}");
    }

    #[test]
    fn running_a_guest_opens_no_window_and_does_not_block() {
        let args = run_args("macos-7f3a2c").join(" ");

        assert!(args.contains("--display none"), "{args}");
        assert!(args.contains("--detach"), "{args}");
    }

    #[test]
    fn the_one_password_login_names_the_account_it_is_for() {
        let args = ssh_args("macos", "whoami").join(" ");

        assert!(args.starts_with("ssh macos whoami"), "{args}");
        assert!(args.contains("--user lume"), "{args}");
        assert!(args.contains("--password lume"), "{args}");
    }

    #[test]
    fn deleting_does_not_stop_to_ask() {
        assert!(delete_args("macos-7f3a2c").contains(&"--force".to_string()));
    }

    #[test]
    fn the_shape_lume_actually_answers_with_is_a_list_of_one() {
        // Verbatim from `lume get <name> --format json`, trimmed. Reading the
        // fields off the list itself finds nothing and reports every VM as
        // being in an unknown state.
        let info = parse_info(
            "vitro-macos",
            r#"[
              {
                "locationName" : "home",
                "sharedDirectories" : null,
                "networkMode" : "nat",
                "ipAddress" : "192.168.64.7",
                "name" : "vitro-macos",
                "diskSize" : { "allocated" : 22857289728, "total" : 85899345920 },
                "os" : "macOS",
                "status" : "running",
                "display" : "1024x768",
                "sshAvailable" : true,
                "vncUrl" : null,
                "cpuCount" : 4,
                "provisioningOperation" : null,
                "memorySize" : 8589934592,
                "downloadProgress" : null
              }
            ]"#,
        )
        .unwrap();

        assert_eq!(info.name, "vitro-macos");
        assert_eq!(info.status, "running");
        assert_eq!(info.ip_address.as_deref(), Some("192.168.64.7"));
        assert!(info.ssh_available);
    }

    #[test]
    fn a_stopped_vm_reports_null_rather_than_leaving_the_fields_out() {
        let info = parse_info(
            "vitro-macos",
            r#"[{"name":"vitro-macos","status":"stopped","ipAddress":null,"sshAvailable":null}]"#,
        )
        .unwrap();

        assert_eq!(info.status, "stopped");
        assert_eq!(info.ip_address, None);
        assert!(!info.ssh_available);
    }

    #[test]
    fn an_empty_list_means_there_is_no_such_vm() {
        let err = parse_info("ghost", "[]").unwrap_err().to_string();

        assert!(err.contains("ghost"), "{err}");
    }

    #[test]
    fn the_address_and_the_status_are_read_out_of_what_lume_reports() {
        let info = parse_info(
            "macos",
            r#"{"name":"macos","status":"running","ipAddress":"192.168.64.7",
                "sshAvailable":true,"cpuCount":4,"somethingNew":"ignored"}"#,
        )
        .unwrap();

        assert_eq!(info.status, "running");
        assert_eq!(info.ip_address.as_deref(), Some("192.168.64.7"));
        assert!(info.ssh_available);
        assert!(info.is_running());
    }

    #[test]
    fn an_address_without_sshd_is_not_somewhere_to_connect_yet() {
        // The window between the two is tens of seconds on a macOS guest, and
        // arriving in it gets the connection refused rather than retried.
        let info = parse_info(
            "macos",
            r#"{"name":"macos","status":"running","ipAddress":"192.168.64.7","sshAvailable":false}"#,
        )
        .unwrap();

        assert_eq!(info.reachable_address(), None);
    }

    #[test]
    fn a_guest_with_both_halves_reports_where_to_connect() {
        let info = parse_info(
            "macos",
            r#"{"name":"macos","status":"running","ipAddress":"192.168.64.7","sshAvailable":true}"#,
        )
        .unwrap();

        assert_eq!(info.reachable_address(), Some("192.168.64.7"));
    }

    #[test]
    fn a_stopped_guest_has_no_address_and_says_so() {
        let info = parse_info("macos", r#"{"name":"macos","status":"stopped"}"#).unwrap();

        assert_eq!(info.ip_address, None);
        assert!(!info.ssh_available);
        assert!(!info.is_running());
    }

    #[test]
    fn an_empty_address_reads_as_no_address_rather_than_as_one() {
        let info = parse_info(
            "macos",
            r#"{"name":"macos","status":"running","ipAddress":""}"#,
        )
        .unwrap();

        assert_eq!(info.ip_address, None);
    }

    #[test]
    fn something_that_is_not_json_names_the_vm_it_was_about() {
        let err = format!("{:#}", parse_info("macos", "No such VM").unwrap_err());

        assert!(err.contains("macos"), "{err}");
    }

    #[test]
    fn the_download_url_is_picked_out_of_lumes_chatter() {
        let output = "[2026-09-20T13:00:17Z] INFO: Fetching latest supported IPSW URL\n\
                      [2026-09-20T13:00:17Z] INFO: Found latest IPSW URL url=https://example.com/a.ipsw\n\
                      https://updates.cdn-apple.com/x/UniversalMac_26.6.2_25G83_Restore.ipsw\n";

        let url = parse_ipsw_url(output).unwrap();

        assert_eq!(
            url,
            "https://updates.cdn-apple.com/x/UniversalMac_26.6.2_25G83_Restore.ipsw"
        );
    }

    #[test]
    fn no_url_at_all_is_an_error_rather_than_an_empty_download() {
        let err = parse_ipsw_url("INFO: something went wrong\n")
            .unwrap_err()
            .to_string();

        assert!(err.contains("no download URL"), "{err}");
    }

    #[test]
    fn a_missing_binary_is_reported_with_its_path() {
        let err = format!(
            "{:#}",
            run(&PathBuf::from("/nowhere/lume"), &["ls".into()]).unwrap_err()
        );

        assert!(err.contains("/nowhere/lume"), "{err}");
    }
}
