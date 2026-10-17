//! Building `ssh` and `scp` command lines, and waiting for a guest to answer.
//!
//! Two rules here are the whole reason this is a module rather than a format
//! string. vitro passes `-F /dev/null` so no `ssh_config` on the host can reach
//! these connections — macOS ships `SendEnv LANG LC_*` system-wide, which
//! leaks a `setlocale` warning into every guest command's stderr, and a user's
//! `Host *` block can inject `ProxyCommand` or `ControlMaster` just as easily.
//! And readiness is the SSH handshake succeeding, never the forwarded TCP port
//! accepting: QEMU's user networking accepts before the guest exists.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

/// How long a single connection attempt may take while polling for readiness.
/// Short, because a guest that is not up yet should fail fast and be retried.
const PROBE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// What readiness is tested with.
///
/// `exit` rather than `true`, because the command runs in whatever shell the
/// guest gives an SSH session, and PowerShell — which is what a Windows guest
/// gives — has no `true`. It reports "the term `true` is not recognized", the
/// probe never succeeds, and a guest that is up and answering is waited on
/// until the timeout runs out.
///
/// One word, too: anything with a space in it gets quoted, and `'exit 0'` is a
/// command named "exit 0" rather than a command and an argument.
const PROBE_COMMAND: &str = "exit";
const PROBE_INTERVAL: Duration = Duration::from_secs(2);

/// How long a guest is allowed to go on refusing the key before that is taken
/// as the answer rather than as a stage of booting.
///
/// Not zero, because there is a real window where refusal is temporary: the
/// Windows first-logon script starts sshd and only then writes
/// `authorized_keys`, so a probe landing in between is answered and refused by
/// a guest that is seconds from being fine. That window is seconds; a golden
/// image built with a different key never stops refusing.
const AUTH_REFUSAL_GRACE: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub key: PathBuf,
    /// Per-VM, so a recycled forwarded port cannot raise a host-key mismatch.
    pub known_hosts: PathBuf,
}

impl SshTarget {
    pub fn destination(&self) -> String {
        format!("{}@{}", self.user, self.host)
    }
}

/// Whether the guest command gets a terminal.
///
/// A pty and piped input do not mix: the pty never delivers EOF, so a command
/// reading stdin never returns. Only ask for one when vitro's own stdin is a
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tty {
    No,
    Yes,
}

/// One `-L` forward: a port on the host standing in for a port in the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forward {
    pub host: u16,
    pub guest: u16,
}

/// How a command is written for the shell waiting at the other end.
///
/// sshd hands the remote command to that shell as one string, and the two
/// families disagree about what quoting means. A POSIX shell runs
/// `sh -c 'exit 42'`; PowerShell treats `'exit 42'` as a string expression and
/// prints it, which is how `vitro exec` came to echo commands at people instead
/// of running them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quoting {
    /// Quote each word so the guest's shell sees the argument list vitro meant.
    Posix,
    /// Join the words and let the guest's shell parse them. The caller is
    /// writing in that shell's own language.
    Raw,
}

impl From<crate::Guest> for Quoting {
    fn from(guest: crate::Guest) -> Self {
        match guest {
            crate::Guest::Unix => Quoting::Posix,
            crate::Guest::Windows => Quoting::Raw,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshOptions {
    pub tty: Tty,
    /// Fail rather than prompt. Wrong for an interactive session, right for
    /// everything vitro does on the user's behalf.
    pub batch: bool,
    pub connect_timeout: Option<Duration>,
    pub command: Vec<String>,
    pub quoting: Quoting,
    /// Ports to carry. A non-empty list also means `-N`: the connection exists
    /// only for the forwards, and ends when the user interrupts it.
    pub forwards: Vec<Forward>,
    pub prelude: Prelude,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            tty: Tty::No,
            batch: true,
            connect_timeout: None,
            command: Vec::new(),
            quoting: Quoting::Posix,
            forwards: Vec::new(),
            prelude: Prelude::default(),
        }
    }
}

/// Where the command runs and what it finds in its environment, written in
/// front of it in the guest shell's own syntax.
///
/// `Quoting::Raw` is only ever chosen for a Windows guest, whose shell is
/// PowerShell, so that is the syntax it gets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Prelude {
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// Rebuild PATH from the registry first. Windows sshd hands every session
    /// the PATH it started with, so without this a tool installed after boot
    /// stays invisible until the guest restarts. A POSIX login shell reads its
    /// profile on every connection, so there it means nothing.
    pub fresh_path: bool,
}

