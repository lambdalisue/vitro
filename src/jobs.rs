//! Commands that run in the guest to start, watch and list detached jobs.
//!
//! A job is a command left running in the guest after vitro's connection
//! closes, so a build that takes an hour survives a dropped SSH session and
//! can be watched from another. Everything about a job lives in a directory of
//! its own under the guest account's home — the command, its output, its
//! process and, once it ends, its exit code — so any later connection can tell
//! what became of it.

use std::fmt;

use anyhow::{bail, Result};

use crate::ssh::{self, Prelude, Quoting};
use crate::Guest;

/// Where jobs live, relative to the guest account's home.
const JOBS_DIR: &str = ".vitro-jobs";

/// How a job stands, as the guest reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Running,
    Exited(i32),
    /// The process is gone and left no exit code: the guest restarted, or the
    /// job was killed.
    Lost,
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Status::Running => write!(f, "running"),
            Status::Exited(code) => write!(f, "exit {code}"),
            Status::Lost => write!(f, "lost"),
        }
    }
}

/// A job's address: the VM it runs in and its id there, written `<vm>:<id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRef {
    /// `None` for `:<id>`, which means the one running VM.
    pub vm: Option<String>,
    pub id: String,
}

impl JobRef {
    pub fn parse(text: &str) -> Result<Self> {
        let Some((vm, id)) = text.rsplit_once(':') else {
            bail!("{text:?} is not a job; jobs are written <vm>:<id>, as `exec --detach` printed");
        };
        if !valid_id(id) {
            bail!("{id:?} is not a job id");
        }
        Ok(Self {
            vm: (!vm.is_empty()).then(|| vm.to_string()),
            id: id.to_string(),
        })
    }
}

/// Ids are written into guest commands unquoted, so only these characters.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_hexdigit())
}

pub fn new_id() -> String {
    format!("{:06x}", rand::random::<u32>() % 0x100_0000)
}

/// The command that starts `command` as job `id` and prints its process id.
pub fn start(guest: Guest, id: &str, command: &[String], prelude: &Prelude) -> Vec<String> {
    match guest {
        Guest::Unix => {
            let line = ssh::compose(command, Quoting::Posix, prelude);
            let dir = format!("\"$HOME/{JOBS_DIR}/{id}\"");
            // The inner shell writes the exit code only after the command has
            // finished writing its output, so a reader that sees the code has
            // the whole log.
            let job = format!("( {line} ) > {dir}/log 2>&1 < /dev/null; echo $? > {dir}/exit");
            vec![
                "sh".into(),
                "-c".into(),
                // The directory first, as a statement of its own: `a && b &`
                // would put the whole chain in the background, and the pid
                // file could be written before its directory exists.
                format!(
                    "mkdir -p {dir} && printf '%s\\n' {cmd} > {dir}/cmd || exit 1; \
                     nohup sh -c {job} > /dev/null 2>&1 < /dev/null & \
                     echo $! > {dir}/pid; cat {dir}/pid",
                    cmd = ssh::quote(&command.join(" ")),
                    job = ssh::quote(&job),
                ),
            ]
        }
        Guest::Windows => {
            let line = ssh::compose(command, Quoting::Raw, prelude);
            let text = command.join(" ");
            // A process started from the SSH session ends with it, so the job
            // is created through WMI, outside the session. It runs a script
            // that writes the exit code last, for the same reason as on Unix.
            let script = format!(
                "{line}\n\
                 $code = $LASTEXITCODE; if ($null -eq $code) {{ $code = [int](-not $?) }}\n\
                 Set-Content -LiteralPath (Join-Path $PSScriptRoot 'exit') -Value $code -Encoding ascii\n"
            );
            vec![format!(
                "$d = Join-Path $HOME '{JOBS_DIR}\\{id}'; \
                 New-Item -ItemType Directory -Force -Path $d | Out-Null; \
                 Set-Content -LiteralPath (Join-Path $d 'cmd') -Value {text} -Encoding utf8; \
                 Set-Content -LiteralPath (Join-Path $d 'run.ps1') -Value {script} -Encoding utf8; \
                 $line = 'cmd /c powershell -NoProfile -ExecutionPolicy Bypass -File \"' + (Join-Path $d 'run.ps1') + '\" > \"' + (Join-Path $d 'log') + '\" 2>&1'; \
                 $p = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{{ CommandLine = $line; CurrentDirectory = $HOME }}; \
                 if ($p.ReturnValue -ne 0) {{ exit 1 }}; \
                 Set-Content -LiteralPath (Join-Path $d 'pid') -Value $p.ProcessId -Encoding ascii; \
                 $p.ProcessId",
                text = ssh::powershell_literal(&text),
                script = ssh::powershell_literal(&script),
            )]
        }
    }
}

