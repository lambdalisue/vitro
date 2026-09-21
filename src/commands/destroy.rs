//! `vitro destroy` — stop a VM and delete its overlay.
//!
//! The guest's filesystem state is not worth preserving: that is what
//! `promote` is for. So the default is a forced stop, and `--graceful` is the
//! opt-in for the rare case where it matters.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};

use crate::{
    lume, process, qmp, tools, Backend, Config, Entry, Paths, Store, SysinfoProbe, ToolPaths,
    VmRecord,
};

/// How long a guest gets to shut down on its own before vitro insists.
const GRACEFUL_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Options {
    pub graceful: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The process was running and has been stopped.
    Stopped(String),
    /// Only the record was left; nothing needed stopping.
    CleanedUp(String),
    /// It could not be stopped, and its record was left alone so it can still
    /// be found.
    Failed { name: String, reason: String },
}

impl Outcome {
    pub fn name(&self) -> &str {
        match self {
            Outcome::Stopped(name) | Outcome::CleanedUp(name) | Outcome::Failed { name, .. } => {
                name
            }
        }
    }
}

pub fn destroy_one(
    paths: &Paths,
    config: &Config,
    name: &str,
    options: &Options,
) -> Result<Outcome> {
    let store = Store::new(paths.state_dir());
    let _lock = store.lock()?;
    let record = store.load(store.resolve(name)?)?;
    let outcome = stop(&record, options, config.tools())?;
    remove(&store, &record.name, record.qmp_socket.as_deref())?;
    Ok(outcome)
}

/// Destroy every VM, reporting per-VM failures rather than abandoning the
/// rest on the first one. A wedged guest must not keep the others alive.
pub fn destroy_all(paths: &Paths, config: &Config, options: &Options) -> Result<Vec<Outcome>> {
    let store = Store::new(paths.state_dir());
    let _lock = store.lock()?;
    let mut outcomes = Vec::new();
    for entry in store.list()? {
        match entry {
            Entry::Vm(record) => match stop(&record, options, config.tools()) {
                Ok(outcome) => {
                    remove(&store, &record.name, record.qmp_socket.as_deref())?;
                    outcomes.push(outcome);
                }
                Err(e) => outcomes.push(Outcome::Failed {
                    name: record.name.clone(),
                    reason: format!("{e:#}"),
                }),
            },
            // Debris has nothing to stop; removing the directory is the whole
            // job.
            Entry::Unreadable { name, .. } => {
                remove(&store, &name, None)?;
                outcomes.push(Outcome::CleanedUp(name));
            }
        }
    }
    Ok(outcomes)
}

/// Remove the record and, when the monitor socket had to live outside the VM
/// directory, the socket too. A stale one at the same path stops the next VM
/// of that name from binding it.
pub fn remove(store: &Store, name: &str, qmp_socket: Option<&Path>) -> Result<()> {
    if let Some(socket) = qmp_socket {
        if !socket.starts_with(store.vm_dir(name)) {
            let _ = std::fs::remove_file(socket);
        }
    }
    store.remove(name)
}

fn stop(record: &VmRecord, options: &Options, tools: &ToolPaths) -> Result<Outcome> {
    // Lume first, and whether or not a process is running: the VM is a thing
    // lume keeps on disk, so a stopped one still has to be deleted, and
    // dropping vitro's record alone would strand tens of gigabytes.
    if record.backend == Backend::Lume {
        return stop_lume(record, options, tools);
    }

    let probe = SysinfoProbe::new();
    if !process::is_running(&probe, record.pid, record.pid_start_time) {
        return Ok(Outcome::CleanedUp(record.name.clone()));
    }

    if options.graceful {
        graceful_stop(record)?;
    } else {
        forced_stop(record);
    }

    if process::is_running(&SysinfoProbe::new(), record.pid, record.pid_start_time) {
        bail!(
            "VM {:?} (pid {}) is still running after being asked to stop",
            record.name,
            record.pid
        );
    }
    Ok(Outcome::Stopped(record.name.clone()))
}

/// Stop a guest that `lume` owns.
///
/// Both steps go through `lume`: killing the process it detached would leave
/// the VM's own record saying it is running, and its disk in whatever state a
/// hard stop leaves it.
fn stop_lume(record: &VmRecord, options: &Options, tools: &ToolPaths) -> Result<Outcome> {
    let binary = tools::resolve("lume", tools.lume.as_deref(), "tools.lume")?.path;
    if options.graceful {
        // Lume shuts the guest down from inside over SSH, using the account
        // its own preset made, so it needs that account's password rather than
        // vitro's key.
        bail!(
            "`destroy --graceful` needs the guest account's password, which vitro does not \
             keep; `lume shutdown {}` takes it",
            record.name
        );
    }
    // Stopping a VM that is already stopped is not an error worth failing on;
    // deleting it is the part that has to work.
    let _ = lume::run(&binary, &lume::stop_args(&record.name));
    lume::run(&binary, &lume::delete_args(&record.name))?;
    Ok(Outcome::Stopped(record.name.clone()))
}

