//! `vitro run` — start a VM from a golden image.
//!
//! Everything created along the way is owned by a guard that tears it down
//! unless the whole sequence succeeds. A half-started VM is worse than none:
//! it leaves a directory that looks like a record, a port that looks taken,
//! and possibly a process nobody will ever stop.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use time::OffsetDateTime;

use crate::{
    golden, image, lume, process, qemu, ssh, tools, Backend, Config, Golden, Image, Paths,
    ProcessProbe, Store, SysinfoProbe, VmRecord,
};

/// How many times to re-draw a port when QEMU refuses to bind the one we took.
///
/// The window between closing the probe socket and QEMU binding is real but
/// tiny: a thousand consecutive probe-and-rebind attempts on this host never
/// lost the race, and QEMU fails loudly when it does happen. So this is a
/// safety net, not a loop that is expected to spin.
const PORT_ATTEMPTS: usize = 8;

/// The VM's writable disk. `promote` flattens this one, so the name is
/// shared rather than spelled out twice.
pub const OVERLAY_FILE: &str = "overlay.qcow2";
const VARSTORE_FILE: &str = "efi-vars.fd";
const SERIAL_LOG: &str = "serial.log";
const QEMU_LOG: &str = "qemu.log";
const PIDFILE: &str = "qemu.pid";
const KNOWN_HOSTS: &str = "known_hosts";

pub struct Options {
    pub image: String,
    pub name: Option<String>,
}

pub fn run(paths: &Paths, config: &Config, options: Options) -> Result<VmRecord> {
    let image = config.image(&options.image)?;
    if image.backend == Backend::Lume {
        return run_lume(paths, config, image, options);
    }
    let golden = match &image.golden {
        Golden::Image(path) => path.clone(),
        Golden::Latest => golden::newest(&paths.golden_dir(), &image.key).ok_or_else(|| {
            anyhow!(
                "no image has been built for {0:?} yet; run `vitro build {0}`",
                image.key
            )
        })?,
        Golden::VmName(name) => bail!("image {:?} names a VM ({name}), not a disk", image.key),
    };

    let store = Store::new(paths.state_dir());

    let host = qemu::HostTarget::detect()?;
    let cfg_tools = config.tools();
    let tools = tools::Tools::for_qemu(
        &host.binary,
        cfg_tools.qemu.as_deref(),
        cfg_tools.qemu_img.as_deref(),
        cfg_tools.ssh.as_deref(),
        cfg_tools.scp.as_deref(),
    )?;

    let name = match options.name {
        Some(name) => {
            if store.vm_dir(&name).exists() {
                bail!("a VM named {name:?} already exists");
            }
            name
        }
        None => unique_name(&store, &image.key),
    };

    // Held across reaping and record creation: `reap_dead` deletes any
    // directory without a readable record, and a concurrent `run` has exactly
    // that between creating its directory and writing `vm.json`.
    let lock = store.lock()?;
    reap_dead(&store, &SysinfoProbe::new())?;
    let dir = store.vm_dir(&name);
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let mut guard = VmGuard::new(dir.clone());

    image::create_overlay(&tools.qemu_img, &golden, &dir.join(OVERLAY_FILE), None)?;
    qemu::ensure_varstore(&dir.join(VARSTORE_FILE))?;

    let qmp_socket = host
        .supports_qmp()
        .then(|| qemu::qmp_socket_path(&dir, &name))
        .flatten();
    let (pid, port) =
        start_with_a_free_port(&tools.qemu, &host, image, &name, &dir, qmp_socket.clone())?;
    guard.owns_process(pid);

    let pid_start_time = SysinfoProbe::new().start_time(pid).ok_or_else(|| {
        anyhow::anyhow!("QEMU reported PID {pid} but the process was already gone")
    })?;
    guard.owns_process_from(pid, pid_start_time);

    let record = VmRecord {
        name: name.clone(),
        image: image.key.clone(),
        backend: image.backend,
        guest: image.guest,
        pid,
        pid_start_time,
        ssh_host: "127.0.0.1".into(),
        ssh_port: port,
        ssh_user: image.ssh_user.clone(),
        golden: Golden::Image(golden),
        dir: dir.clone(),
        qmp_socket,
        created_at: OffsetDateTime::now_utc(),
    };
    store.save(&record)?;
    // The lock covers record creation only. Holding it while waiting for a
    // guest to boot would serialise every `run` on the machine.
    drop(lock);

    let target = ssh::SshTarget {
        host: record.ssh_host.clone(),
        port,
        user: record.ssh_user.clone(),
        key: image.ssh_key.clone(),
        known_hosts: dir.join(KNOWN_HOSTS),
    };
    wait_for_guest(
        &tools.ssh,
        &target,
        image.boot_timeout,
        pid,
        pid_start_time,
        &dir,
    )?;

    guard.commit();
    Ok(record)
}