/// Read `NAME=VALUE`.
///
/// The name has to be one both a POSIX shell and PowerShell take as a bare
/// assignment target: it is written into the command unquoted, so anything
/// else would be a quoting hole rather than a variable.
pub fn parse_env(text: &str) -> Result<(String, String)> {
    let Some((name, value)) = text.split_once('=') else {
        bail!("{text:?} is not NAME=VALUE");
    };
    let valid = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        bail!("{name:?} is not a variable name: use letters, digits and `_`, not starting with a digit");
    }
    Ok((name.to_string(), value.to_string()))
}

pub fn ssh_args(target: &SshTarget, options: &SshOptions) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![OsString::from("-F"), OsString::from("/dev/null")];

    for option in base_options(target) {
        args.push(OsString::from("-o"));
        args.push(option);
    }
    if options.batch {
        args.push(OsString::from("-o"));
        args.push(OsString::from("BatchMode=yes"));
    }
    if let Some(timeout) = options.connect_timeout {
        args.push(OsString::from("-o"));
        args.push(OsString::from(format!(
            "ConnectTimeout={}",
            timeout.as_secs().max(1)
        )));
    }

    args.push(OsString::from("-i"));
    args.push(target.key.clone().into_os_string());
    args.push(OsString::from("-p"));
    args.push(OsString::from(target.port.to_string()));

    // `-t` alone is ignored when vitro's stdin is not a terminal, which is
    // exactly when a caller asking for one still means it.
    if options.tty == Tty::Yes {
        args.push(OsString::from("-tt"));
    }

    // Before the destination: OpenSSH does accept options after it, but every
    // other `ssh` does not, and the argument list reads as intended this way.
    if !options.forwards.is_empty() {
        args.push(OsString::from("-N"));
        for forward in &options.forwards {
            args.push(OsString::from("-L"));
            // The guest side is 127.0.0.1 because it is resolved in the guest.
            args.push(OsString::from(format!(
                "{}:127.0.0.1:{}",
                forward.host, forward.guest
            )));
        }
    }

    args.push(OsString::from(target.destination()));

    if !options.command.is_empty() {
        args.push(OsString::from("--"));
        args.push(OsString::from(compose(
            &options.command,
            options.quoting,
            &options.prelude,
        )));
    }

    args
}

/// Join a command into the one string sshd will hand to the guest's shell.
///
/// ssh's own argv boundaries do not survive the trip: sshd concatenates the
/// words with spaces and runs the result through a login shell. Passing
/// `sh -c "exit 42"` as three arguments therefore arrives as `sh -c exit 42`,
/// which exits 0. Quoting here puts the boundaries back.
fn shell_join(command: &[String], quoting: Quoting) -> String {
    match quoting {
        Quoting::Raw => command.join(" "),
        Quoting::Posix => command
            .iter()
            .map(|part| quote(part))
            .collect::<Vec<_>>()
            .join(" "),
    }
}

pub(crate) fn compose(command: &[String], quoting: Quoting, prelude: &Prelude) -> String {
    let command = shell_join(command, quoting);
    match quoting {
        Quoting::Posix => {
            let mut line = String::new();
            if let Some(cwd) = &prelude.cwd {
                line.push_str(&format!("cd {} && ", quote(cwd)));
            }
            // `NAME=value cmd` rather than `export`: the variables are for this
            // command, and the form needs no second statement.
            for (name, value) in &prelude.env {
                line.push_str(&format!("{name}={} ", quote(value)));
            }
            line + &command
        }
        Quoting::Raw => {
            let mut line = String::new();
            if prelude.fresh_path {
                line.push_str(
                    "$env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + \
                     [Environment]::GetEnvironmentVariable('Path','User'); ",
                );
            }
            if let Some(cwd) = &prelude.cwd {
                line.push_str(&format!(
                    "Set-Location -LiteralPath {} -ErrorAction Stop; ",
                    powershell_literal(cwd)
                ));
            }
            for (name, value) in &prelude.env {
                line.push_str(&format!("$env:{name}={}; ", powershell_literal(value)));
            }
            line + &command
        }
    }
}

