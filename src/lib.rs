//! vitro — disposable development VMs driven from a CLI.
//!
//! Everything that builds an argument list, merges configuration or renders
//! output is a pure function; only a thin layer spawns processes or touches
//! the filesystem. The pure half is where the tests live.

mod config;
mod paths;
mod state;
mod units;

pub mod cli;
pub mod commands;
pub mod golden;
pub mod image;
pub mod iso;
pub mod lume;
pub mod media;
pub mod process;
pub mod qemu;
pub mod qmp;
pub mod seed;
pub mod signals;
pub mod ssh;
pub mod tools;
pub mod unattend;

pub use config::*;
pub use paths::*;
pub use state::*;
pub use units::*;
