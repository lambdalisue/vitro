//! `vitro promote` — keep what you just did to a VM.
//!
//! The counterpart to `build`: instead of installing an operating system from
//! media, it takes a VM someone has already set up the way they want and turns
//! its disk into the next golden image. The VM is shut down first, because
//! flattening a disk with a running filesystem on it produces an image that
//! boots into a repair.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use time::OffsetDateTime;

use crate::{
    commands::{destroy, exec, run},
    golden, lume, process, signals, tools, Backend, Config, Paths, Store,
};

pub struct Options {
    pub name: String,
    pub as_name: Option<String>,
    pub keep: bool,
    pub force: bool,
}

pub struct Promoted {
    pub golden: PathBuf,
    pub vm: String,
    /// The image key the VM came from, so the caller can tell whether the
    /// result is one a later `run` would pick up by itself.
    pub image: String,
    pub kept: bool,
}

pub fn promote(paths: &Paths, config: &Config, options: &Options) -> Result<Promoted> {
    let (record, target, ssh_binary) = exec::prepare(paths, config, &options.name)?;
    if record.backend == Backend::Lume {
        return promote_lume(paths, config, &record, options);
    }

    let destination = destination(
        paths,
        &record.image,
        options.as_name.as_deref(),
        OffsetDateTime::now_utc(),
    );
    if destination.exists() && !options.force {
        bail!(
            "{} already exists; pass --force to replace it",
            destination.display()
        );
    }

    let cfg_tools = config.tools();
    let qemu_img =
        tools::resolve("qemu-img", cfg_tools.qemu_img.as_deref(), "tools.qemu_img")?.path;

    // Everything that can be checked is checked before the guest is stopped.
    // A promote that fails after the shutdown leaves a VM nobody asked to stop,
    // and the next `run` reaps its record — taking the work being promoted.
    let overlay = record.dir.join(run::OVERLAY_FILE);
    if !overlay.exists() {
        bail!(
            "{} has no disk at {}; it may have been cleaned up already",
            record.name,
            overlay.display()
        );
    }

    // Held from the shutdown until the record is gone. In between the VM looks
    // dead to everyone else, and `vitro run` reaps dead records — which would
    // delete the very overlay being converted, along with the work in it.
    let store = Store::new(paths.state_dir());
    let _lock = store.lock()?;

    golden::shut_down(
        &ssh_binary,
        &target,
        record.guest,
        record.pid,
        record.qmp_socket.as_deref(),
    )?;

    signals::check()?;
    golden::install(&qemu_img, &overlay, &destination)?;

    // The VM is powered off either way; `--keep` only decides whether its
    // overlay and record survive for another look.
    if !options.keep {
        process::stop_and_wait(record.pid, Some(record.pid_start_time));
        destroy::remove(&store, &record.name, record.qmp_socket.as_deref())?;
    }

    Ok(Promoted {
        golden: destination,
        image: record.image.clone(),
        vm: record.name,
        kept: options.keep,
    })
}

/// Promote a macOS guest by cloning it inside `lume`.
///
/// The golden image is a VM name for this backend, so there is nothing to
/// flatten: stopping the guest and cloning it is the whole operation, and on
/// APFS the clone costs seconds whatever the VM's size.
fn promote_lume(
    paths: &Paths,
    config: &Config,
    record: &crate::VmRecord,
    options: &Options,
) -> Result<Promoted> {
    let binary = tools::resolve("lume", config.tools().lume.as_deref(), "tools.lume")?.path;
    let destination = options.as_name.clone().unwrap_or_else(|| {
        format!(
            "{}-{}",
            record.image,
            golden::stamp(OffsetDateTime::now_utc())
        )
    });

    if destination.contains('/') {
        bail!("a lume golden image is a VM name, not a path ({destination:?})");
    }
    if lume::get(&binary, &destination).is_ok() && !options.force {
        bail!("a VM named {destination:?} already exists; pass --force to replace it");
    }

    let store = Store::new(paths.state_dir());
    let _lock = store.lock()?;

    // Stopped rather than shut down from inside: `lume shutdown` wants the
    // guest account's password, which vitro does not keep — it authenticates
    // with a key. `lume stop` is Virtualization.framework's own request to the
    // guest, not a power cut.
    lume::run(&binary, &lume::stop_args(&record.name))?;

    signals::check()?;
    if options.force {
        let _ = lume::run(&binary, &lume::delete_args(&destination));
    }
    lume::run(&binary, &lume::clone_args(&record.name, &destination))?;

    if !options.keep {
        lume::run(&binary, &lume::delete_args(&record.name))?;
        destroy::remove(&store, &record.name, record.qmp_socket.as_deref())?;
    }

    Ok(Promoted {
        golden: PathBuf::from(destination),
        image: record.image.clone(),
        vm: record.name.clone(),
        kept: options.keep,
    })
}

/// Where the new image goes.
///
/// `--as` naming a bare file lands it beside the other golden images, which is
/// what someone typing `--as linux-with-rust` means; anything with a directory
/// component is taken literally.
fn destination(
    paths: &Paths,
    image_key: &str,
    as_name: Option<&str>,
    now: OffsetDateTime,
) -> PathBuf {
    let Some(as_name) = as_name else {
        return golden::dated_path(&paths.golden_dir(), image_key, now);
    };
    let given = Path::new(as_name);
    let bare = given
        .parent()
        .is_none_or(|parent| parent.as_os_str().is_empty());
    let named = if given.extension().is_some() {
        PathBuf::from(as_name)
    } else {
        PathBuf::from(format!("{as_name}.qcow2"))
    };
    if bare {
        paths.golden_dir().join(named)
    } else {
        named
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn paths(root: &Path) -> Paths {
        Paths::from_env(&Default::default(), root)
    }

    #[test]
    fn without_a_name_the_image_is_dated_and_keyed_by_image() {
        let paths = paths(Path::new("/home/dev"));

        let path = destination(&paths, "linux", None, datetime!(2026-09-20 21:16:33 UTC));

        assert!(path.starts_with(paths.golden_dir()), "{}", path.display());
        assert_eq!(path.file_name().unwrap(), "linux-20260920-211633.qcow2");
    }

    #[test]
    fn a_bare_name_lands_beside_the_other_golden_images() {
        let paths = paths(Path::new("/home/dev"));

        let path = destination(
            &paths,
            "linux",
            Some("linux-with-rust"),
            datetime!(2026-09-20 21:16:33 UTC),
        );

        assert_eq!(
            path,
            paths.golden_dir().join("linux-with-rust.qcow2"),
            "{}",
            path.display()
        );
    }

    #[test]
    fn a_name_with_a_directory_is_taken_literally() {
        let paths = paths(Path::new("/home/dev"));

        let path = destination(
            &paths,
            "linux",
            Some("/srv/images/base.qcow2"),
            datetime!(2026-09-20 21:16:33 UTC),
        );

        assert_eq!(path, Path::new("/srv/images/base.qcow2"));
    }
}
