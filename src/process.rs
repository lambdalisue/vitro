//! Stopping processes vitro started but cannot `wait` for.
//!
//! QEMU is started with `-daemonize`, so it is not a child of this process and
//! the usual `Child::kill` is unavailable. Everything here works from a PID,
//! which is why every caller must first confirm the PID's start time still
//! matches the record.

use std::time::{Duration, Instant};

use crate::ProcessProbe;

const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// One budget shared by every stage of a macOS image build.
#[derive(Clone, Copy)]
pub struct Deadline {
    started: Instant,
    timeout: Duration,
    label: &'static str,
    interruptible: bool,
}

impl Deadline {
    pub fn new(timeout: Duration, label: &'static str) -> Self {
        Self {
            started: Instant::now(),
            timeout,
            label,
            interruptible: true,
        }
    }

    pub fn uninterruptible(mut self) -> Self {
        self.interruptible = false;
        self
    }

    pub fn remaining(&self) -> anyhow::Result<Duration> {
        if self.interruptible {
            crate::signals::check()?;
        }
        self.timeout
            .checked_sub(self.started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| anyhow::anyhow!("{} ({:?}) exceeded", self.label, self.timeout))
    }

    pub fn status(
        &self,
        command: &mut std::process::Command,
    ) -> anyhow::Result<std::process::ExitStatus> {
        use anyhow::Context;
        self.remaining()?;
        // Killing the shell alone leaves its installer or SSH child running.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot run {:?}", command.get_program()))?;
        loop {
            let budget = self.remaining();
            match budget {
                Err(error) => {
                    #[cfg(unix)]
                    unsafe {
                        libc_kill(-(child.id() as i32), 9);
                    }
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
                Ok(left) => {
                    if let Some(status) = child.try_wait()? {
                        return Ok(status);
                    }
                    std::thread::sleep(left.min(Duration::from_millis(10)));
                }
            }
        }
    }

    pub fn output(
        &self,
        command: &mut std::process::Command,
    ) -> anyhow::Result<std::process::Output> {
        use std::io::{Read, Seek};
        // Files cannot fill a pipe while the parent waits for the deadline.
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        command
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?);
        let status = self.status(command)?;
        stdout.rewind()?;
        stderr.rewind()?;
        let mut output = std::process::Output {
            status,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        stdout.read_to_end(&mut output.stdout)?;
        stderr.read_to_end(&mut output.stderr)?;
        Ok(output)
    }
}

/// Ask a process to exit. Best effort: a process that is already gone is not
/// an error, because that is the outcome the caller wanted.
pub fn terminate(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc_kill(pid as i32, 15);
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .output();
    }
}

/// Interrupt a process the way Ctrl-C would. What lume answers by shutting the
/// guest it holds down cleanly.
pub fn interrupt(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc_kill(pid as i32, 2);
    }
    #[cfg(not(unix))]
    terminate(pid);
}

/// Stop insisting.
pub fn kill(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc_kill(pid as i32, 9);
    }
    #[cfg(not(unix))]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    }
}

/// Wait for a process to disappear, re-probing as it goes.
///
/// Returns whether it is gone. The probe is rebuilt on each poll because a
/// cached process table would report a corpse as alive forever.
///
/// An interrupted wait gives up immediately and reports the process as still
/// present, which is the truth. These waits run to three minutes, and a Ctrl-C
/// that only takes effect after them is one the user has to sit through.
pub fn wait_for_exit(timeout: Duration, probe: impl FnMut() -> bool) -> bool {
    wait(timeout, probe, true)
}

/// [`wait_for_exit`] that a Ctrl-C does not cut short, for a stop that has
/// already been asked for and must be seen through before anything is deleted.
pub fn wait_until_gone(timeout: Duration, probe: impl FnMut() -> bool) -> bool {
    wait(timeout, probe, false)
}

