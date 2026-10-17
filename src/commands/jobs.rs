//! `vitro exec --detach`, `vitro attach` and `vitro jobs` — commands left
//! running in a guest, and watching them.
//!
//! Fourteen sessions ran long builds in a background task of their own and
//! seven hand-rolled `Start-Process` or `nohup` with a log to poll, because a
//! build that outlives one SSH connection has nowhere to report back to.

use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;

use crate::commands::exec;
use crate::jobs::{self, JobRef, Status};
use crate::{ssh, tools, Config, Paths, Store, SysinfoProbe};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How many polls in a row may fail to reach the guest before attaching gives
/// up: a guest restarting under a job is worth riding out, a gone one is not.
const UNREACHABLE_POLLS: u32 = 120;

/// Start `command` as a job in VM `name` and return its address.
pub fn start(
    paths: &Paths,
    config: &Config,
    name: &str,
    command: &[String],
    sync: Option<&Path>,
    mut prelude: ssh::Prelude,
) -> Result<String> {
    let (record, target, ssh_binary) = exec::prepare(paths, config, name)?;
    if let Some(dir) = sync {
        let scp = tools::resolve("scp", config.tools().scp.as_deref(), "tools.scp")?.path;
        let landed = crate::sync::sync(&ssh_binary, &scp, &target, record.guest, dir)?;
        prelude.cwd.get_or_insert(landed);
    }
    prelude.fresh_path = true;

    let id = jobs::new_id();
    let options = ssh::SshOptions {
        command: jobs::start(record.guest, &id, command, &prelude),
        quoting: record.guest.into(),
        ..ssh::SshOptions::default()
    };
    match ssh::run_output(&ssh_binary, &target, &options)? {
        (0, _) => Ok(format!("{}:{id}", record.name)),
        (code, _) => bail!("could not start the job in {} (exit {code})", record.name),
    }
}

/// Follow a job's output until it ends, and return its exit code.
///
/// Interrupting stops the following, not the job: it goes on in the guest and
/// can be attached to again.
pub fn attach(paths: &Paths, config: &Config, job: &JobRef, out: &mut impl Write) -> Result<i32> {
    let name =
        Store::new(paths.state_dir()).resolve_or_sole(job.vm.as_deref(), &SysinfoProbe::new())?;
    let (record, target, ssh_binary) = exec::prepare(paths, config, &name)?;
    let mut offset = 0u64;
    let mut unreachable = 0;
    loop {
        if crate::signals::interrupted() {
            eprintln!(
                "vitro: {name}:{} goes on running; `vitro attach {name}:{}` follows it again",
                job.id, job.id
            );
            return Ok(130);
        }
        let options = ssh::SshOptions {
            command: jobs::poll(record.guest, &job.id, offset),
            quoting: record.guest.into(),
            connect_timeout: Some(Duration::from_secs(5)),
            ..ssh::SshOptions::default()
        };
        let (code, text) = ssh::run_output(&ssh_binary, &target, &options)?;
        match code {
            0 => unreachable = 0,
            3 => bail!(
                "{name} has no job {}; `vitro jobs {name}` lists them",
                job.id
            ),
            _ => {
                unreachable += 1;
                if unreachable >= UNREACHABLE_POLLS {
                    bail!("lost touch with {name} while following {}", job.id);
                }
                std::thread::sleep(POLL_INTERVAL);
                continue;
            }
        }
        let (status, chunk) = split_poll(&text)?;
        if !chunk.is_empty() {
            out.write_all(&chunk)?;
            out.flush()?;
            offset += chunk.len() as u64;
        }
        match status {
            Status::Exited(code) => return Ok(code),
            Status::Lost => bail!(
                "{name}:{} stopped without an exit code; the guest restarted, or the job was killed",
                job.id
            ),
            Status::Running => std::thread::sleep(POLL_INTERVAL),
        }
    }
}

/// Every job in VM `name`: id, status, command.
pub fn list(paths: &Paths, config: &Config, name: &str) -> Result<Vec<(String, Status, String)>> {
    let (record, target, ssh_binary) = exec::prepare(paths, config, name)?;
    let options = ssh::SshOptions {
        command: jobs::list(record.guest),
        quoting: record.guest.into(),
        ..ssh::SshOptions::default()
    };
    let (code, text) = ssh::run_output(&ssh_binary, &target, &options)?;
    if code != 0 {
        bail!("could not list the jobs in {} (exit {code})", record.name);
    }
    Ok(parse_list(&text))
}

/// A poll's output: the status on the first line, then base64.
fn split_poll(text: &str) -> Result<(Status, Vec<u8>)> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let status = jobs::parse_status(first).with_context(|| {
        format!("the guest reported a job status vitro does not know: {first:?}")
    })?;
    let encoded: String = rest.chars().filter(|c| !c.is_whitespace()).collect();
    let chunk = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .context("the guest sent output vitro could not decode")?;
    Ok((status, chunk))
}

fn parse_list(text: &str) -> Vec<(String, Status, String)> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let id = fields.next()?.trim().to_string();
            let status = jobs::parse_status(fields.next()?)?;
            let command = fields.next().unwrap_or("").trim().to_string();
            Some((id, status, command))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poll_is_a_status_line_then_the_output_in_base64() {
        // base64 wraps its lines on Linux; the wrapping is not the output.
        let (status, chunk) = split_poll("running\naGVsbG8g\nd29ybGQK\n").unwrap();

        assert_eq!(status, Status::Running);
        assert_eq!(chunk, b"hello world\n");
    }

    #[test]
    fn a_poll_with_nothing_new_is_just_the_status() {
        let (status, chunk) = split_poll("exit:3\r\n").unwrap();

        assert_eq!(status, Status::Exited(3));
        assert!(chunk.is_empty());
    }

    #[test]
    fn a_status_vitro_does_not_know_is_an_error_not_a_guess() {
        assert!(split_poll("sleeping\n").is_err());
    }

    #[test]
    fn the_listing_is_id_status_command_per_line() {
        let jobs = parse_list("a3f9c2\trunning\tcargo build\n0b1c2d\texit:1\tmake check\n\n");

        assert_eq!(
            jobs,
            [
                (
                    "a3f9c2".to_string(),
                    Status::Running,
                    "cargo build".to_string()
                ),
                (
                    "0b1c2d".to_string(),
                    Status::Exited(1),
                    "make check".to_string()
                ),
            ]
        );
    }
}
