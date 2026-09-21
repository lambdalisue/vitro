use std::io::Write;
use std::process::ExitCode;

use anyhow::{bail, Result};
use clap::Parser;

use vitro::cli::{Cli, Command};
use vitro::commands::{ls, status};
use vitro::{Config, Paths, Store, SysinfoProbe};

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

    let filter = match EnvFilter::try_from_env("VITRO_LOG") {
        Ok(filter) => filter,
        Err(_) => return,
    };
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

fn dispatch(cli: Cli) -> Result<i32> {
    let paths = Paths::resolve()?;
    let config = load_config(&paths)?;

    match cli.command {
        None => show_status(&paths, &config, None, false),

        Some(Command::Ls { json, prune }) => {
            if prune {
                bail!("`--prune` needs the VM lifecycle, which is not built yet");
            }
            let store = Store::new(paths.state_dir());
            let listing = ls::collect(&store, &config, &SysinfoProbe::new(), ls::now())?;

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

        Some(Command::Status { name, json }) => show_status(&paths, &config, name.as_deref(), json),

        // The command line describes the whole tool on purpose, so the parts
        // that are not built yet have to say so rather than silently do
        // nothing. See `cli.rs`.
        Some(_) => bail!("that command is not built yet"),
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
