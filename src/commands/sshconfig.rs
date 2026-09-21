//! `vitro ssh-config` — give every ssh_config-aware tool a stable name for a VM.
//!
//! The forwarded port changes on every `run`, which is fine for `vitro exec` and
//! useless for `scp`, `rsync`, a debugger or an editor's remote connection.
//! Those all read `ssh_config`, so publishing a block per VM is what makes them
//! work without anyone copying a port number around.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{commands::exec, Config, Entry, Paths, ProcessProbe, Store, VmRecord};

/// The prefix that keeps vitro's aliases out of the way of real host names.
const ALIAS_PREFIX: &str = "vitro-";

pub fn alias(name: &str) -> String {
    format!("{ALIAS_PREFIX}{name}")
}

/// One `Host` block. The options mirror what `vitro exec` passes on the command
/// line, so reaching a VM through this block behaves the same way.
pub fn block(record: &VmRecord, target: &crate::ssh::SshTarget) -> String {
    let mut out = String::new();
    out.push_str(&format!("Host {}\n", alias(&record.name)));
    out.push_str(&format!("  HostName {}\n", target.host));
    out.push_str(&format!("  Port {}\n", target.port));
    out.push_str(&format!("  User {}\n", target.user));
    out.push_str(&format!("  IdentityFile {}\n", target.key.display()));
    out.push_str("  IdentitiesOnly yes\n");
    out.push_str(&format!(
        "  UserKnownHostsFile {}\n",
        target.known_hosts.display()
    ));
    out.push_str("  StrictHostKeyChecking accept-new\n");
    out
}

/// Blocks for every running VM, or for one named VM.
///
/// Dead VMs are left out on purpose: their forwarded port has been released and
/// may already belong to something else, so a block naming it would send a
/// connection somewhere unexpected.
pub fn render(
    paths: &Paths,
    config: &Config,
    probe: &impl ProcessProbe,
    name: Option<&str>,
) -> Result<String> {
    let store = Store::new(paths.state_dir());

    let records: Vec<VmRecord> = match name {
        Some(name) => vec![store.load(store.resolve(name)?)?],
        None => store
            .list()?
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Vm(record) => Some(record),
                Entry::Unreadable { .. } => None,
            })
            .collect(),
    };

    let blocks: Vec<String> = records
        .iter()
        .filter(|record| name.is_some() || record.liveness(probe) == crate::Liveness::Running)
        .map(|record| block(record, &exec::ssh_target(paths, config, record)))
        .collect();
    Ok(blocks.join("\n"))
}

/// Overwrite the include file vitro owns.
///
/// A file of our own rather than markers inside `~/.ssh/config`: editing
/// someone else's configuration in place invites conflicts, and a botched edit
/// there breaks every SSH connection they have, not just vitro's.
pub fn write(paths: &Paths, contents: &str) -> Result<PathBuf> {
    let path = paths.ssh_config();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    let mut text = String::from(
        "# Written by vitro. Edits are lost on the next update.\n\
         # Add `Include this-file` near the top of ~/.ssh/config to use it.\n\n",
    );
    text.push_str(contents);
    if !text.ends_with('\n') {
        text.push('\n');
    }
    // Written and renamed rather than truncated in place, the way the state
    // store writes records. This file is read by every SSH connection the user
    // makes once it is included, so a truncated one is not a local problem.
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("cannot write in {}", dir.display()))?;
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .with_context(|| format!("cannot write {}", path.display()))?;
    temp.persist(&path)
        .with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(path)
}

/// The user's own SSH configuration — the one file vitro is a guest in.
pub fn user_config(paths: &Paths) -> PathBuf {
    paths.home().join(".ssh").join("config")
}

/// Whether `~/.ssh/config` already pulls in the file vitro writes.
pub fn is_included(paths: &Paths) -> bool {
    let wanted = paths.ssh_config().display().to_string();
    fs::read_to_string(user_config(paths))
        .unwrap_or_default()
        .lines()
        .any(|line| {
            let line = line.trim();
            line.starts_with("Include") && line.contains(&wanted)
        })
}

