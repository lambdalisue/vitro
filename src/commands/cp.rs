//! `vitro cp` — copy files between the host and a VM.
//!
//! `scp -r` with the connection vitro already knows: the forwarded port, the
//! key, and the VM's own known_hosts. An operand written `<vm>:<path>` is in
//! the guest; anything else is on the host.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::{commands::exec, ssh, tools, Config, Paths, Store, SysinfoProbe};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Operand {
    Host(PathBuf),
    /// `vm` is `None` for `:<path>`, which means the one running VM.
    Guest {
        vm: Option<String>,
        path: String,
    },
}

/// Which way a copy goes, with every operand checked to agree on it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    vm: Option<String>,
    sources: Vec<ssh::CopyEnd>,
    destination: ssh::CopyEnd,
}

pub fn cp(paths: &Paths, config: &Config, operands: &[String]) -> Result<i32> {
    let plan = plan(operands, cfg!(windows))?;
    let name =
        Store::new(paths.state_dir()).resolve_or_sole(plan.vm.as_deref(), &SysinfoProbe::new())?;
    let (_, target, _) = exec::prepare(paths, config, &name)?;
    let scp = tools::resolve("scp", config.tools().scp.as_deref(), "tools.scp")?.path;

    let status = Command::new(&scp)
        .args(ssh::copy_args(&target, &plan.sources, &plan.destination))
        .status()
        .with_context(|| format!("cannot run {}", scp.display()))?;
    // scp has already said what went wrong; its code is the answer.
    Ok(status.code().unwrap_or(255))
}

fn plan(operands: &[String], windows_host: bool) -> Result<Plan> {
    let Some((last, sources)) = operands.split_last() else {
        bail!("name at least one source and a destination");
    };
    if sources.is_empty() {
        bail!("name at least one source and a destination");
    }
    let destination = parse(last, windows_host);
    let sources: Vec<Operand> = sources.iter().map(|s| parse(s, windows_host)).collect();

    match destination {
        Operand::Guest { vm, path } => {
            let mut local = Vec::new();
            for source in sources {
                match source {
                    Operand::Host(path) => local.push(ssh::CopyEnd::Local(path)),
                    Operand::Guest { .. } => {
                        bail!(
                            "copy from the host to a VM or from a VM to the host, not between VMs"
                        )
                    }
                }
            }
            Ok(Plan {
                vm,
                sources: local,
                destination: ssh::CopyEnd::Guest(path),
            })
        }
        Operand::Host(destination) => {
            let mut vm: Option<Option<String>> = None;
            let mut remote = Vec::new();
            for source in sources {
                match source {
                    Operand::Guest { vm: this, path } => {
                        if vm.as_ref().is_some_and(|seen| *seen != this) {
                            bail!("every source has to be in the same VM");
                        }
                        vm = Some(this);
                        remote.push(ssh::CopyEnd::Guest(path));
                    }
                    Operand::Host(_) => {
                        bail!("one side has to be a VM: write it as <vm>:<path>")
                    }
                }
            }
            Ok(Plan {
                vm: vm.flatten(),
                sources: remote,
                destination: ssh::CopyEnd::Local(destination),
            })
        }
    }
}

/// A colon makes an operand a guest path unless what comes before it could
/// only be part of a host path: it contains a separator, or on Windows it is a
/// drive letter.
fn parse(text: &str, windows_host: bool) -> Operand {
    if let Some((vm, path)) = text.split_once(':') {
        let drive = windows_host && vm.len() == 1 && vm.chars().all(|c| c.is_ascii_alphabetic());
        let has_separator = vm.contains('/') || vm.contains('\\');
        if !drive && !has_separator {
            return Operand::Guest {
                vm: (!vm.is_empty()).then(|| vm.to_string()),
                path: path.to_string(),
            };
        }
    }
    Operand::Host(PathBuf::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_vm_name_and_a_colon_mark_a_guest_path() {
        assert_eq!(
            parse("linux:src/main.rs", false),
            Operand::Guest {
                vm: Some("linux".into()),
                path: "src/main.rs".into()
            }
        );
    }

    #[test]
    fn a_bare_colon_means_the_one_running_vm() {
        assert_eq!(
            parse(":out.exe", false),
            Operand::Guest {
                vm: None,
                path: "out.exe".into()
            }
        );
    }

    #[test]
    fn an_empty_guest_path_is_the_guest_home() {
        assert_eq!(
            parse("linux:", false),
            Operand::Guest {
                vm: Some("linux".into()),
                path: String::new()
            }
        );
    }

    #[test]
    fn a_path_without_a_colon_is_on_the_host() {
        assert_eq!(parse("./src", false), Operand::Host("./src".into()));
    }

    #[test]
    fn a_colon_after_a_separator_is_part_of_a_host_path() {
        assert_eq!(parse("./a:b", false), Operand::Host("./a:b".into()));
    }

    #[test]
    fn a_drive_letter_is_a_host_path_on_windows_and_a_vm_elsewhere() {
        assert_eq!(parse(r"C:\src", true), Operand::Host(r"C:\src".into()));
        assert!(matches!(parse("c:src", false), Operand::Guest { .. }));
    }

    #[test]
    fn a_guest_path_on_windows_may_carry_its_own_drive() {
        assert_eq!(
            parse("win:C:/work", true),
            Operand::Guest {
                vm: Some("win".into()),
                path: "C:/work".into()
            }
        );
    }

    #[test]
    fn host_sources_and_a_guest_destination_copy_into_the_guest() {
        let plan = plan(&strings(&["src", "Cargo.toml", "win:project"]), false).unwrap();

        assert_eq!(plan.vm.as_deref(), Some("win"));
        assert_eq!(
            plan.sources,
            [
                ssh::CopyEnd::Local("src".into()),
                ssh::CopyEnd::Local("Cargo.toml".into())
            ]
        );
        assert_eq!(plan.destination, ssh::CopyEnd::Guest("project".into()));
    }

    #[test]
    fn guest_sources_and_a_host_destination_copy_out_of_the_guest() {
        let plan = plan(&strings(&[":a.exe", ":b.pdb", "out"]), false).unwrap();

        assert_eq!(plan.vm, None);
        assert_eq!(
            plan.sources,
            [
                ssh::CopyEnd::Guest("a.exe".into()),
                ssh::CopyEnd::Guest("b.pdb".into())
            ]
        );
        assert_eq!(plan.destination, ssh::CopyEnd::Local("out".into()));
    }

    #[test]
    fn a_copy_with_no_guest_side_is_refused() {
        let err = plan(&strings(&["a", "b"]), false).unwrap_err().to_string();

        assert!(err.contains("<vm>:<path>"), "{err}");
    }

    #[test]
    fn a_copy_between_two_vms_is_refused() {
        assert!(plan(&strings(&["a:x", "b:y"]), false).is_err());
        assert!(plan(&strings(&["a:x", "b:y", "out"]), false).is_err());
    }

    #[test]
    fn a_copy_mixing_host_and_guest_sources_is_refused() {
        assert!(plan(&strings(&["a:x", "local", "out"]), false).is_err());
    }

    #[test]
    fn a_lone_operand_is_not_a_copy() {
        assert!(plan(&strings(&["a:x"]), false).is_err());
    }
}