/// The command that prints job `id`'s status on the first line, then its
/// output from byte `offset` on, in base64 so nothing is lost on the way.
///
/// Status first: the exit code is written after the output is complete, so a
/// status of "exited" read before the output guarantees the output is whole.
pub fn poll(guest: Guest, id: &str, offset: u64) -> Vec<String> {
    match guest {
        Guest::Unix => vec![
            "sh".into(),
            "-c".into(),
            format!(
                "d=\"$HOME/{JOBS_DIR}/{id}\"; [ -d \"$d\" ] || exit 3; {status}; \
                 tail -c +{from} \"$d/log\" 2>/dev/null | base64",
                status = unix_status("$d"),
                from = offset + 1,
            ),
        ],
        Guest::Windows => vec![format!(
            "$d = Join-Path $HOME '{JOBS_DIR}\\{id}'; if (-not (Test-Path $d)) {{ exit 3 }}; {status}; \
             $log = Join-Path $d 'log'; \
             if (Test-Path $log) {{ \
               $s = [IO.File]::Open($log, 'Open', 'Read', 'ReadWrite'); \
               [void]$s.Seek({offset}, 'Begin'); $m = New-Object IO.MemoryStream; $s.CopyTo($m); $s.Close(); \
               [Convert]::ToBase64String($m.ToArray()) }}",
            status = windows_status(),
        )],
    }
}

/// The command that lists every job: id, status and command, tab-separated.
pub fn list(guest: Guest) -> Vec<String> {
    match guest {
        Guest::Unix => vec![
            "sh".into(),
            "-c".into(),
            format!(
                "for d in \"$HOME/{JOBS_DIR}\"/*/; do [ -d \"$d\" ] || continue; \
                 printf '%s\\t' \"$(basename \"$d\")\"; {status} | tr -d '\\n'; \
                 printf '\\t%s\\n' \"$(head -n 1 \"$d/cmd\" 2>/dev/null)\"; done",
                status = unix_status("$d"),
            ),
        ],
        Guest::Windows => vec![format!(
            "Get-ChildItem -Directory -Path (Join-Path $HOME '{JOBS_DIR}') -ErrorAction SilentlyContinue | \
             ForEach-Object {{ $d = $_.FullName; $st = & {{ {status} }}; \
             $c = Get-Content -LiteralPath (Join-Path $d 'cmd') -TotalCount 1 -ErrorAction SilentlyContinue; \
             \"$($_.Name)`t$st`t$c\" }}",
            status = windows_status(),
        )],
    }
}

/// Prints `exit:<code>`, `running` or `lost` for the job directory `$d`.
fn unix_status(dir: &str) -> String {
    format!(
        "if [ -f {dir}/exit ]; then printf 'exit:%s\\n' \"$(cat {dir}/exit)\"; \
         elif kill -0 \"$(cat {dir}/pid 2>/dev/null)\" 2>/dev/null; then echo running; \
         else echo lost; fi"
    )
}

/// The same for Windows, reading the directory from `$d`.
fn windows_status() -> String {
    "$e = Join-Path $d 'exit'; \
     if (Test-Path $e) { 'exit:' + (Get-Content -LiteralPath $e -TotalCount 1).Trim() } \
     elseif (Get-Process -Id ([int](Get-Content -LiteralPath (Join-Path $d 'pid') -ErrorAction SilentlyContinue)) -ErrorAction SilentlyContinue) { 'running' } \
     else { 'lost' }"
        .to_string()
}

