//! The command line surface.
//!
//! The whole surface is declared here even where the command is not built yet,
//! so `--help` describes the tool rather than the half of it that happens to
//! exist. Unimplemented commands fail immediately and say so.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "vitro",
    version,
    about = "Disposable development VMs driven from a CLI",
    // `vitro` with no arguments reports status rather than printing usage.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Build a golden image from installation media
    Build {
        /// An `[images.*]` key from the configuration
        image: String,
        /// Keep the working directory when the build fails
        #[arg(long)]
        keep_failed: bool,
    },

    /// Start a VM from a golden image
    Run {
        /// An `[images.*]` key from the configuration
        image: String,
        /// Name the VM instead of generating one
        #[arg(long)]
        name: Option<String>,
    },

    /// Run a command in a VM and pass its exit code through
    Exec {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },

    /// Open an interactive shell in a VM
    Ssh { name: String },

    /// Stop a VM and delete its overlay
    Destroy {
        name: Option<String>,
        /// Destroy every VM
        #[arg(long, conflicts_with = "name")]
        all: bool,
        /// Wait for the guest to shut down cleanly
        #[arg(long)]
        graceful: bool,
    },

    /// Turn a running VM into a new golden image
    Promote {
        name: String,
        /// Write the new golden image here instead of the generated name
        #[arg(long = "as")]
        as_name: Option<String>,
        /// Keep the stopped VM instead of destroying it
        #[arg(long)]
        keep: bool,
        /// Overwrite an existing golden image
        #[arg(long)]
        force: bool,
    },

    /// List VMs and golden images
    Ls {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
        /// Remove records whose process is gone
        #[arg(long)]
        prune: bool,
    },

    /// Print an ssh_config block for one or every VM
    SshConfig {
        name: Option<String>,
        /// Update the include file vitro owns
        #[arg(long)]
        write: bool,
    },

    /// Forward guest ports to the host until interrupted
    Forward {
        name: String,
        /// `<hostport>[:<guestport>]`
        #[arg(required = true)]
        ports: Vec<String>,
    },

    /// Print a VM's SSH port and nothing else
    Port { name: String },

    /// Print a VM's record as JSON
    Inspect { name: String },

    /// Check everything `run` and `build` depend on
    Doctor,

    /// Show what is running and what to do next
    Status {
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },

    /// Save a picture of the guest's screen
    Screenshot {
        name: String,
        #[arg(long, value_name = "PATH")]
        out: Option<PathBuf>,
    },

    /// Start a program on the guest's desktop
    Launch {
        name: String,
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn no_arguments_means_status() {
        let cli = Cli::try_parse_from(["vitro"]).unwrap();
        assert!(cli.command.is_none());
    }

    #[test]
    fn exec_keeps_the_guest_command_intact() {
        let cli =
            Cli::try_parse_from(["vitro", "exec", "vm1", "cargo", "test", "--", "-q"]).unwrap();

        let Some(Command::Exec { name, command }) = cli.command else {
            panic!("expected exec");
        };
        assert_eq!(name, "vm1");
        assert_eq!(command, ["cargo", "test", "--", "-q"]);
    }

    #[test]
    fn destroy_takes_a_name_or_all_but_not_both() {
        assert!(Cli::try_parse_from(["vitro", "destroy", "vm1"]).is_ok());
        assert!(Cli::try_parse_from(["vitro", "destroy", "--all"]).is_ok());
        assert!(Cli::try_parse_from(["vitro", "destroy", "vm1", "--all"]).is_err());
    }

    #[test]
    fn an_unknown_subcommand_is_a_usage_error() {
        let err = Cli::try_parse_from(["vitro", "summon"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
