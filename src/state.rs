//! The record of every VM vitro started, and the lock that keeps two of them from
//! writing the same directory at once.
//!
//! A record is trusted only as far as the process it names still exists *and*
//! still has the start time it had when the record was written. A PID on its
//! own is a reusable number, and acting on a stale one means signalling
//! whatever inherited it.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::{Backend, Golden, Guest};

pub const RECORD_FILE: &str = "vm.json";
const LOCK_FILE: &str = "vitro.lock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmRecord {
    pub name: String,
    /// The `[images.*]` key this VM was started from.
    pub image: String,
    pub backend: Backend,
    /// Which family of guest is inside. Defaulted rather than required so a
    /// record written before vitro tracked this still reads.
    #[serde(default)]
    pub guest: Guest,
    pub pid: u32,
    /// Guards against PID reuse: a recycled PID will not have this start time.
    pub pid_start_time: u64,
    pub ssh_host: String,
    pub ssh_port: u16,
    pub ssh_user: String,
    pub golden: Golden,
    pub dir: PathBuf,
    /// Where the monitor socket ended up. Not always inside `dir`: a deep
    /// path overflows `sun_path`, and Windows has no Unix sockets at all.
    #[serde(default)]
    pub qmp_socket: Option<PathBuf>,
    #[serde(with = "time::serde::rfc3339")]
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    Running,
    /// The record outlived its process. Safe to clean up.
    Dead,
}

/// How liveness is decided, behind a trait so the rule can be tested without
/// spawning anything.
pub trait ProcessProbe {
    /// `None` when no such process exists.
    fn start_time(&self, pid: u32) -> Option<u64>;
}

/// The real probe. `sysinfo` covers both `kill(pid, 0)` on Unix and
/// `OpenProcess` on Windows, which matters because vitro detaches its children
/// and so can never `wait` for them.
pub struct SysinfoProbe {
    system: sysinfo::System,
}

impl SysinfoProbe {
    pub fn new() -> Self {
        let mut system = sysinfo::System::new();
        // Command lines are not collected by the default refresh, and finding
        // the process behind a Lume guest is a search through them.
        system.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::All,
            true,
            sysinfo::ProcessRefreshKind::new()
                .with_cmd(sysinfo::UpdateKind::Always)
                .with_exe(sysinfo::UpdateKind::Always),
        );
        Self { system }
    }
}

impl Default for SysinfoProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessProbe for SysinfoProbe {
    fn start_time(&self, pid: u32) -> Option<u64> {
        self.system
            .process(sysinfo::Pid::from_u32(pid))
            .map(|p| p.start_time())
    }
}

impl SysinfoProbe {
    /// The newest process whose command line contains every one of `arguments`
    /// as an argument of its own.
    ///
    /// For a guest vitro did not start itself: `lume` detaches and reports no
    /// PID, but the process holding the VM is still there to be found, and
    /// finding it is what lets a Lume guest use the same liveness rules as
    /// every other one. Newest, because a name may have been used before.
    ///
    /// Whole arguments rather than a substring of the joined line: a VM called
    /// `mac` would otherwise match the process running `mac-test`, and vitro
    /// would record another VM's PID as its own.
    pub fn find_by_command(&self, arguments: &[&str]) -> Option<(u32, u64)> {
        self.system
            .processes()
            .values()
            .filter(|process| {
                let argv: Vec<String> = process
                    .cmd()
                    .iter()
                    .map(|part| part.to_string_lossy().into_owned())
                    .collect();
                arguments.iter().all(|wanted| {
                    argv.iter().any(|arg| {
                        // The program itself arrives as a path.
                        arg == wanted
                            || Path::new(arg)
                                .file_name()
                                .is_some_and(|name| name == *wanted)
                    })
                })
            })
            .max_by_key(|process| process.start_time())
            .map(|process| (process.pid().as_u32(), process.start_time()))
    }
}