/// Start a macOS guest by asking `lume` to clone the golden VM and run it.
///
/// A clone rather than an overlay because that is what `lume` offers, and on
/// APFS it costs nothing: the copy is made by reference, so a 26 GB VM appears
/// in about two seconds.
fn run_lume(
    paths: &Paths,
    config: &Config,
    image: &crate::Image,
    options: Options,
) -> Result<VmRecord> {
    let Golden::VmName(golden) = &image.golden else {
        bail!(
            "image {:?} uses the lume backend, so `golden` must name a VM rather than a disk",
            image.key
        );
    };

    let cfg_tools = config.tools();
    let binary = tools::resolve("lume", cfg_tools.lume.as_deref(), "tools.lume")?.path;
    let ssh_binary = tools::resolve("ssh", cfg_tools.ssh.as_deref(), "tools.ssh")?.path;
    let store = Store::new(paths.state_dir());

    let name = match options.name {
        Some(name) => {
            if store.vm_dir(&name).exists() {
                bail!("a VM named {name:?} already exists");
            }
            name
        }
        None => unique_name(&store, &image.key),
    };

    // Long enough to claim the name and no longer. Cloning, starting and
    // waiting for an address take minutes, and holding the lock across them
    // would stop every other vitro on the machine for the duration.
    let dir = {
        let _lock = store.lock()?;
        reap_dead(&store, &SysinfoProbe::new())?;
        let dir = store.vm_dir(&name);
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        dir
    };
    let mut guard = VmGuard::new(dir.clone());

    lume::run(&binary, &lume::clone_args(golden, &name))?;
    guard.owns_lume(binary.clone(), name.clone());
    lume::run(&binary, &lume::run_args(&name))?;

    // One ceiling covering both waits, not one each.
    let booting = Instant::now();
    let address = lume::wait_until_ready(&binary, &name, image.boot_timeout)?;

    // `lume` detaches and reports no PID, so the process holding the VM is
    // found rather than told. Without it every later command would have to ask
    // `lume` again just to know whether the guest is still there.
    let (pid, pid_start_time) = SysinfoProbe::new()
        .find_by_command(&["lume", &name])
        .ok_or_else(|| anyhow::anyhow!("lume started {name} but left no process to watch"))?;
    guard.owns_process_from(pid, pid_start_time);

    let record = VmRecord {
        name: name.clone(),
        image: image.key.clone(),
        backend: image.backend,
        guest: image.guest,
        pid,
        pid_start_time,
        ssh_host: address,
        // A guest with its own address needs no forwarding, so this is the
        // port sshd actually listens on rather than one vitro picked.
        ssh_port: 22,
        ssh_user: image.ssh_user.clone(),
        golden: image.golden.clone(),
        dir: dir.clone(),
        qmp_socket: None,
        created_at: OffsetDateTime::now_utc(),
    };
    {
        let _lock = store.lock()?;
        store.save(&record)?;
    }

    let target = ssh::SshTarget {
        host: record.ssh_host.clone(),
        port: record.ssh_port,
        user: record.ssh_user.clone(),
        key: image.ssh_key.clone(),
        known_hosts: dir.join(KNOWN_HOSTS),
    };
    let left = ssh::remaining(image.boot_timeout, booting.elapsed());
    ssh::wait_until_reachable(&ssh_binary, &target, left, || {
        process::is_running(&SysinfoProbe::new(), pid, pid_start_time)
    })?;

    guard.commit();
    Ok(record)
}

/// Remove records whose process is gone, so a fresh `run` does not inherit the
/// debris of an older one.
/// Delete the VM behind a record being reaped, for a backend that keeps one.
///
/// A QEMU guest is only an overlay inside the directory that goes with the
/// record. A Lume guest is a VM in lume's own store, and dropping the record
/// alone would leave tens of gigabytes that vitro can no longer name.
fn reap_backend(record: &VmRecord) {
    if record.backend != Backend::Lume {
        return;
    }
    let Ok(found) = tools::resolve("lume", None, "tools.lume") else {
        eprintln!(
            "vitro: {} is gone but lume could not be found to delete it",
            record.name
        );
        return;
    };
    let _ = lume::run(&found.path, &lume::stop_args(&record.name));
    if let Err(e) = lume::run(&found.path, &lume::delete_args(&record.name)) {
        eprintln!("vitro: could not delete the lume VM {}: {e:#}", record.name);
    }
}