/// A single-quoted PowerShell string, in which nothing expands and a quote is
/// escaped by doubling it.
pub(crate) fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(crate) fn quote(part: &str) -> String {
    const SAFE: &[u8] = b"-_./:=@,+";
    let bare = !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || SAFE.contains(&b));
    if bare {
        return part.to_string();
    }
    // A single quote cannot appear inside single quotes, so leave and re-enter.
    format!("'{}'", part.replace('\'', r"'\''"))
}

pub fn scp_args(target: &SshTarget, from: &Path, to_remote: &str) -> Vec<OsString> {
    let mut args = scp_options(target);
    args.push(from.to_path_buf().into_os_string());
    args.push(OsString::from(format!(
        "{}:{to_remote}",
        target.destination()
    )));
    args
}

/// One side of a copy: a path on the host, or a path in the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyEnd {
    Local(PathBuf),
    /// Relative to the guest account's home unless absolute.
    Guest(String),
}

/// `scp -r` from every source to the destination, directories included.
pub fn copy_args(target: &SshTarget, sources: &[CopyEnd], destination: &CopyEnd) -> Vec<OsString> {
    let mut args = scp_options(target);
    args.push(OsString::from("-r"));
    // The operands are the user's file names, and one that starts with `-`
    // would otherwise be taken as an option — `-oProxyCommand=` runs a command.
    args.push(OsString::from("--"));
    let end = |end: &CopyEnd| match end {
        CopyEnd::Local(path) => path.clone().into_os_string(),
        CopyEnd::Guest(path) => OsString::from(format!("{}:{path}", target.destination())),
    };
    args.extend(sources.iter().map(end));
    args.push(end(destination));
    args
}

fn scp_options(target: &SshTarget) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![OsString::from("-F"), OsString::from("/dev/null")];
    for option in base_options(target) {
        args.push(OsString::from("-o"));
        args.push(option);
    }
    args.push(OsString::from("-o"));
    args.push(OsString::from("BatchMode=yes"));
    args.push(OsString::from("-i"));
    args.push(target.key.clone().into_os_string());
    // scp spells the port flag with a capital P.
    args.push(OsString::from("-P"));
    args.push(OsString::from(target.port.to_string()));
    args
}

fn base_options(target: &SshTarget) -> Vec<OsString> {
    let mut known_hosts = OsString::from("UserKnownHostsFile=");
    known_hosts.push(target.known_hosts.as_os_str());
    vec![
        known_hosts,
        OsString::from("StrictHostKeyChecking=accept-new"),
        // Without this, an agent with many keys can exhaust the server's
        // authentication attempts before vitro's own key is offered.
        OsString::from("IdentitiesOnly=yes"),
        OsString::from("LogLevel=ERROR"),
    ]
}

/// Whether vitro's own stdin is a terminal, which decides whether asking for a
/// pty is safe.
pub fn stdin_is_terminal() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdin())
}

/// What is left of a budget after part of it has been spent.
///
/// A guest that takes two waits to become usable — lume reports an address and
/// a listening sshd separately — is spending one ceiling in two stages, not
/// being given the ceiling twice. `boot_timeout` is what the caller said the
/// whole thing may take, and without this a guest that never comes up costs
/// double that before anyone is told.
///
/// Never zero: whatever is left, the next stage gets one real attempt. A stage
/// that had already overrun would have failed rather than reaching here.
pub fn remaining(total: Duration, spent: Duration) -> Duration {
    total.saturating_sub(spent).max(Duration::from_secs(1))
}

