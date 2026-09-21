use std::io::Write;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::Parser;

use vitro::cli::{Cli, Command};
use vitro::commands::{
    build, destroy, doctor, exec, forward, inspect, keygen, ls, promote, run, setup, sshconfig,
    status,
};
use vitro::{golden, Config, Golden, Paths, Store, SysinfoProbe};

/// Exit codes, as promised to callers: 0 success, 1 failure, 2 bad command
/// line. clap already uses 2 for its own parse errors.
const FAILURE: u8 = 1;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing();
    vitro::signals::install();

    match dispatch(cli) {
        // `exec` and `ssh` hand back the guest's exit code, which is the whole
        // point of them: a failing build inside the VM must fail outside it.
        Ok(code) => ExitCode::from(u8::try_from(code).unwrap_or(FAILURE)),
        Err(e) => {
            // `{:#}` prints the whole context chain on one line, which is what
            // makes `in /…/config.toml: image "linux": ...` readable.
            eprintln!("vitro: {e:#}");
            ExitCode::from(FAILURE)
        }
    }
}

/// Logging is off unless asked for. When a guest will not start, the useful
/// artifacts are `serial.log` and the exact command line, both of which vitro
/// reports without needing a log level.
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_env("VITRO_LOG").unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(false)
        .try_init();
}

fn dispatch(cli: Cli) -> Result<i32> {
    let paths = Paths::resolve()?;
    let config = load_config(&paths)?;

    match cli.command {
        None => show_status(&paths, &config, None, false),

        Some(Command::Ls { json, prune }) => {
            let store = Store::new(paths.state_dir());
            if prune {
                // stderr, so `ls --json --prune` still emits only JSON.
                for name in run::reap_dead(&store, &SysinfoProbe::new())? {
                    eprintln!("pruned {name}");
                }
                sshconfig::refresh_if_present(&paths, &config, &SysinfoProbe::new());
            }
            let listing = ls::collect(&paths, &store, &config, &SysinfoProbe::new(), ls::now())?;

            let mut stdout = std::io::stdout().lock();
            if json {
                serde_json::to_writer_pretty(&mut stdout, &listing)?;
                stdout.write_all(b"\n")?;
            } else {
                stdout.write_all(ls::render(&listing).as_bytes())?;
                if !paths.config_file().exists() {
                    writeln!(
                        stdout,
                        "\nno configuration at {}; `vitro doctor` will say what to put there",
                        paths.config_file().display()
                    )?;
                }
            }
            Ok(0)
        }

        Some(Command::Build { image, keep_failed }) => {
            let image_key = image.clone();
            let built = build::build(&paths, &config, &build::Options { image, keep_failed })?;
            println!(
                "built {} in {}",
                built.golden.display(),
                humantime_took(built.took)
            );
            if let Some(password) = built.password {
                // Nothing else records it, and a guest that stops answering on
                // its key has no other way in.
                println!("the guest account's password is {password}");
            }
            if let Some(hint) = repoint_hint(&paths, &config, &image_key, &built.golden) {
                println!("{hint}");
            }
            Ok(0)
        }

        Some(Command::Run { image, name }) => {
            let record = run::run(&paths, &config, run::Options { image, name })?;
            sshconfig::refresh_if_present(&paths, &config, &SysinfoProbe::new());
            println!(
                "{} is up: ssh {}@{} -p {}",
                record.name, record.ssh_user, record.ssh_host, record.ssh_port
            );
            Ok(0)
        }

        Some(Command::Exec { name, command }) => exec::exec(&paths, &config, &name, &command),

        Some(Command::Promote {
            name,
            as_name,
            keep,
            force,
        }) => {
            let promoted = promote::promote(
                &paths,
                &config,
                &promote::Options {
                    name,
                    as_name,
                    keep,
                    force,
                },
            )?;
            sshconfig::refresh_if_present(&paths, &config, &SysinfoProbe::new());
            println!("promoted {} to {}", promoted.vm, promoted.golden.display());
            if promoted.kept {
                // Being explicit because the record is indistinguishable from a
                // crashed VM, and the next `run` reaps it on those grounds.
                println!(
                    "{} is stopped and will be cleared by the next `vitro run`",
                    promoted.vm
                );
            }
            if let Some(hint) = repoint_hint(&paths, &config, &promoted.image, &promoted.golden) {
                println!("{hint}");
            }
            Ok(0)
        }

        Some(Command::SshConfig { name, write }) => {
            let text = sshconfig::render(&paths, &config, &SysinfoProbe::new(), name.as_deref())?;
            if write {
                let path = sshconfig::write(&paths, &text)?;
                println!("wrote {}", path.display());
            } else {
                print!("{text}");
            }
            Ok(0)
        }

        Some(Command::Forward { name, ports }) => forward::forward(&paths, &config, &name, &ports),

        Some(Command::Port { name }) => {
            println!("{}", inspect::port(&paths, &SysinfoProbe::new(), &name)?);
            Ok(0)
        }

        Some(Command::Inspect { name }) => {
            let inspected = inspect::inspect(&paths, &SysinfoProbe::new(), &name)?;
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, &inspected)?;
            stdout.write_all(b"\n")?;
            Ok(0)
        }

        Some(Command::Setup) => setup::setup(&paths, &config),

        Some(Command::Keygen { force }) => {
            let generated = keygen::keygen(&paths, &config, force)?;
            if generated.existed {
                println!(
                    "{} already exists; pass --force to replace it",
                    generated.path.display()
                );
            } else {
                println!("wrote {}", generated.path.display());
            }
            Ok(0)
        }

        Some(Command::Doctor) => doctor::report(&paths, &config),

        Some(Command::Status { name, json }) => show_status(&paths, &config, name.as_deref(), json),

        Some(Command::Ssh { name }) => exec::interactive(&paths, &config, &name),

        // The command line describes the whole tool on purpose, so the parts
        // that are not built yet have to say so rather than silently do
        // nothing. See `cli.rs`.
        Some(Command::Screenshot { .. } | Command::Launch { .. }) => {
            bail!("that command is not built yet")
        }

        Some(Command::Destroy {
            name,
            all,
            graceful,
        }) => {
            let options = destroy::Options { graceful };
            let outcomes = match (name, all) {
                (Some(name), _) => vec![destroy::destroy_one(&paths, &config, &name, &options)?],
                (None, true) => destroy::destroy_all(&paths, &config, &options)?,
                (None, false) => bail!("name a VM to destroy, or pass --all"),
            };
            sshconfig::refresh_if_present(&paths, &config, &SysinfoProbe::new());
            if outcomes.is_empty() {
                println!("no VMs to destroy");
            }
            let mut failed = false;
            for outcome in outcomes {
                match outcome {
                    destroy::Outcome::Stopped(name) => println!("stopped {name}"),
                    destroy::Outcome::CleanedUp(name) => println!("cleaned up {name}"),
                    destroy::Outcome::Failed { name, reason } => {
                        failed = true;
                        eprintln!("vitro: could not destroy {name}: {reason}");
                    }
                }
            }
            if failed {
                bail!("some VMs could not be destroyed");
            }
            Ok(0)
        }
    }
}