pub fn reap_dead(store: &Store, probe: &impl ProcessProbe) -> Result<Vec<String>> {
    let mut reaped = Vec::new();
    for entry in store.list()? {
        let stale = match &entry {
            crate::Entry::Vm(record) => record.liveness(probe) == crate::Liveness::Dead,
            crate::Entry::Unreadable { .. } => true,
        };
        if stale {
            let name = entry.name().to_string();
            // The backend's own VM first: once the record is gone, nothing
            // knows the VM was vitro's.
            if let crate::Entry::Vm(record) = &entry {
                reap_backend(record);
            }
            store.remove(&name)?;
            reaped.push(name);
        }
    }
    Ok(reaped)
}

fn unique_name(store: &Store, image_key: &str) -> String {
    loop {
        let suffix: u32 = rand::random::<u32>() % 0x100_0000;
        let name = format!("{image_key}-{suffix:06x}");
        if !store.vm_dir(&name).exists() {
            return name;
        }
    }
}

/// Take a free port, start QEMU on it, and re-draw if QEMU could not bind it.
fn start_with_a_free_port(
    qemu_binary: &Path,
    host: &qemu::HostTarget,
    image: &Image,
    name: &str,
    dir: &Path,
    qmp_socket: Option<PathBuf>,
) -> Result<(u32, u16)> {
    let mut last_error = None;
    for _ in 0..PORT_ATTEMPTS {
        let port = free_port()?;
        let spec = spec_for(image, name, dir, port, qmp_socket.clone());
        let args = qemu::args(&spec, host)?;

        match qemu::spawn(qemu_binary, &args, &dir.join(QEMU_LOG), &dir.join(PIDFILE)) {
            Ok(pid) => return Ok((pid, port)),
            Err(e) => {
                // QEMU says exactly this when the forwarded port is taken;
                // anything else is not worth retrying.
                let text = format!("{e:#}");
                if !text.contains("Could not set up host forwarding rule") {
                    return Err(e);
                }
                last_error = Some(e);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("could not find a free port")))
}

fn spec_for(
    image: &Image,
    name: &str,
    dir: &Path,
    port: u16,
    qmp_socket: Option<PathBuf>,
) -> qemu::VmSpec {
    qemu::VmSpec {
        name: name.to_string(),
        cpus: image.cpus,
        memory: image.memory,
        system_disk: dir.join(OVERLAY_FILE),
        firmware: image.firmware.clone(),
        varstore: dir.join(VARSTORE_FILE),
        ssh_port: port,
        resolution: image.resolution,
        serial_log: dir.join(SERIAL_LOG),
        pidfile: dir.join(PIDFILE),
        qmp_socket,
        media: Vec::new(),
        extra_args: image.extra_args.clone(),
    }
}

/// A port nobody is listening on right now.
///
/// There is an unavoidable gap between closing this socket and QEMU binding
/// the number; the caller retries if something slips in.
fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").context("cannot find a free port")?;
    Ok(listener.local_addr()?.port())
}

fn wait_for_guest(
    ssh_binary: &Path,
    target: &ssh::SshTarget,
    timeout: Duration,
    pid: u32,
    pid_start_time: u64,
    dir: &Path,
) -> Result<()> {
    // Rebuilt on every poll on purpose: `SysinfoProbe` snapshots the process
    // table when it is constructed, so a probe made once would report a
    // process that has since died as alive for the whole wait.
    let alive = move || process::is_running(&SysinfoProbe::new(), pid, pid_start_time);

    ssh::wait_until_reachable(ssh_binary, target, timeout, alive).map_err(|e| {
        // Without the tail of the console the user has an error and nowhere to
        // look; with it, most boot failures are obvious at a glance.
        match tail(&dir.join(SERIAL_LOG), 20) {
            Some(tail) if !tail.trim().is_empty() => {
                e.context(format!("last lines of serial.log:\n{tail}"))
            }
            _ => e,
        }
    })?;
    Ok(())
}