/// Add the `Include` line to the user's own SSH configuration.
///
/// At the very top, and this is not cosmetic: OpenSSH keeps the first value it
/// sees for each option, and an `Include` after a `Host *` block is read too
/// late to set anything that block already set. It also has to be outside every
/// `Host` block, because an `Include` inside one applies only within it.
///
/// Nothing existing is rewritten — one line and a blank line go in front of
/// whatever is there — so the worst case is a line somebody deletes again.
pub fn add_include(paths: &Paths) -> Result<PathBuf> {
    let path = user_config(paths);
    if is_included(paths) {
        return Ok(path);
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;

    let existing = fs::read_to_string(&path).unwrap_or_default();
    let mut text = format!("Include {}\n\n", paths.ssh_config().display());
    text.push_str(&existing);

    fs::write(&path, &text).with_context(|| format!("cannot write {}", path.display()))?;
    // `ssh` refuses a configuration others can write, and says so in terms of
    // permissions rather than of the file, which sends people looking in the
    // wrong place.
    restrict_to_owner(&path);
    Ok(path)
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) {}

/// Bring the include file up to date, but only once the user has asked for it
/// to exist. `run` and `destroy` call this, so the aliases stay correct without
/// anyone remembering to refresh them — and without a file appearing in the
/// configuration directory of someone who never wanted one.
pub fn refresh_if_present(paths: &Paths, config: &Config, probe: &impl ProcessProbe) {
    if !paths.ssh_config().exists() {
        return;
    }
    let result = render(paths, config, probe, None).and_then(|text| write(paths, &text));
    if let Err(e) = result {
        eprintln!(
            "vitro: could not update {}: {e:#}",
            paths.ssh_config().display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, Golden, SysinfoProbe};
    use std::collections::HashMap;
    use time::OffsetDateTime;

    struct FakeProbe(HashMap<u32, u64>);

    impl ProcessProbe for FakeProbe {
        fn start_time(&self, pid: u32) -> Option<u64> {
            self.0.get(&pid).copied()
        }
    }

    fn record(name: &str, pid: u32, port: u16, dir: PathBuf) -> VmRecord {
        VmRecord {
            name: name.into(),
            image: "linux".into(),
            backend: Backend::Qemu,
            guest: Default::default(),
            pid,
            pid_start_time: 1_000,
            ssh_host: "127.0.0.1".into(),
            ssh_port: port,
            ssh_user: "dev".into(),
            golden: Golden::Image(PathBuf::from("/srv/linux.qcow2")),
            qmp_socket: None,
            dir,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn paths(root: &Path) -> Paths {
        Paths::from_env(&Default::default(), root)
    }

    #[test]
    fn a_block_carries_everything_needed_to_connect() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let record = record("linux-7f3a2c", 4_000_000, 53422, PathBuf::from("/state/vm"));

        let block = block(
            &record,
            &exec::ssh_target(&paths, &Config::default(), &record),
        );

        assert!(block.starts_with("Host vitro-linux-7f3a2c\n"), "{block}");
        assert!(block.contains("  HostName 127.0.0.1\n"), "{block}");
        assert!(block.contains("  Port 53422\n"), "{block}");
        assert!(block.contains("  User dev\n"), "{block}");
        assert!(block.contains("  IdentitiesOnly yes\n"), "{block}");
        // Built rather than spelled out: the separator in the rendered block is
        // the host's, and a literal `/state/vm/known_hosts` only matches on the
        // hosts that happen to use that one.
        let known_hosts = Path::new("/state/vm").join("known_hosts");
        assert!(
            block.contains(&known_hosts.display().to_string()),
            "{block}"
        );
    }

    #[test]
    fn a_dead_vm_is_left_out_so_nothing_connects_to_a_recycled_port() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let store = Store::new(paths.state_dir());
        store
            .save(&record(
                "linux-alive",
                4_000_001,
                53422,
                store.vm_dir("linux-alive"),
            ))
            .unwrap();
        store
            .save(&record(
                "linux-dead",
                4_000_002,
                53423,
                store.vm_dir("linux-dead"),
            ))
            .unwrap();
        let probe = FakeProbe(HashMap::from([(4_000_001, 1_000)]));

        let text = render(&paths, &Config::default(), &probe, None).unwrap();

        assert!(text.contains("Host vitro-linux-alive"), "{text}");
        assert!(!text.contains("vitro-linux-dead"), "{text}");
    }

    #[test]
    fn naming_a_vm_reports_it_even_when_it_is_not_running() {
        // Asking about one VM by name is a question, not a connection attempt;
        // an empty answer would read as "no such VM".
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());
        let store = Store::new(paths.state_dir());
        store
            .save(&record(
                "linux-dead",
                4_000_002,
                53423,
                store.vm_dir("linux-dead"),
            ))
            .unwrap();

        let text = render(
            &paths,
            &Config::default(),
            &FakeProbe(HashMap::new()),
            Some("linux-d"),
        )
        .unwrap();

        assert!(text.contains("Host vitro-linux-dead"), "{text}");
    }

    #[test]
    fn the_include_file_says_it_is_generated_and_how_to_use_it() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());

        let path = write(&paths, "Host vitro-linux-7f3a2c\n").unwrap();
        let text = fs::read_to_string(&path).unwrap();

        assert!(text.contains("Written by vitro"), "{text}");
        assert!(text.contains("Include"), "{text}");
        assert!(text.contains("Host vitro-linux-7f3a2c"), "{text}");
    }

    #[test]
    fn refreshing_does_nothing_until_the_user_has_asked_for_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths(temp.path());

        refresh_if_present(&paths, &Config::default(), &SysinfoProbe::new());

        assert!(!paths.ssh_config().exists());
    }
}
