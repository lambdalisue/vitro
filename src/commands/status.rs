//! `vitro status`, and `vitro` with no arguments.
//!
//! `ls` says what exists. `status` also says what to do about it, because the
//! usual caller is an agent that has just arrived in a session and has to
//! decide its next command without reading the documentation.

use anyhow::Result;
use serde::Serialize;

use crate::{commands::ls, Config, Paths, ProcessProbe, Store};

/// How long a VM may run before `status` asks what it is for. A day, because
/// a VM that has seen one night has stopped being a scratch machine and turned
/// into a machine somebody is keeping without meaning to.
const LONG_LIVED_SECONDS: u64 = 24 * 60 * 60;

#[derive(Debug, Serialize)]
pub struct Status {
    #[serde(flatten)]
    pub listing: ls::Listing,
    /// Commands worth running next, most useful first.
    pub next: Vec<String>,
}

pub fn collect(
    paths: &Paths,
    config: &Config,
    probe: &impl ProcessProbe,
    name: Option<&str>,
) -> Result<Status> {
    let store = Store::new(paths.state_dir());
    let mut listing = ls::collect(paths, &store, config, probe, ls::now())?;

    if let Some(name) = name {
        let resolved = store.resolve(name)?;
        listing.vms.retain(|vm| vm.name == resolved);
    }

    Ok(Status {
        next: suggestions(paths, config, &listing),
        listing,
    })
}

fn suggestions(paths: &Paths, config: &Config, listing: &ls::Listing) -> Vec<String> {
    let mut next = Vec::new();

    if !paths.config_file().exists() {
        next.push(format!(
            "write {} — `vitro doctor` says what belongs in it",
            paths.config_file().display()
        ));
        return next;
    }

    let running: Vec<&ls::VmRow> = listing
        .vms
        .iter()
        .filter(|vm| vm.status == ls::Status::Running)
        .collect();

    for vm in &running {
        next.push(format!("vitro exec {} <command>", vm.name));
    }
    for vm in &running {
        if let Some(uptime) = vm.uptime_seconds.filter(|s| *s >= LONG_LIVED_SECONDS) {
            next.push(format!(
                "{0} has been up {1}: `vitro promote {0}` keeps it as an image, \
                 `vitro destroy {0}` ends it",
                vm.name,
                ls::humanize_duration(uptime)
            ));
        }
    }
    if let Some(vm) = running.first() {
        next.push(format!("vitro destroy {}", vm.name));
    }

    if running.is_empty() {
        for golden in &listing.golden {
            if golden.present {
                next.push(format!("vitro run {}", golden.image));
            } else {
                next.push(format!("vitro build {}", golden.image));
            }
        }
        if config.is_empty() {
            next.push(format!(
                "add an [images.*] section to {}",
                paths.config_file().display()
            ));
        }
    }

    if listing
        .vms
        .iter()
        .any(|vm| vm.status != ls::Status::Running)
    {
        next.push("vitro ls --prune".into());
    }

    next
}

pub fn render(status: &Status) -> String {
    let mut out = ls::render(&status.listing);
    if !status.next.is_empty() {
        out.push_str("\nNEXT\n");
        for step in &status.next {
            out.push_str(&format!("  {step}\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, Golden, VmRecord};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use time::OffsetDateTime;

    struct FakeProbe(HashMap<u32, u64>);

    impl ProcessProbe for FakeProbe {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).copied()
        }
    }

    fn save(paths: &Paths, name: &str, pid: u32) {
        save_created(paths, name, pid, OffsetDateTime::UNIX_EPOCH);
    }

    fn save_created(paths: &Paths, name: &str, pid: u32, created_at: OffsetDateTime) {
        let store = Store::new(paths.state_dir());
        store
            .save(&VmRecord {
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
                dir: store.vm_dir(name),
                created_at,
            })
            .unwrap();
    }

    #[test]
    fn without_a_configuration_the_only_advice_is_to_write_one() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        let status = collect(&paths, &Config::default(), &FakeProbe(HashMap::new()), None).unwrap();

        assert_eq!(status.next.len(), 1);
        assert!(status.next[0].contains("config.toml"), "{:?}", status.next);
    }

    #[test]
    fn a_running_vm_is_offered_as_a_target_for_exec_and_destroy() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "").unwrap();
        save(&paths, "linux-7f3a2c", 4_000_000);

        let status = collect(
            &paths,
            &Config::default(),
            &FakeProbe(HashMap::from([(4_000_000, 1_000)])),
            None,
        )
        .unwrap();

        assert!(
            status
                .next
                .iter()
                .any(|s| s == "vitro exec linux-7f3a2c <command>"),
            "{:?}",
            status.next
        );
        assert!(
            status
                .next
                .iter()
                .any(|s| s == "vitro destroy linux-7f3a2c"),
            "{:?}",
            status.next
        );
    }

    fn configured() -> (tempfile::TempDir, Paths) {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "").unwrap();
        (temp, paths)
    }

    #[test]
    fn a_vm_up_for_more_than_a_day_is_offered_promote_or_destroy() {
        let (_temp, paths) = configured();
        let three_days_ago = ls::now() - time::Duration::days(3);
        save_created(&paths, "linux-7f3a2c", 4_000_000, three_days_ago);

        let status = collect(
            &paths,
            &Config::default(),
            &FakeProbe(HashMap::from([(4_000_000, 1_000)])),
            None,
        )
        .unwrap();

        let advice = status
            .next
            .iter()
            .find(|s| s.contains("up 3d"))
            .unwrap_or_else(|| panic!("{:?}", status.next));
        assert!(advice.contains("vitro promote linux-7f3a2c"), "{advice}");
        assert!(advice.contains("vitro destroy linux-7f3a2c"), "{advice}");
    }

    #[test]
    fn a_vm_started_today_gets_no_such_advice() {
        let (_temp, paths) = configured();
        save_created(&paths, "linux-7f3a2c", 4_000_000, ls::now());

        let status = collect(
            &paths,
            &Config::default(),
            &FakeProbe(HashMap::from([(4_000_000, 1_000)])),
            None,
        )
        .unwrap();

        assert!(
            !status.next.iter().any(|s| s.contains("promote")),
            "{:?}",
            status.next
        );
    }

    #[test]
    fn a_dead_record_is_answered_with_the_command_that_clears_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "").unwrap();
        save(&paths, "linux-7f3a2c", 4_000_000);

        let status = collect(&paths, &Config::default(), &FakeProbe(HashMap::new()), None).unwrap();

        assert!(
            status.next.iter().any(|s| s == "vitro ls --prune"),
            "{:?}",
            status.next
        );
    }

    #[test]
    fn naming_a_vm_narrows_the_listing_to_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "").unwrap();
        save(&paths, "linux-aaaaaa", 4_000_000);
        save(&paths, "linux-bbbbbb", 4_000_001);

        let status = collect(
            &paths,
            &Config::default(),
            &FakeProbe(HashMap::new()),
            Some("linux-b"),
        )
        .unwrap();

        assert_eq!(status.listing.vms.len(), 1);
        assert_eq!(status.listing.vms[0].name, "linux-bbbbbb");
    }
}