/// Poll until the guest completes an SSH handshake, or the deadline passes.
///
/// `still_alive` is checked between attempts so a guest that died is reported
/// as dead rather than as a timeout — the distinction is the difference
/// between reading `serial.log` and blaming the network.
pub fn wait_until_reachable(
    ssh: &Path,
    target: &SshTarget,
    timeout: Duration,
    mut still_alive: impl FnMut() -> bool,
) -> Result<Duration> {
    let started = Instant::now();
    let options = SshOptions {
        connect_timeout: Some(PROBE_CONNECT_TIMEOUT),
        command: vec![PROBE_COMMAND.into()],
        ..SshOptions::default()
    };
    let args = ssh_args(target, &options);
    let mut refusing_since: Option<Instant> = None;

    loop {
        crate::signals::check()?;
        if !still_alive() {
            bail!("the guest stopped before it became reachable");
        }

        let output = Command::new(ssh)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            // Captured rather than discarded: what ssh says on stderr is the
            // only thing that separates "not up yet" from "up, and it will
            // never let this key in".
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("cannot run {}", ssh.display()))?;
        if output.status.success() {
            return Ok(started.elapsed());
        }

        let stderr = String::from_utf8_lossy(&output.stderr);
        if refuses_the_key(&stderr) {
            let since = *refusing_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= AUTH_REFUSAL_GRACE {
                bail!(
                    "{} is up but will not accept {}; \
                     the golden image was probably built with a different key",
                    target.destination(),
                    target.key.display()
                );
            }
        } else {
            // A guest mid-reboot goes back to refusing connections outright,
            // and that is not the same guest state as before.
            refusing_since = None;
        }

        if started.elapsed() >= timeout {
            // With what ssh last said: "no answer" on its own sends people to
            // the network, and the line underneath usually names the real
            // reason.
            bail!(
                "no SSH answer from {} after {}s{}",
                target.destination(),
                timeout.as_secs(),
                match last_line(&stderr) {
                    Some(line) => format!(" (ssh: {line})"),
                    None => String::new(),
                }
            );
        }
        std::thread::sleep(PROBE_INTERVAL);
    }
}

