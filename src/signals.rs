//! Noticing Ctrl-C during a long wait.
//!
//! `run` and `build` spend most of their time waiting on a guest while holding
//! a QEMU process and a half-written directory. Cleanup is a `Drop`, and `Drop`
//! does not run when the default SIGINT handler terminates the process — so an
//! impatient Ctrl-C would leave an orphaned VM behind, which is exactly the
//! mess this tool exists to avoid.
//!
//! A handler cannot do the cleanup itself: almost nothing is safe to call from
//! one. It sets a flag instead, and the polling loops turn that into an
//! ordinary error, which unwinds through the guards the normal way.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Result};

static INTERRUPTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
mod imp {
    use super::{Ordering, INTERRUPTED};

    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;
    const SIG_DFL: usize = 0;

    extern "C" fn handle(signum: i32) {
        INTERRUPTED.store(true, Ordering::SeqCst);
        // Restore the default so a second Ctrl-C kills immediately. Someone who
        // presses it twice has decided not to wait for a tidy exit, and being
        // unable to leave would be worse than the debris.
        unsafe { signal(signum, SIG_DFL) };
    }

    pub fn install() {
        unsafe {
            signal(SIGINT, handle as *const () as usize);
            signal(SIGTERM, handle as *const () as usize);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn install() {}
}

/// Arrange for interruptions to be noticed instead of fatal. Call once, early.
pub fn install() {
    imp::install();
}

pub fn interrupted() -> bool {
    INTERRUPTED.load(Ordering::SeqCst)
}

/// Fail if the user has asked to stop, so a waiting loop unwinds through the
/// cleanup guards rather than being killed inside them.
pub fn check() -> Result<()> {
    if interrupted() {
        bail!("interrupted");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_interrupted_until_a_signal_arrives() {
        // The flag is process-wide, and no test raises a signal, so this also
        // guards against `check` reporting failure for everyone else.
        assert!(!interrupted());
        assert!(check().is_ok());
    }

    #[test]
    fn installing_twice_is_harmless() {
        install();
        install();
    }
}