fn show_status(paths: &Paths, config: &Config, name: Option<&str>, json: bool) -> Result<i32> {
    let status = status::collect(paths, config, &SysinfoProbe::new(), name)?;
    let mut stdout = std::io::stdout().lock();
    if json {
        serde_json::to_writer_pretty(&mut stdout, &status)?;
        stdout.write_all(b"\n")?;
    } else {
        stdout.write_all(status::render(&status).as_bytes())?;
    }
    Ok(0)
}

/// A missing configuration file is not an error. A fresh installation has none,
/// and `ls` has to work there — it is the command that tells you so.
fn load_config(paths: &Paths) -> Result<Config> {
    let path = paths.config_file();
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::load(&path, paths.home(), &paths.ssh_key())
}

/// What to say after an image has landed somewhere the next `run` will not
/// look by itself.
///
/// Nothing at all in the ordinary case: with `golden` unset, a dated image is
/// picked up on its own, and telling somebody to go and edit a file they do not
/// need to edit is how a tool ends up needing expertise to use. A pinned
/// `golden`, or a `promote --as` name that no `run` would choose, still has to
/// be said.
fn repoint_hint(
    paths: &Paths,
    config: &Config,
    image_key: &str,
    produced: &std::path::Path,
) -> Option<String> {
    let image = config.image(image_key).ok()?;
    let picked_up = matches!(image.golden, Golden::Latest)
        && golden::newest(&paths.golden_dir(), image_key).as_deref() == Some(produced);
    (!picked_up).then(|| {
        format!(
            "point `golden` at it in {} to use it by default",
            paths.config_file().display()
        )
    })
}

/// Minutes and seconds; a build long enough to matter is never sub-second.
fn humantime_took(took: std::time::Duration) -> String {
    let secs = took.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}