impl VmRecord {
    pub fn liveness(&self, probe: &impl ProcessProbe) -> Liveness {
        match probe.start_time(self.pid) {
            Some(start) if start == self.pid_start_time => Liveness::Running,
            _ => Liveness::Dead,
        }
    }

    pub fn ssh_target(&self) -> String {
        format!("{}@{}:{}", self.ssh_user, self.ssh_host, self.ssh_port)
    }
}

/// What a scan of the state directory found. A directory whose record cannot
/// be read is still reported, because it is exactly the debris `prune` exists
/// to remove — dropping it silently would leave disk in use and no way to see
/// why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Vm(VmRecord),
    Unreadable {
        name: String,
        dir: PathBuf,
        reason: String,
    },
}

impl Entry {
    pub fn name(&self) -> &str {
        match self {
            Entry::Vm(record) => &record.name,
            Entry::Unreadable { name, .. } => name,
        }
    }

    pub fn dir(&self) -> &Path {
        match self {
            Entry::Vm(record) => &record.dir,
            Entry::Unreadable { dir, .. } => dir,
        }
    }
}

/// Reads and writes the per-VM records under the state directory.
pub struct Store {
    vms_dir: PathBuf,
    state_dir: PathBuf,
}

impl Store {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        let state_dir = state_dir.into();
        Self {
            vms_dir: state_dir.join("vms"),
            state_dir,
        }
    }

    pub fn vms_dir(&self) -> &Path {
        &self.vms_dir
    }

    pub fn vm_dir(&self, name: impl AsRef<str>) -> PathBuf {
        self.vms_dir.join(name.as_ref())
    }

    /// Every VM directory, sorted by name so output is stable between runs.
    /// A missing state directory is an empty list, not an error: that is what
    /// a fresh installation looks like.
    pub fn list(&self) -> Result<Vec<Entry>> {
        let read_dir = match fs::read_dir(&self.vms_dir) {
            Ok(iter) => iter,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("cannot read {}", self.vms_dir.display()))
            }
        };

        let mut entries = Vec::new();
        for item in read_dir {
            let item = item.with_context(|| format!("cannot read {}", self.vms_dir.display()))?;
            if !item.file_type()?.is_dir() {
                continue;
            }
            let dir = item.path();
            let name = item.file_name().to_string_lossy().into_owned();
            entries.push(match self.read_record(&dir) {
                Ok(record) => Entry::Vm(record),
                Err(reason) => Entry::Unreadable {
                    name,
                    dir,
                    reason: reason.to_string(),
                },
            });
        }
        entries.sort_by(|a, b| a.name().cmp(b.name()));
        Ok(entries)
    }

    pub fn load(&self, name: impl AsRef<str>) -> Result<VmRecord> {
        let name = name.as_ref();
        let dir = self.vm_dir(name);
        self.read_record(&dir)
            .with_context(|| format!("no usable record for VM {name:?}"))
    }

    /// Turn what the user typed into a VM name.
    ///
    /// An exact name wins, then an unambiguous prefix — a generated name ends
    /// in six hex digits, and nobody wants to type them. An ambiguous prefix
    /// lists the candidates instead of picking one: these names are arguments
    /// to `destroy`.
    pub fn resolve(&self, name: impl AsRef<str>) -> Result<String> {
        let name = name.as_ref();
        let names: Vec<String> = self
            .list()?
            .iter()
            .map(|entry| entry.name().to_owned())
            .collect();

        if names.iter().any(|known| known == name) {
            return Ok(name.to_owned());
        }
        let mut matched = names.into_iter().filter(|known| known.starts_with(name));
        let Some(first) = matched.next() else {
            bail!("no VM is called {name:?}; `vitro ls` shows what there is");
        };
        let rest: Vec<String> = matched.collect();
        if rest.is_empty() {
            return Ok(first);
        }
        let mut all = vec![first];
        all.extend(rest);
        all.sort();
        bail!("{name:?} matches {}; say which one", all.join(", "))
    }

    fn read_record(&self, dir: &Path) -> Result<VmRecord> {
        let path = dir.join(RECORD_FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("cannot read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("cannot parse {}", path.display()))
    }

    /// Write the record, creating the VM directory if needed.
    ///
    /// The write goes to a temporary file in the same directory and is then
    /// renamed, so a crash mid-write leaves the previous record intact rather
    /// than a truncated one that would read as debris.
    pub fn save(&self, record: &VmRecord) -> Result<()> {
        let dir = self.vm_dir(&record.name);
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;

        let json = serde_json::to_string_pretty(record)?;
        let mut temp = tempfile::NamedTempFile::new_in(&dir)
            .with_context(|| format!("cannot create a temporary file in {}", dir.display()))?;
        temp.write_all(json.as_bytes())?;
        temp.write_all(b"\n")?;
        temp.as_file().sync_all()?;

        let path = dir.join(RECORD_FILE);
        temp.persist(&path)
            .map_err(|e| e.error)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(())
    }

    /// Missing is success: the caller wanted it gone.
    pub fn remove(&self, name: impl AsRef<str>) -> Result<()> {
        let dir = self.vm_dir(name);
        match fs::remove_dir_all(&dir) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("cannot remove {}", dir.display())),
        }
    }

    /// Serialise record creation and removal across processes.
    ///
    /// Held only around state changes. A VM's whole lifetime, or a build that
    /// runs for hours, must never sit inside this.
    pub fn lock(&self) -> Result<StateLock> {
        fs::create_dir_all(&self.state_dir)
            .with_context(|| format!("cannot create {}", self.state_dir.display()))?;
        let path = self.state_dir.join(LOCK_FILE);
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;

        // `try_lock_exclusive` reports "someone else holds it" as an error,
        // not as `Ok(false)`. Letting that propagate untranslated would turn a
        // second vitro running normally into an I/O failure.
        match file.try_lock_exclusive() {
            Ok(()) => Ok(StateLock { _file: file }),
            Err(e) if is_already_held(&e) => {
                bail!("another vitro is using {}", self.state_dir.display())
            }
            Err(e) => Err(e).with_context(|| format!("cannot lock {}", path.display())),
        }
    }
}

