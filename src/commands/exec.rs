//! `vitro exec` and `vitro ssh` — running something in a guest.
//!
//! The guest command's exit code is this tool's exit code. A failing build
//! inside the VM has to fail outside it, or vitro cannot be used for
//! verification at all, which is the only reason it exists.

use anyhow::{bail, Result};

use crate::{process, ssh, tools, Config, Paths, Store, SysinfoProbe, VmRecord};

pub const KNOWN_HOSTS: &str = "known_hosts";

pub fn exec(paths: &Paths, config: &Config, name: &str, command: &[String]) -> Result<i32> {
    let (record, target, ssh_binary) = prepare(paths, config, name)?;

    ssh::run_passthrough(
        &ssh_binary,
        &target,
        &ssh::SshOptions {
            // A pty and piped stdin cannot coexist: the pty never delivers
            // EOF. Only ask for one when vitro's own stdin is a terminal, in
            // which case there is nothing being piped in.
            tty: if ssh::stdin_is_terminal() {
                ssh::Tty::Yes
            } else {
                ssh::Tty::No
            },
            batch: true,
            connect_timeout: None,
            command: command.to_vec(),
            quoting: record.guest.into(),
            forwards: Vec::new(),
        },
    )
}

pub fn interactive(paths: &Paths, config: &Config, name: &str) -> Result<i32> {
    let (_, target, ssh_binary) = prepare(paths, config, name)?;

    ssh::run_passthrough(
        &ssh_binary,
        &target,
        &ssh::SshOptions {
            tty: ssh::Tty::Yes,
            // An interactive session may legitimately need to prompt.
            batch: false,
            connect_timeout: None,
            command: Vec::new(),
            quoting: ssh::Quoting::Posix,
            forwards: Vec::new(),
        },
    )
}

/// How to reach a VM over SSH.
///
/// The key comes from the image rather than from the record, so rotating it in
/// the configuration takes effect without restarting every VM.
pub fn ssh_target(paths: &Paths, config: &Config, record: &VmRecord) -> ssh::SshTarget {
    let key = config
        .image(&record.image)
        .map(|image| image.ssh_key.clone())
        .unwrap_or_else(|_| paths.ssh_key());

    ssh::SshTarget {
        host: record.ssh_host.clone(),
        port: record.ssh_port,
        user: record.ssh_user.clone(),
        key,
        known_hosts: record.dir.join(KNOWN_HOSTS),
    }
}

/// Load a VM that is expected to be running, and everything needed to talk to it.
pub fn prepare(
    paths: &Paths,
    config: &Config,
    name: &str,
) -> Result<(VmRecord, ssh::SshTarget, std::path::PathBuf)> {
    let store = Store::new(paths.state_dir());
    let name = store.resolve(name)?;
    let record = store.load(&name)?;

    if !process::is_running(&SysinfoProbe::new(), record.pid, record.pid_start_time) {
        bail!(
            "VM {name:?} is not running; `vitro ls` shows it as dead, and `vitro run` starts a new one"
        );
    }

    let cfg_tools = config.tools();
    let ssh_binary = tools::resolve("ssh", cfg_tools.ssh.as_deref(), "tools.ssh")?.path;
    let target = ssh_target(paths, config, &record);
    Ok((record, target, ssh_binary))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Backend, Golden, Paths as VitroPaths};
    use std::path::PathBuf;
    use time::OffsetDateTime;

    fn record(name: &str, dir: PathBuf) -> VmRecord {
        VmRecord {
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
            dir,
            created_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_vm_that_is_not_running_is_refused_with_the_next_step() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VitroPaths::from_env(&Default::default(), temp.path());
        let store = Store::new(paths.state_dir());
        store
            .save(&record("linux-7f3a2c", store.vm_dir("linux-7f3a2c")))
            .unwrap();

        let err = prepare(&paths, &Config::default(), "linux-7f3a2c")
            .unwrap_err()
            .to_string();

        assert!(err.contains("not running"), "{err}");
        assert!(err.contains("vitro run"), "{err}");
    }

    #[test]
    fn an_unknown_vm_names_itself_in_the_error() {
        let temp = tempfile::tempdir().unwrap();
        let paths = VitroPaths::from_env(&Default::default(), temp.path());

        let err = format!(
            "{:#}",
            prepare(&paths, &Config::default(), "nope").unwrap_err()
        );

        assert!(err.contains("nope"), "{err}");
    }
}