fn tail(path: &Path, lines: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let collected: Vec<&str> = text.lines().rev().take(lines).collect();
    Some(collected.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

/// Undoes a partially started VM unless `commit` is called.
struct VmGuard {
    dir: PathBuf,
    pid: Option<(u32, Option<u64>)>,
    /// A cloned Lume VM, which has to be deleted through `lume` rather than by
    /// removing a directory vitro owns.
    lume_vm: Option<(PathBuf, String)>,
    committed: bool,
}

impl VmGuard {
    fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            pid: None,
            lume_vm: None,
            committed: false,
        }
    }

    fn owns_lume(&mut self, binary: PathBuf, name: String) {
        self.lume_vm = Some((binary, name));
    }

    /// Before the start time is known, all the guard can do is ask the PID to
    /// stop; the window is short and nothing else has been written yet.
    fn owns_process(&mut self, pid: u32) {
        self.pid = Some((pid, None));
    }

    fn owns_process_from(&mut self, pid: u32, start_time: u64) {
        self.pid = Some((pid, Some(start_time)));
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for VmGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // Stop the process *before* the directory goes, and confirm it. Once
        // the record is deleted nothing can find this QEMU again, so leaving
        // it running would strand it holding a port and a deleted overlay.
        if let Some((binary, name)) = &self.lume_vm {
            // Through lume, and in its order: a half-created clone left behind
            // is a VM the user never asked for, holding tens of gigabytes.
            let _ = lume::run(binary, &lume::stop_args(name));
            if let Err(e) = lume::run(binary, &lume::delete_args(name)) {
                eprintln!("vitro: could not delete the lume VM {name}: {e:#}");
            }
        } else if let Some((pid, start_time)) = self.pid {
            if !process::stop_and_wait(pid, start_time) {
                eprintln!(
                    "vitro: could not stop QEMU (pid {pid}); {} was left in place",
                    self.dir.display()
                );
                return;
            }
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Liveness;
    use std::collections::HashMap;

    struct FakeProbe(HashMap<u32, u64>);

    impl ProcessProbe for FakeProbe {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).copied()
        }
    }

    fn record(name: &str, pid: u32) -> VmRecord {
        VmRecord {
            name: name.into(),
            image: "linux".into(),
            backend: Backend::Qemu,
            guest: Default::default(),
            pid,
            pid_start_time: 1_000,
            ssh_host: "127.0.0.1".into(),
            ssh_port: 53422,
            ssh_user: "dev".into(),
            golden: Golden::Image(PathBuf::from("/srv/linux.qcow2")),
            qmp_socket: None,
            dir: PathBuf::new(),
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_free_port_is_actually_free() {
        let port = free_port().unwrap();

        TcpListener::bind(("127.0.0.1", port)).expect("the port should be bindable again");
    }

    #[test]
    fn reaping_removes_records_whose_process_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        store.save(&record("alive", 100)).unwrap();
        store.save(&record("gone", 200)).unwrap();
        let probe = FakeProbe(HashMap::from([(100, 1_000)]));

        let reaped = reap_dead(&store, &probe).unwrap();

        assert_eq!(reaped, ["gone"]);
        assert!(store.vm_dir("alive").exists());
        assert!(!store.vm_dir("gone").exists());
    }

    #[test]
    fn reaping_removes_directories_whose_record_is_unreadable() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        let dir = store.vm_dir("rubble");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("vm.json"), "{ truncated").unwrap();

        let reaped = reap_dead(&store, &FakeProbe(HashMap::new())).unwrap();

        assert_eq!(reaped, ["rubble"]);
        assert!(!dir.exists());
    }

    #[test]
    fn a_reused_pid_does_not_keep_a_record_alive() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        store.save(&record("stale", 100)).unwrap();
        // Same PID, different process.
        let probe = FakeProbe(HashMap::from([(100, 9_999)]));

        assert_eq!(
            store.load("stale").unwrap().liveness(&probe),
            Liveness::Dead
        );
        assert_eq!(reap_dead(&store, &probe).unwrap(), ["stale"]);
    }

    #[test]
    fn generated_names_start_with_the_image_key() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());

        let name = unique_name(&store, "linux");

        assert!(name.starts_with("linux-"), "{name}");
        assert_eq!(name.len(), "linux-".len() + 6);
    }

    #[test]
    fn the_guard_removes_the_directory_unless_committed() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("vm");
        fs::create_dir_all(&dir).unwrap();

        drop(VmGuard::new(dir.clone()));

        assert!(!dir.exists());
    }

    #[test]
    fn a_committed_guard_leaves_everything_in_place() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("vm");
        fs::create_dir_all(&dir).unwrap();

        let mut guard = VmGuard::new(dir.clone());
        guard.commit();
        drop(guard);

        assert!(dir.exists());
    }

    #[test]
    fn the_serial_log_tail_is_the_last_lines_in_order() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("serial.log");
        fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

        assert_eq!(tail(&path, 2).unwrap(), "three\nfour");
    }

    #[test]
    fn a_missing_serial_log_yields_no_tail() {
        assert!(tail(Path::new("/nonexistent/serial.log"), 5).is_none());
    }
}