pub fn parse_status(token: &str) -> Option<Status> {
    match token.trim() {
        "running" => Some(Status::Running),
        "lost" => Some(Status::Lost),
        other => other
            .strip_prefix("exit:")
            .and_then(|code| code.trim().parse().ok())
            .map(Status::Exited),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn a_job_is_written_vm_colon_id_and_a_bare_colon_means_the_only_vm() {
        assert_eq!(
            JobRef::parse("linux-7f3a2c:a3f9c2").unwrap(),
            JobRef {
                vm: Some("linux-7f3a2c".into()),
                id: "a3f9c2".into()
            }
        );
        assert_eq!(JobRef::parse(":a3f9c2").unwrap().vm, None);
    }

    #[test]
    fn anything_else_is_not_a_job() {
        // The id goes into guest commands unquoted, so it is held to hex.
        for bad in ["a3f9c2", "linux:", "linux:a3f; rm -rf ~", "linux:../x"] {
            assert!(JobRef::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn new_ids_are_valid_ids() {
        assert!(valid_id(&new_id()));
    }

    #[test]
    fn a_status_reads_as_the_guest_writes_it() {
        assert_eq!(parse_status("running\n"), Some(Status::Running));
        assert_eq!(parse_status("lost"), Some(Status::Lost));
        assert_eq!(parse_status("exit:101"), Some(Status::Exited(101)));
        assert_eq!(parse_status("exit: 0\r"), Some(Status::Exited(0)));
        assert_eq!(parse_status("exit:x"), None);
        assert_eq!(parse_status("weird"), None);
    }

    #[test]
    fn a_unix_job_outlives_the_connection_and_writes_its_exit_code_after_its_output() {
        let line = start(
            Guest::Unix,
            "a3f9c2",
            &words("cargo build"),
            &Prelude::default(),
        )
        .join(" ");

        assert!(line.contains("nohup sh -c"), "{line}");
        assert!(line.contains(".vitro-jobs/a3f9c2"), "{line}");
        let log = line.find("/log 2>&1").expect("output to the log");
        let exit = line.find("echo $? >").expect("exit code recorded");
        assert!(log < exit, "{line}");
        assert!(line.contains("echo $! >"), "{line}");
    }

    #[test]
    fn a_unix_job_directory_exists_before_anything_is_backgrounded() {
        // `mkdir … && … &` backgrounds the mkdir too, and the pid file was then
        // written into a directory that did not exist yet.
        let line = start(Guest::Unix, "a3f9c2", &words("make"), &Prelude::default()).join(" ");

        let made = line
            .find("|| exit 1;")
            .expect("the directory is its own statement");
        let backgrounded = line.find("nohup").expect("the job");
        assert!(made < backgrounded, "{line}");
    }

    #[test]
    fn a_windows_job_is_created_outside_the_ssh_session() {
        // A process started from the session dies with it.
        let line = start(
            Guest::Windows,
            "a3f9c2",
            &words("cargo build"),
            &Prelude::default(),
        )
        .join(" ");

        assert!(
            line.contains("Invoke-CimMethod -ClassName Win32_Process"),
            "{line}"
        );
        assert!(line.contains("run.ps1"), "{line}");
        assert!(line.contains("$LASTEXITCODE"), "{line}");
    }

    #[test]
    fn the_prelude_reaches_a_detached_command() {
        let prelude = Prelude {
            cwd: Some("proj".into()),
            ..Prelude::default()
        };

        let line = start(Guest::Unix, "a3f9c2", &words("make"), &prelude).join(" ");

        assert!(line.contains("cd proj"), "{line}");
    }

    #[test]
    fn polling_reports_the_status_before_the_output_and_reads_from_the_offset() {
        let line = poll(Guest::Unix, "a3f9c2", 100).join(" ");

        let status = line.find("exit:").expect("status");
        let output = line.find("tail -c +101").expect("output from byte 100");
        assert!(status < output, "{line}");
        assert!(line.contains("base64"), "{line}");

        let line = poll(Guest::Windows, "a3f9c2", 100).join(" ");
        assert!(line.contains("Seek(100"), "{line}");
        assert!(line.contains("ToBase64String"), "{line}");
    }
}
