//! `vitro forward` — reach a server running inside a guest.
//!
//! An `ssh -N -L` child held in the foreground, and nothing else. QEMU's user
//! networking can add a forward through the monitor, but that works only for
//! the QEMU backend and only where QMP is available, whereas this is the same
//! code for every backend and needs no resident process to manage.

use std::process::Command;

use anyhow::{Context, Result};

use crate::{commands::exec, ssh, Config, Paths};

pub fn forward(paths: &Paths, config: &Config, name: &str, ports: &[String]) -> Result<i32> {
    let forwards = ports
        .iter()
        .map(|spec| parse(spec))
        .collect::<Result<Vec<_>>>()?;
    let (record, target, ssh_binary) = exec::prepare(paths, config, name)?;

    for forward in &forwards {
        println!(
            "127.0.0.1:{} -> {}:{}",
            forward.host, record.name, forward.guest
        );
    }
    println!("press Ctrl-C to stop");

    let status = Command::new(&ssh_binary)
        .args(ssh::ssh_args(
            &target,
            &ssh::SshOptions {
                forwards,
                ..ssh::SshOptions::default()
            },
        ))
        .status()
        .with_context(|| format!("cannot run {}", ssh_binary.display()))?;

    // Interrupting the forward is how it is meant to end, and ssh reports the
    // signal rather than an exit code when that happens.
    Ok(status.code().unwrap_or(0))
}

/// `<hostport>` or `<hostport>:<guestport>`.
fn parse(spec: &str) -> Result<ssh::Forward> {
    let port = |text: &str| -> Result<u16> {
        text.parse::<u16>()
            .ok()
            .filter(|p| *p != 0)
            .with_context(|| format!("{text:?} is not a port number"))
    };
    match spec.split_once(':') {
        None => {
            let both = port(spec)?;
            Ok(ssh::Forward {
                host: both,
                guest: both,
            })
        }
        Some((host, guest)) => Ok(ssh::Forward {
            host: port(host)?,
            guest: port(guest)?,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_number_means_the_same_port_on_both_sides() {
        assert_eq!(
            parse("8080").unwrap(),
            ssh::Forward {
                host: 8080,
                guest: 8080
            }
        );
    }

    #[test]
    fn a_pair_maps_a_host_port_onto_a_different_guest_port() {
        assert_eq!(
            parse("8080:80").unwrap(),
            ssh::Forward {
                host: 8080,
                guest: 80
            }
        );
    }

    #[test]
    fn a_port_that_is_not_a_port_says_which_one() {
        for spec in ["", "http", "0", "70000", "8080:", "8080:0"] {
            let err = parse(spec).unwrap_err().to_string();
            assert!(err.contains("port number"), "{spec}: {err}");
        }
    }
}