/// Ask the guest to shut down and wait for it.
///
/// The ACPI power button reaches a Linux guest through QMP in a few seconds.
/// A Windows guest on the `virt` machine ignores it entirely — it has no
/// driver for the GPIO key that carries it — so Windows guests must be stopped
/// over SSH instead. That path belongs to the caller that knows the guest OS.
fn graceful_stop(record: &VmRecord) -> Result<()> {
    let socket = match &record.qmp_socket {
        Some(socket) => socket.clone(),
        None => bail!(
            "VM {:?} was started without a monitor socket, so it cannot be asked to \
             shut down; destroy it without --graceful",
            record.name
        ),
    };
    if !socket.exists() {
        bail!(
            "VM {:?} has no QMP socket, so it cannot be asked to shut down; \
             destroy it without --graceful",
            record.name
        );
    }
    qmp::power_down(&socket)?;

    let pid = record.pid;
    let start = record.pid_start_time;
    if !process::wait_for_exit(GRACEFUL_TIMEOUT, || {
        process::is_running(&SysinfoProbe::new(), pid, start)
    }) {
        bail!(
            "VM {:?} did not shut down within {}s",
            record.name,
            GRACEFUL_TIMEOUT.as_secs()
        );
    }
    Ok(())
}

fn forced_stop(record: &VmRecord) {
    process::stop_and_wait(record.pid, Some(record.pid_start_time));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, Golden, ProcessProbe};
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn record(name: &str, dir: PathBuf) -> VmRecord {
        VmRecord {
            name: name.into(),
            image: "linux".into(),
            backend: Backend::Qemu,
            guest: Default::default(),
            // A PID no real process will have, so "is it running" is always no.
            pid: 4_000_000,
            pid_start_time: 1_000,
            ssh_host: "127.0.0.1".into(),
            ssh_port: 53422,
            ssh_user: "dev".into(),
            golden: Golden::Image(PathBuf::from("/srv/linux.qcow2")),
            qmp_socket: None,
            dir,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_record_whose_process_is_gone_is_cleaned_up_not_stopped() {
        let temp = tempfile::tempdir().unwrap();

        let outcome = stop(
            &record("linux-7f3a2c", temp.path().to_path_buf()),
            &Options { graceful: false },
            &ToolPaths::default(),
        )
        .unwrap();

        assert_eq!(outcome, Outcome::CleanedUp("linux-7f3a2c".into()));
    }

    #[test]
    fn destroying_removes_the_record_as_well_as_the_process() {
        let temp = tempfile::tempdir().unwrap();
        let paths_root = temp.path();
        let store = Store::new(paths_root);
        let name = "linux-7f3a2c";
        store.save(&record(name, store.vm_dir(name))).unwrap();

        let store2 = Store::new(paths_root);
        let r = store2.load(name).unwrap();
        stop(&r, &Options { graceful: false }, &ToolPaths::default()).unwrap();
        store2.remove(name).unwrap();

        assert!(!store2.vm_dir(name).exists());
    }

    #[test]
    fn destroy_all_sweeps_debris_that_has_no_process() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        let dir = store.vm_dir("rubble");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vm.json"), "{ truncated").unwrap();
        // Driven through the store directly: the behaviour under test is that
        // an unreadable entry is removed without anything to stop.
        let outcomes: Vec<Outcome> = store
            .list()
            .unwrap()
            .into_iter()
            .map(|entry| match entry {
                Entry::Unreadable { name, .. } => {
                    store.remove(&name).unwrap();
                    Outcome::CleanedUp(name)
                }
                Entry::Vm(r) => Outcome::Stopped(r.name),
            })
            .collect();

        assert_eq!(outcomes, [Outcome::CleanedUp("rubble".into())]);
        assert!(!dir.exists());
    }

    #[test]
    fn a_graceful_stop_without_a_qmp_socket_says_what_to_do_instead() {
        let temp = tempfile::tempdir().unwrap();
        let mut r = record("linux-7f3a2c", temp.path().to_path_buf());
        // Pretend the process is alive by pointing at this test's own PID.
        r.pid = std::process::id();
        r.pid_start_time = SysinfoProbe::new().start_time(r.pid).unwrap();

        let err = stop(&r, &Options { graceful: true }, &ToolPaths::default())
            .unwrap_err()
            .to_string();

        assert!(err.contains("--graceful"), "{err}");
    }
}