/// Whether a refused lock means somebody else is holding it.
///
/// Unix reports `WouldBlock`. Windows reports `ERROR_LOCK_VIOLATION`, which
/// Rust does not map to it, so matching on the kind alone turned a second vitro
/// running perfectly normally into an I/O failure there — the exact translation
/// the caller is trying to avoid.
fn is_already_held(error: &std::io::Error) -> bool {
    if error.kind() == ErrorKind::WouldBlock {
        return true;
    }
    #[cfg(windows)]
    {
        const ERROR_SHARING_VIOLATION: i32 = 32;
        const ERROR_LOCK_VIOLATION: i32 = 33;
        matches!(
            error.raw_os_error(),
            Some(ERROR_SHARING_VIOLATION | ERROR_LOCK_VIOLATION)
        )
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Releases the state lock when dropped, including on an early return.
#[derive(Debug)]
pub struct StateLock {
    _file: fs::File,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A probe with whatever processes a test says exist.
    struct FakeProbe(HashMap<u32, u64>);

    impl FakeProbe {
        fn with(pid: u32, start_time: u64) -> Self {
            Self(HashMap::from([(pid, start_time)]))
        }

        fn empty() -> Self {
            Self(HashMap::new())
        }
    }

    impl ProcessProbe for FakeProbe {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).copied()
        }
    }

    fn record(name: &str) -> VmRecord {
        VmRecord {
            name: name.to_string(),
            image: "linux".into(),
            backend: Backend::Qemu,
            guest: Default::default(),
            pid: 4242,
            pid_start_time: 1_700_000_000,
            ssh_host: "127.0.0.1".into(),
            ssh_port: 53422,
            ssh_user: "dev".into(),
            golden: Golden::Image(PathBuf::from("/srv/linux.qcow2")),
            dir: PathBuf::from("/state/vms").join(name),
            qmp_socket: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_live_pid_with_the_recorded_start_time_is_running() {
        let record = record("linux-7f3a2c");
        let probe = FakeProbe::with(record.pid, record.pid_start_time);

        assert_eq!(record.liveness(&probe), Liveness::Running);
    }

    #[test]
    fn a_vanished_pid_is_dead() {
        let record = record("linux-7f3a2c");

        assert_eq!(record.liveness(&FakeProbe::empty()), Liveness::Dead);
    }

    #[test]
    fn a_reused_pid_is_dead_because_the_start_time_differs() {
        // The whole reason the start time is recorded: this process is not ours.
        let record = record("linux-7f3a2c");
        let probe = FakeProbe::with(record.pid, record.pid_start_time + 1);

        assert_eq!(record.liveness(&probe), Liveness::Dead);
    }

    #[test]
    fn a_record_survives_a_round_trip_through_disk() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        let original = record("linux-7f3a2c");

        store.save(&original).unwrap();
        let loaded = store.load("linux-7f3a2c").unwrap();

        assert_eq!(loaded, original);
    }

    #[test]
    fn listing_a_state_dir_that_does_not_exist_yet_is_empty() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("never-created"));

        assert_eq!(store.list().unwrap(), Vec::new());
    }

    #[test]
    fn listing_is_sorted_by_name() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        for name in ["windows-01", "linux-02", "linux-01"] {
            store.save(&record(name)).unwrap();
        }

        let entries = store.list().unwrap();
        let names: Vec<&str> = entries.iter().map(|e| e.name()).collect();

        assert_eq!(names, ["linux-01", "linux-02", "windows-01"]);
    }

    #[test]
    fn a_directory_with_an_unreadable_record_is_listed_as_debris() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        store.save(&record("good")).unwrap();
        let broken = store.vm_dir("broken");
        fs::create_dir_all(&broken).unwrap();
        fs::write(broken.join(RECORD_FILE), "{ not json").unwrap();

        let entries = store.list().unwrap();

        assert_eq!(entries.len(), 2);
        assert!(matches!(entries[0], Entry::Unreadable { .. }));
        assert_eq!(entries[0].name(), "broken");
        assert!(matches!(entries[1], Entry::Vm(_)));
    }

    #[test]
    fn a_directory_with_no_record_at_all_is_also_debris() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        fs::create_dir_all(store.vm_dir("empty")).unwrap();

        let entries = store.list().unwrap();

        assert!(matches!(entries[0], Entry::Unreadable { .. }));
    }

    #[test]
    fn saving_twice_replaces_the_record_without_leaving_scratch_files() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        let mut r = record("linux-7f3a2c");
        store.save(&r).unwrap();
        r.ssh_port = 40000;
        store.save(&r).unwrap();

        assert_eq!(store.load("linux-7f3a2c").unwrap().ssh_port, 40000);
        let files: Vec<_> = fs::read_dir(store.vm_dir("linux-7f3a2c"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(files, [RECORD_FILE]);
    }

    #[test]
    fn removing_a_vm_that_is_already_gone_succeeds() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());

        store.remove("never-existed").unwrap();
    }

    #[test]
    fn removing_a_vm_takes_its_directory_with_it() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());
        store.save(&record("linux-7f3a2c")).unwrap();

        store.remove("linux-7f3a2c").unwrap();

        assert!(!store.vm_dir("linux-7f3a2c").exists());
        assert_eq!(store.list().unwrap(), Vec::new());
    }

    #[test]
    fn a_second_lock_is_refused_while_the_first_is_held() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path());

        let held = store.lock().unwrap();
        let err = store.lock().unwrap_err().to_string();
        assert!(err.contains("another vitro"), "{err}");

        drop(held);
        store
            .lock()
            .expect("the lock is free once the guard is dropped");
    }
}