fn wait(timeout: Duration, mut probe: impl FnMut() -> bool, interruptible: bool) -> bool {
    let started = Instant::now();
    loop {
        if !probe() {
            return true;
        }
        if started.elapsed() >= timeout || (interruptible && crate::signals::interrupted()) {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// True while a process with this PID and start time is still running.
pub fn is_running(probe: &impl ProcessProbe, pid: u32, start_time: u64) -> bool {
    probe.start_time(pid) == Some(start_time)
}

/// Ask a process to stop, then insist, and report whether it actually did.
///
/// Callers that are about to delete the record naming this PID must check the
/// answer: once the record is gone, nothing can find the process again, so a
/// surviving QEMU would hold its forwarded port and its deleted overlay with
/// no way left to stop it.
///
/// `start_time` guards against a recycled PID. It is `None` only in the narrow
/// window before the start time has been read, where the PID has just come
/// from QEMU and cannot yet have been reused.
pub fn stop_and_wait(pid: u32, start_time: Option<u64>) -> bool {
    let alive = move || match start_time {
        Some(start) => is_running(&crate::SysinfoProbe::new(), pid, start),
        None => crate::SysinfoProbe::new().start_time(pid).is_some(),
    };

    terminate(pid);
    if wait_for_exit(TERM_TIMEOUT, alive) {
        return true;
    }
    kill(pid);
    // Not cut short by Ctrl-C, unlike the wait above: a killed process is
    // gone within moments, and `run --rm` cleans up right after an interrupt,
    // where giving up at once would report a VM that is dying as one that
    // could not be stopped and leave its record behind.
    wait(KILL_TIMEOUT, alive, false)
}

/// How long a process gets between being asked to stop and being made to.
const TERM_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a killed process gets to disappear from the process table.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, signal: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, signal);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct FakeProbe(HashMap<u32, u64>);

    impl ProcessProbe for FakeProbe {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).copied()
        }
    }

    #[test]
    fn an_expired_deadline_does_not_start_a_command() {
        let deadline = Deadline::new(Duration::ZERO, "test deadline");
        let error = deadline
            .status(&mut std::process::Command::new(
                "nonexistent-deadline-command",
            ))
            .unwrap_err();
        assert!(error.to_string().contains("test deadline"), "{error:#}");
    }

    #[cfg(unix)]
    #[test]
    fn a_deadline_terminates_descendants_as_well_as_the_shell() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("survived");
        let deadline = Deadline::new(Duration::from_millis(100), "test deadline");
        let error = deadline
            .status(
                std::process::Command::new("/bin/sh")
                    .args(["-c", "(sleep 0.4; touch \"$1\") & wait", "sh"])
                    .arg(&marker),
            )
            .unwrap_err();
        assert!(error.to_string().contains("test deadline"), "{error:#}");
        std::thread::sleep(Duration::from_millis(500));
        assert!(!marker.exists(), "a descendant survived the deadline");
    }

    #[test]
    fn a_process_matches_only_when_both_the_pid_and_the_start_time_agree() {
        let probe = FakeProbe(HashMap::from([(42, 1_000)]));

        assert!(is_running(&probe, 42, 1_000));
        assert!(!is_running(&probe, 42, 1_001), "a recycled PID is not ours");
        assert!(!is_running(&probe, 43, 1_000));
    }

    #[test]
    fn waiting_returns_at_once_when_the_process_is_already_gone() {
        let started = Instant::now();

        assert!(wait_for_exit(Duration::from_secs(30), || false));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn waiting_gives_up_when_the_process_will_not_die() {
        assert!(!wait_for_exit(Duration::from_millis(1), || true));
    }

    #[test]
    fn waiting_notices_an_exit_partway_through() {
        let mut polls = 0;

        let gone = wait_for_exit(Duration::from_secs(5), || {
            polls += 1;
            polls < 3
        });

        assert!(gone);
        assert_eq!(polls, 3);
    }

    #[test]
    fn stopping_a_process_that_is_already_gone_reports_success() {
        // The caller is about to delete the record naming this PID, so a
        // false negative here would strand a VM directory forever.
        assert!(stop_and_wait(4_000_000, Some(1_000)));
    }

    #[cfg(unix)]
    #[test]
    fn terminating_a_process_that_does_not_exist_is_not_an_error() {
        // PID 0 addresses a process group on Unix; a very high PID is simply
        // absent, which is the case that matters here.
        terminate(4_000_000);
        kill(4_000_000);
    }
}
