//! `qemu-img` calls: overlays, normalisation and integrity checks.
//!
//! A golden image is only ever read. Overlays are made with `create -b`, and a
//! promotion is a `convert` into a fresh file — never `qemu-img commit`, which
//! writes the overlay's contents back into the golden image and would destroy
//! the one artifact worth keeping.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::ByteSize;

/// Build the argument list for a copy-on-write overlay.
///
/// The backing path is recorded exactly as given. It is resolved relative to
/// the *overlay's* directory when the image is opened, not relative to the
/// working directory at creation time, so passing a relative path that looks
/// right from here produces an image that cannot be opened at all.
pub fn create_overlay_args(golden: &Path, overlay: &Path, size: Option<ByteSize>) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec![
        OsString::from("create"),
        OsString::from("-f"),
        OsString::from("qcow2"),
        OsString::from("-F"),
        OsString::from("qcow2"),
        OsString::from("-b"),
        golden.to_path_buf().into_os_string(),
        overlay.to_path_buf().into_os_string(),
    ];
    if let Some(size) = size {
        args.push(OsString::from(size.bytes().to_string()));
    }
    args
}

pub fn create_blank_args(disk: &Path, size: ByteSize) -> Vec<OsString> {
    vec![
        OsString::from("create"),
        OsString::from("-f"),
        OsString::from("qcow2"),
        disk.to_path_buf().into_os_string(),
        OsString::from(size.bytes().to_string()),
    ]
}

/// Flatten an overlay into a standalone image.
pub fn convert_args(from: &Path, to: &Path) -> Vec<OsString> {
    vec![
        OsString::from("convert"),
        OsString::from("-O"),
        OsString::from("qcow2"),
        from.to_path_buf().into_os_string(),
        to.to_path_buf().into_os_string(),
    ]
}

pub fn resize_args(disk: &Path, size: ByteSize) -> Vec<OsString> {
    vec![
        OsString::from("resize"),
        disk.to_path_buf().into_os_string(),
        OsString::from(size.bytes().to_string()),
    ]
}

pub fn check_args(disk: &Path) -> Vec<OsString> {
    vec![OsString::from("check"), disk.to_path_buf().into_os_string()]
}

/// Run `qemu-img`, failing with whatever it said rather than a bare status.
pub fn run(qemu_img: &Path, args: &[OsString]) -> Result<String> {
    let output = Command::new(qemu_img)
        .args(args)
        .output()
        .with_context(|| format!("cannot run {}", qemu_img.display()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        bail!(
            "qemu-img {} failed: {}",
            args.first()
                .map(|a| a.to_string_lossy().into_owned())
                .unwrap_or_default(),
            if detail.is_empty() {
                "no output"
            } else {
                detail
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn create_overlay(
    qemu_img: &Path,
    golden: &Path,
    overlay: &Path,
    size: Option<ByteSize>,
) -> Result<()> {
    if !golden.exists() {
        bail!(
            "golden image {} does not exist; build it or point `golden` elsewhere",
            golden.display()
        );
    }
    run(qemu_img, &create_overlay_args(golden, overlay, size))?;
    Ok(())
}

pub fn convert(qemu_img: &Path, from: &Path, to: &Path) -> Result<()> {
    run(qemu_img, &convert_args(from, to))?;
    Ok(())
}

pub fn check(qemu_img: &Path, disk: &Path) -> Result<()> {
    run(qemu_img, &check_args(disk))?;
    Ok(())
}

/// Where a promoted image goes: dated, so promoting never overwrites the
/// image the running VMs are backed by.
pub fn dated_golden_path(golden_dir: &Path, image_key: &str, stamp: &str) -> PathBuf {
    golden_dir.join(format!("{image_key}-{stamp}.qcow2"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(args: &[OsString]) -> String {
        args.iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn an_overlay_names_its_backing_format_so_qemu_does_not_probe_it() {
        let args = create_overlay_args(
            Path::new("/data/golden/linux.qcow2"),
            Path::new("/state/vms/a/overlay.qcow2"),
            None,
        );

        assert_eq!(
            line(&args),
            "create -f qcow2 -F qcow2 -b /data/golden/linux.qcow2 /state/vms/a/overlay.qcow2"
        );
    }

    #[test]
    fn an_overlay_can_be_larger_than_its_backing_image() {
        let args = create_overlay_args(
            Path::new("/data/golden/linux.qcow2"),
            Path::new("/state/vms/a/overlay.qcow2"),
            Some(ByteSize::from_gib(64)),
        );

        assert!(line(&args).ends_with("68719476736"), "{}", line(&args));
    }

    #[test]
    fn promotion_converts_rather_than_committing() {
        // `commit` would write the overlay back into the golden image.
        let args = convert_args(
            Path::new("/state/vms/a/overlay.qcow2"),
            Path::new("/data/golden/linux-new.qcow2"),
        );

        assert!(line(&args).starts_with("convert -O qcow2 "));
        assert!(!line(&args).contains("commit"));
    }

    #[test]
    fn a_blank_disk_is_created_with_an_explicit_size() {
        let args = create_blank_args(Path::new("/build/disk.qcow2"), ByteSize::from_gib(128));

        assert_eq!(
            line(&args),
            "create -f qcow2 /build/disk.qcow2 137438953472"
        );
    }

    #[test]
    fn a_missing_golden_image_is_reported_before_qemu_img_is_run() {
        let temp = tempfile::tempdir().unwrap();

        let err = create_overlay(
            Path::new("/nonexistent/qemu-img"),
            &temp.path().join("absent.qcow2"),
            &temp.path().join("overlay.qcow2"),
            None,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("does not exist"), "{err}");
        assert!(err.contains("golden"), "{err}");
    }

    #[test]
    fn a_promoted_image_is_dated_so_it_cannot_clobber_the_running_one() {
        let path = dated_golden_path(Path::new("/data/golden"), "linux", "20260920-2100");

        assert_eq!(path, Path::new("/data/golden/linux-20260920-2100.qcow2"));
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_qemu_img_reports_what_it_printed() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let fake = temp.path().join("qemu-img");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho \"qemu-img: Could not open 'x': No such file\" >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

        let err = run(&fake, &check_args(Path::new("x")))
            .unwrap_err()
            .to_string();

        assert!(err.contains("Could not open"), "{err}");
        assert!(err.contains("check"), "{err}");
    }
}