/// Whether sshd answered and turned the key down.
///
/// Distinguished from every other failure because waiting cannot fix it: the
/// guest is up, the port is open, and the handshake got far enough to be
/// rejected on credentials.
/// The last thing ssh said, for an error message that has to fit on one line.
///
/// The last rather than the first: `ssh -v` is not in play, so what is there is
/// at most a couple of lines, and the conclusion comes after the symptom.
fn last_line(stderr: &str) -> Option<String> {
    stderr
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

/// Whether sshd answered and turned the key down.
///
/// Distinguished from every other failure because waiting cannot fix it: the
/// guest is up, the port is open, and the handshake got far enough to be
/// rejected on credentials.
fn refuses_the_key(stderr: &str) -> bool {
    stderr.contains("Permission denied")
        || stderr.contains("Too many authentication failures")
        || stderr.contains("No supported authentication methods")
}

/// Run a command in the guest, passing stdio straight through.
///
/// Returns the guest command's exit code. Anything less is useless for the
/// job vitro exists to do: a failing build inside the VM has to fail outside
/// it too.
pub fn run_passthrough(ssh: &Path, target: &SshTarget, options: &SshOptions) -> Result<i32> {
    let status = Command::new(ssh)
        .args(ssh_args(target, options))
        .status()
        .with_context(|| format!("cannot run {}", ssh.display()))?;

    // A guest command killed by a signal has no exit code of its own; ssh
    // reports 255 for its own failures, so mirroring the shell convention here
    // keeps "non-zero" meaningful.
    Ok(status.code().unwrap_or(255))
}

/// Run a command in the guest and return its exit code and what it printed.
pub fn run_output(ssh: &Path, target: &SshTarget, options: &SshOptions) -> Result<(i32, String)> {
    let output = Command::new(ssh)
        .args(ssh_args(target, options))
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {}", ssh.display()))?;
    Ok((
        output.status.code().unwrap_or(255),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    ))
}

/// [`run_passthrough`] with nothing shown, for a probe whose exit code is the
/// whole answer.
pub fn run_quietly(ssh: &Path, target: &SshTarget, options: &SshOptions) -> Result<i32> {
    let status = Command::new(ssh)
        .args(ssh_args(target, options))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .with_context(|| format!("cannot run {}", ssh.display()))?;
    Ok(status.code().unwrap_or(255))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            host: "127.0.0.1".into(),
            port: 53422,
            user: "dev".into(),
            key: PathBuf::from("/home/dev/.config/vitro/id_ed25519"),
            known_hosts: PathBuf::from("/state/vms/linux-7f3a2c/known_hosts"),
        }
    }

    fn line(options: &SshOptions) -> String {
        ssh_args(&target(), options)
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn the_hosts_ssh_config_is_never_read() {
        assert!(line(&SshOptions::default()).contains("-F /dev/null"));
    }

    #[test]
    fn known_hosts_are_kept_per_vm() {
        // Forwarded ports get reused, and a shared known_hosts would then
        // report a host-key mismatch for an unrelated VM.
        assert!(line(&SshOptions::default())
            .contains("UserKnownHostsFile=/state/vms/linux-7f3a2c/known_hosts"));
    }

    #[test]
    fn only_vitros_own_key_is_offered() {
        let line = line(&SshOptions::default());

        assert!(line.contains("IdentitiesOnly=yes"), "{line}");
        assert!(
            line.contains("-i /home/dev/.config/vitro/id_ed25519"),
            "{line}"
        );
    }

    #[test]
    fn a_budget_spent_in_stages_is_not_handed_out_twice() {
        let total = Duration::from_secs(600);

        assert_eq!(remaining(total, Duration::ZERO), total);
        assert_eq!(
            remaining(total, Duration::from_secs(90)),
            Duration::from_secs(510)
        );
    }

    #[test]
    fn an_exhausted_budget_still_buys_one_attempt() {
        // Better a single try than a wait that reports failure without ever
        // having asked.
        let left = remaining(Duration::from_secs(600), Duration::from_secs(9000));

        assert_eq!(left, Duration::from_secs(1));
    }

    #[test]
    fn the_readiness_probe_survives_being_quoted() {
        // It goes through the same quoting as any other command, so anything
        // with a space in it arrives as a command *named* "exit 0" and fails
        // with 127 on a guest that is up and answering.
        assert_eq!(quote(PROBE_COMMAND), PROBE_COMMAND);
        assert!(!PROBE_COMMAND.contains(' '), "{PROBE_COMMAND:?}");
    }

    #[test]
    fn the_readiness_probe_runs_in_a_windows_shell_as_well_as_a_unix_one() {
        // PowerShell has no `true`, so a probe using it never succeeds against
        // a Windows guest no matter how ready the guest is. `exit` is a
        // builtin in both.
        assert_eq!(PROBE_COMMAND, "exit");
    }

    #[test]
    fn a_refused_key_is_told_apart_from_a_guest_that_is_not_up() {
        // Waiting fixes one of these and never fixes the other, and the wait is
        // ten minutes long.
        for refusal in [
            "dev@127.0.0.1: Permission denied (publickey,keyboard-interactive).",
            "Received disconnect from 127.0.0.1 port 53422:2: Too many authentication failures",
            "No supported authentication methods available (server sent: publickey)",
        ] {
            assert!(refuses_the_key(refusal), "{refusal}");
        }

        for still_booting in [
            "ssh: connect to host 127.0.0.1 port 53422: Connection refused",
            "ssh: connect to host 127.0.0.1 port 53422: Operation timed out",
            "kex_exchange_identification: Connection closed by remote host",
            "banner exchange: Connection to 127.0.0.1 port 53422: invalid format",
            "",
        ] {
            assert!(!refuses_the_key(still_booting), "{still_booting}");
        }
    }

    #[test]
    fn a_command_is_separated_from_the_destination() {
        let line = line(&SshOptions {
            command: vec!["uname".into(), "-a".into()],
            ..SshOptions::default()
        });

        assert!(line.ends_with("dev@127.0.0.1 -- uname -a"), "{line}");
    }

    #[test]
    fn a_command_keeps_its_word_boundaries_through_the_guests_shell() {
        let line = line(&SshOptions {
            command: vec!["sh".into(), "-c".into(), "exit 42".into()],
            ..SshOptions::default()
        });

        assert!(line.ends_with("-- sh -c 'exit 42'"), "{line}");
    }

    #[test]
    fn a_powershell_guest_is_not_handed_posix_quoting() {
        // PowerShell reads 'exit 42' as a string expression and prints it, so
        // quoting the way a POSIX shell wants turns `exec` into an echo.
        let line = line(&SshOptions {
            command: vec!["exit 42".into()],
            quoting: Quoting::Raw,
            ..SshOptions::default()
        });

        assert!(line.ends_with("-- exit 42"), "{line}");
    }

    fn prelude() -> Prelude {
        Prelude {
            cwd: Some("my src".into()),
            env: vec![
                ("RUST_BACKTRACE".into(), "1".into()),
                ("GREETING".into(), "it's here".into()),
            ],
            fresh_path: false,
        }
    }

    #[test]
    fn a_powershell_guest_can_be_given_the_path_it_has_now_before_anything_else() {
        // Before the variables, so an explicit --env PATH=... still wins.
        let line = line(&SshOptions {
            command: vec!["cargo".into(), "build".into()],
            quoting: Quoting::Raw,
            prelude: Prelude {
                env: vec![("A".into(), "1".into())],
                fresh_path: true,
                ..Prelude::default()
            },
            ..SshOptions::default()
        });

        assert!(
            line.ends_with(
                "-- $env:Path = [Environment]::GetEnvironmentVariable('Path','Machine') + ';' + \
                 [Environment]::GetEnvironmentVariable('Path','User'); $env:A='1'; cargo build"
            ),
            "{line}"
        );
    }

    #[test]
    fn a_posix_guest_needs_no_path_refresh() {
        // A POSIX login shell reads its profile on every connection already.
        let line = line(&SshOptions {
            command: vec!["make".into()],
            prelude: Prelude {
                fresh_path: true,
                ..Prelude::default()
            },
            ..SshOptions::default()
        });

        assert!(line.ends_with("-- make"), "{line}");
    }

    #[test]
    fn a_posix_guest_changes_directory_and_sets_variables_in_its_own_syntax() {
        let line = line(&SshOptions {
            command: vec!["cargo".into(), "test".into()],
            prelude: prelude(),
            ..SshOptions::default()
        });

        assert!(
            line.ends_with(
                r"-- cd 'my src' && RUST_BACKTRACE=1 GREETING='it'\''s here' cargo test"
            ),
            "{line}"
        );
    }

    #[test]
    fn a_powershell_guest_changes_directory_and_sets_variables_in_its_own_syntax() {
        // A failed Set-Location has to stop the line, or the command runs in
        // the home directory and reports on the wrong tree.
        let line = line(&SshOptions {
            command: vec!["cargo".into(), "test".into()],
            quoting: Quoting::Raw,
            prelude: prelude(),
            ..SshOptions::default()
        });

        assert!(
            line.ends_with(
                "-- Set-Location -LiteralPath 'my src' -ErrorAction Stop; \
                 $env:RUST_BACKTRACE='1'; $env:GREETING='it''s here'; cargo test"
            ),
            "{line}"
        );
    }

    #[test]
    fn a_variable_is_given_as_name_equals_value() {
        assert_eq!(
            parse_env("A_1=x=y").unwrap(),
            ("A_1".to_string(), "x=y".to_string())
        );
        assert_eq!(parse_env("EMPTY=").unwrap(), ("EMPTY".into(), "".into()));
    }

    #[test]
    fn a_variable_name_neither_shell_could_assign_is_refused() {
        for bad in ["NOVALUE", "=x", "1A=x", "A-B=x", "A B=x", "$A=x"] {
            assert!(parse_env(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_quoting_follows_the_guest() {
        assert_eq!(Quoting::from(crate::Guest::Unix), Quoting::Posix);
        assert_eq!(Quoting::from(crate::Guest::Windows), Quoting::Raw);
    }

    #[test]
    fn quoting_covers_what_a_guest_shell_would_otherwise_eat() {
        assert_eq!(quote("plain-arg_1.txt"), "plain-arg_1.txt");
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("$HOME"), "'$HOME'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote(""), "''");
    }

    #[test]
    fn no_terminal_is_requested_by_default() {
        assert!(!line(&SshOptions::default()).contains("-tt"));
    }

    #[test]
    fn asking_for_a_terminal_uses_the_forcing_form() {
        // Plain `-t` is ignored when vitro's stdin is not a terminal.
        let line = line(&SshOptions {
            tty: Tty::Yes,
            ..SshOptions::default()
        });

        assert!(line.contains("-tt"), "{line}");
    }

    #[test]
    fn an_interactive_session_does_not_run_in_batch_mode() {
        let line = line(&SshOptions {
            tty: Tty::Yes,
            batch: false,
            ..SshOptions::default()
        });

        assert!(!line.contains("BatchMode"), "{line}");
    }

    #[test]
    fn a_connect_timeout_is_never_rounded_down_to_zero() {
        let line = line(&SshOptions {
            connect_timeout: Some(Duration::from_millis(400)),
            ..SshOptions::default()
        });

        assert!(line.contains("ConnectTimeout=1"), "{line}");
    }

    #[test]
    fn scp_uses_the_capital_port_flag_and_a_remote_destination() {
        let args = scp_args(
            &target(),
            Path::new("/tmp/provision.sh"),
            "/tmp/provision.sh",
        );
        let line = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(line.contains("-P 53422"), "{line}");
        assert!(
            line.ends_with("/tmp/provision.sh dev@127.0.0.1:/tmp/provision.sh"),
            "{line}"
        );
        assert!(line.contains("-F /dev/null"), "{line}");
    }

    #[test]
    fn a_copy_is_recursive_and_names_the_guest_side_with_the_destination() {
        let target = target();
        let args = copy_args(
            &target,
            &[
                CopyEnd::Local(PathBuf::from("src")),
                CopyEnd::Local(PathBuf::from("Cargo.toml")),
            ],
            &CopyEnd::Guest("project".into()),
        );
        let line = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");

        assert!(line.contains("-r"), "{line}");
        assert!(line.contains("-P 53422"), "{line}");
        assert!(line.contains("-F /dev/null"), "{line}");
        assert!(
            line.ends_with("-- src Cargo.toml dev@127.0.0.1:project"),
            "{line}"
        );
    }

    #[test]
    fn a_source_named_like_an_option_stays_a_file_name() {
        // `-oProxyCommand=…` read as an option would run a command on the host.
        let args = copy_args(
            &target(),
            &[CopyEnd::Local(PathBuf::from("-oProxyCommand=touch pwned"))],
            &CopyEnd::Guest(String::new()),
        );

        let separator = args.iter().position(|a| a == "--").expect("a separator");
        let source = args
            .iter()
            .position(|a| a == "-oProxyCommand=touch pwned")
            .unwrap();
        assert!(separator < source, "{args:?}");
    }

    #[cfg(unix)]
    #[test]
    fn waiting_gives_up_when_the_guest_dies_rather_than_running_out_the_clock() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(temp.path(), 255);

        let err = wait_until_reachable(&ssh, &target(), Duration::from_secs(60), || false)
            .unwrap_err()
            .to_string();

        assert!(err.contains("stopped before"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn waiting_returns_as_soon_as_the_handshake_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(temp.path(), 0);

        let elapsed =
            wait_until_reachable(&ssh, &target(), Duration::from_secs(60), || true).unwrap();

        assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    }

    #[cfg(unix)]
    #[test]
    fn waiting_reports_the_target_when_the_deadline_passes() {
        let temp = tempfile::tempdir().unwrap();
        let ssh = fake_ssh(temp.path(), 1);

        let err = wait_until_reachable(&ssh, &target(), Duration::from_millis(1), || true)
            .unwrap_err()
            .to_string();

        assert!(err.contains("dev@127.0.0.1"), "{err}");
    }

    #[cfg(unix)]
    fn fake_ssh(dir: &Path, exit_code: i32) -> PathBuf {
        let path = dir.join("ssh");
        crate::testing::write_script(&path, &format!("exit {exit_code}"));
        path
    }
}
