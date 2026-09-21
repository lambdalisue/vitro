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
pub mod process;
pub mod signals;
pub mod tools;

pub use config::*;
pub use paths::*;
pub use state::*;
pub use units::*;
