//! `vitro port` and `vitro inspect` — the record, for other programs to read.
//!
//! `port` prints one number and nothing else so it can be substituted straight
//! into another command line. `inspect` is `ls --json` narrowed to one VM.

use anyhow::{bail, Result};
use serde::Serialize;

use crate::{Paths, ProcessProbe, Store, VmRecord};

/// The forwarded SSH port, but only while something is listening on it.
///
/// A dead VM's port goes back to the operating system and is handed out again,
/// so answering with it would point `ssh -p $(vitro port vm)` at whatever took
/// it over. The usual caller substitutes this into another command line and
/// cannot check.
pub fn port(paths: &Paths, probe: &impl ProcessProbe, name: &str) -> Result<u16> {
    let store = Store::new(paths.state_dir());
    let record = store.load(store.resolve(name)?)?;
    if record.liveness(probe) != crate::Liveness::Running {
        bail!(
            "VM {:?} is not running, so its port has been released",
            record.name
        );
    }
    Ok(record.ssh_port)
}

#[derive(Debug, Serialize)]
pub struct Inspected {
    #[serde(flatten)]
    pub record: VmRecord,
    /// Derived, not stored: whether the recorded process is still there.
    pub running: bool,
}

pub fn inspect(paths: &Paths, probe: &impl ProcessProbe, name: &str) -> Result<Inspected> {
    let store = Store::new(paths.state_dir());
    let record = store.load(store.resolve(name)?)?;
    let running = record.liveness(probe) == crate::Liveness::Running;
    Ok(Inspected { record, running })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, Golden};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use time::OffsetDateTime;

    struct FakeProbe(HashMap<u32, u64>);

    impl ProcessProbe for FakeProbe {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).copied()
        }
    }

    fn saved(paths: &Paths, name: &str) -> Store {
        let store = Store::new(paths.state_dir());
        store
            .save(&VmRecord {
                name: name.into(),
                image: "linux".into(),
                backend: Backend::Qemu,
                guest: Default::default(),
                pid: 4_000_000,
                pid_start_time: 1_000,
                ssh_host: "127.0.0.1".into(),
                ssh_port: 53422,
                ssh_user: "dev".into(),
                golden: Golden::Image(PathBuf::from("/srv/linux.qcow2")),
                qmp_socket: None,
                dir: store.vm_dir(name),
                created_at: OffsetDateTime::UNIX_EPOCH,
            })
            .unwrap();
        store
    }

    #[test]
    fn the_port_is_reachable_through_a_prefix_of_the_name() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        saved(&paths, "linux-7f3a2c");
        let probe = FakeProbe(HashMap::from([(4_000_000, 1_000)]));

        assert_eq!(port(&paths, &probe, "linux-7f").unwrap(), 53422);
    }

    #[test]
    fn a_dead_vms_port_is_refused_rather_than_handed_to_a_caller_that_cannot_check() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        saved(&paths, "linux-7f3a2c");

        let err = port(&paths, &FakeProbe(HashMap::new()), "linux-7f3a2c")
            .unwrap_err()
            .to_string();

        assert!(err.contains("released"), "{err}");
    }

    #[test]
    fn inspecting_says_whether_the_process_is_still_there() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        saved(&paths, "linux-7f3a2c");

        let alive = inspect(
            &paths,
            &FakeProbe(HashMap::from([(4_000_000, 1_000)])),
            "linux-7f3a2c",
        )
        .unwrap();
        let dead = inspect(&paths, &FakeProbe(HashMap::new()), "linux-7f3a2c").unwrap();

        assert!(alive.running);
        assert!(!dead.running);
    }

    #[test]
    fn the_json_carries_the_record_at_the_top_level() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        saved(&paths, "linux-7f3a2c");

        let text = serde_json::to_string(
            &inspect(&paths, &FakeProbe(HashMap::new()), "linux-7f3a2c").unwrap(),
        )
        .unwrap();

        assert!(text.contains(r#""name":"linux-7f3a2c""#), "{text}");
        assert!(text.contains(r#""ssh_port":53422"#), "{text}");
        assert!(text.contains(r#""running":false"#), "{text}");
    }
}
