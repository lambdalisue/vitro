//! Turning a guest's disk into a golden image.
//!
//! `build` and `promote` differ only in where the disk came from — installation
//! media or a VM someone has been working in — so the part that makes an image
//! out of it lives here and is shared.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use time::OffsetDateTime;

use crate::{image, process, qmp, ssh, Guest, ProcessProbe, SysinfoProbe};

/// A Linux guest answers the power button in seconds. Waiting much longer than
/// that only delays the fallback for a guest that never will.
const POWER_BUTTON_TIMEOUT: Duration = Duration::from_secs(60);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(180);

/// A timestamp that sorts and is safe in a file name.
pub fn stamp(now: OffsetDateTime) -> String {
    format!(
        "{:04}{:02}{:02}-{:02}{:02}{:02}",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second()
    )
}

/// Shut the guest down and wait for QEMU to exit.
///
/// A Unix guest gets the monitor's power button first: it reads that as an ACPI
/// shutdown request and is gone in seconds. Windows on the `virt` machine has
/// no driver for the GPIO key that carries the button and would simply ignore
/// it, so it is asked over SSH straight away rather than after a minute of
/// waiting for a reply that is never coming.
pub fn shut_down(
    ssh_binary: &Path,
    target: &ssh::SshTarget,
    guest: Guest,
    pid: u32,
    qmp_socket: Option<&Path>,
) -> Result<()> {
    let gone =
        |timeout| process::wait_for_exit(timeout, || SysinfoProbe::new().start_time(pid).is_some());

    if guest == Guest::Unix {
        if let Some(socket) = qmp_socket {
            if qmp::power_down(socket).is_ok() && gone(POWER_BUTTON_TIMEOUT) {
                return Ok(());
            }
        }
    }

    let _ = ssh::run_passthrough(
        ssh_binary,
        target,
        &ssh::SshOptions {
            command: shutdown_command(guest),
            quoting: guest.into(),
            ..ssh::SshOptions::default()
        },
    );

    if !gone(SHUTDOWN_TIMEOUT) {
        // Flattening a disk that was cut off mid-write produces a golden image
        // that boots into a filesystem check, or does not boot at all.
        bail!("the guest did not shut down cleanly, so the image was not kept");
    }
    Ok(())
}

fn shutdown_command(guest: Guest) -> Vec<String> {
    match guest {
        // Backgrounded and detached: the guest tears down its own SSH session
        // on the way out, so waiting for the command to finish would mean
        // waiting for a connection that is being closed.
        Guest::Unix => vec![
            "sh".into(),
            "-c".into(),
            "sudo systemctl poweroff --no-block >/dev/null 2>&1 \
             || sudo shutdown -h now >/dev/null 2>&1 &"
                .into(),
        ],
        // `/t 0` rather than the default delay, and `/f` because a pending
        // update dialog would otherwise hold the machine up indefinitely.
        Guest::Windows => vec![
            "shutdown".into(),
            "/s".into(),
            "/f".into(),
            "/t".into(),
            "0".into(),
        ],
    }
}

/// Leave the image in a state where booting it is fast.
///
/// cloud-init has done its one job by the time this runs — the user and the key
/// are in the image — and a VM started from the result gets no seed to find.
/// Left enabled it spends two minutes on every later boot hunting for a
/// datasource that will never be there, which is longer than the rest of the
/// boot put together.
///
/// Best effort: an image without cloud-init is perfectly valid, and failing
/// over a tidying step would be worse than a slow boot.
pub fn quiesce_cloud_init(ssh_binary: &Path, target: &ssh::SshTarget) {
    // `clean` last would undo the flag: it erases cloud-init's own state, and
    // the disable file counts as state.
    let script = "command -v cloud-init >/dev/null 2>&1 \
                    && sudo cloud-init clean --logs >/dev/null 2>&1; \
                  sudo touch /etc/cloud/cloud-init.disabled";
    let result = ssh::run_passthrough(
        ssh_binary,
        target,
        &ssh::SshOptions {
            command: vec!["sh".into(), "-c".into(), script.into()],
            ..ssh::SshOptions::default()
        },
    );
    if let Err(e) = result {
        eprintln!("vitro: could not disable cloud-init in the image ({e:#}); boots will be slower");
    }
}

/// Flatten a disk into a standalone image at `destination`.
///
/// `qemu-img convert` rather than `commit`: the disk being flattened is usually
/// an overlay, and committing would write the changes back into the golden
/// image it was based on — destroying the very thing other VMs are running on.
///
/// The conversion lands beside the destination and is renamed into place, so a
/// failure part-way cannot leave something that looks like a finished image
/// where `run` looks for one. Beside, rather than in a working directory:
/// `--as` can name any path, and a rename across filesystems fails outright —
/// after the guest has already been stopped.
pub fn install(qemu_img: &Path, disk: &Path, destination: &Path) -> Result<()> {
    let parent = destination.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).with_context(|| format!("cannot create {}", parent.display()))?;

    let staging = staging_path(destination);
    let result = (|| {
        image::convert(qemu_img, disk, &staging)?;
        image::check(qemu_img, &staging)?;
        fs::rename(&staging, destination)
            .with_context(|| format!("cannot move the image into {}", destination.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result
}

/// A sibling of the destination that no concurrent run can collide with.
fn staging_path(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "image".into());
    destination.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Where a promoted or built image goes when the caller did not say.
pub fn dated_path(golden_dir: &Path, image_key: &str, now: OffsetDateTime) -> PathBuf {
    image::dated_golden_path(golden_dir, image_key, &stamp(now))
}

/// The most recent image `build` or `promote` made for this key.
///
/// What an image with no `golden` setting runs from. Built images are dated so
/// that rebuilding cannot pull the disk out from under a VM already running on
/// the old one — which would otherwise make every build end in editing the
/// configuration by hand.
pub fn newest(golden_dir: &Path, image_key: &str) -> Option<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(golden_dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| is_dated_image(path, image_key))
        .collect();
    // The stamps are fixed-width, so sorting the names sorts them by date.
    found.sort();
    found.pop()
}

/// Exactly `<key>-<stamp>.qcow2`.
///
/// An image key may contain a hyphen, so matching on the prefix alone would let
/// `windows` claim `windows-devtools-20260920-165554.qcow2` — and `run windows`
/// would then quietly start somebody else's image.
fn is_dated_image(path: &Path, image_key: &str) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(stamp) = name
        .strip_prefix(image_key)
        .and_then(|rest| rest.strip_prefix('-'))
        .and_then(|rest| rest.strip_suffix(".qcow2"))
    else {
        return false;
    };
    let digits =
        |part: &str, width: usize| part.len() == width && part.bytes().all(|b| b.is_ascii_digit());
    matches!(stamp.split_once('-'), Some((day, time)) if digits(day, 8) && digits(time, 6))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn a_windows_guest_is_asked_in_the_only_language_it_answers() {
        assert_eq!(
            shutdown_command(Guest::Windows),
            ["shutdown", "/s", "/f", "/t", "0"]
        );
    }

    #[test]
    fn a_unix_guest_is_asked_in_a_way_that_survives_losing_the_connection() {
        let command = shutdown_command(Guest::Unix).join(" ");

        assert!(command.contains("poweroff"), "{command}");
        assert!(command.ends_with('&'), "detached: {command}");
    }

    #[test]
    fn a_stamp_sorts_chronologically_and_needs_no_quoting() {
        let stamp = stamp(datetime!(2026-09-20 21:16:33 UTC));

        assert_eq!(stamp, "20260920-211633");
        assert!(stamp < self::stamp(datetime!(2026-09-20 21:16:34 UTC)));
        assert!(stamp
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-'));
    }

    #[test]
    fn staging_happens_beside_the_destination_so_the_rename_stays_on_one_filesystem() {
        let staging = staging_path(Path::new("/srv/images/base.qcow2"));

        assert_eq!(staging.parent(), Some(Path::new("/srv/images")));
        assert_ne!(staging.file_name(), Some("base.qcow2".as_ref()));
    }

    #[test]
    fn the_default_destination_names_the_image_and_the_moment() {
        let path = dated_path(
            Path::new("/data/golden"),
            "linux",
            datetime!(2026-09-20 21:16:33 UTC),
        );

        assert_eq!(path, Path::new("/data/golden/linux-20260920-211633.qcow2"));
    }

    #[test]
    fn the_newest_build_is_the_one_an_unset_golden_runs() {
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "windows-20260920-165554.qcow2",
            "windows-20260921-090000.qcow2",
            "windows-20260919-235959.qcow2",
        ] {
            fs::write(temp.path().join(name), b"").unwrap();
        }

        assert_eq!(
            newest(temp.path(), "windows"),
            Some(temp.path().join("windows-20260921-090000.qcow2"))
        );
    }

    #[test]
    fn one_image_never_claims_another_whose_key_starts_the_same_way() {
        // `run windows` starting a devtools image nobody asked for is the kind
        // of wrong that looks like it worked.
        let temp = tempfile::tempdir().unwrap();
        for name in [
            "windows-devtools-20260921-090000.qcow2",
            "windows-20260920-165554.qcow2",
        ] {
            fs::write(temp.path().join(name), b"").unwrap();
        }

        assert_eq!(
            newest(temp.path(), "windows"),
            Some(temp.path().join("windows-20260920-165554.qcow2"))
        );
        assert_eq!(
            newest(temp.path(), "windows-devtools"),
            Some(temp.path().join("windows-devtools-20260921-090000.qcow2"))
        );
    }

    #[test]
    fn an_image_somebody_named_themselves_is_not_mistaken_for_a_build() {
        // `promote --as base` writes a name with no stamp. Picking it up here
        // would make "the newest build" mean something the user never built.
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("linux-base.qcow2"), b"").unwrap();
        fs::write(temp.path().join("linux-20260920.qcow2"), b"").unwrap();

        assert_eq!(newest(temp.path(), "linux"), None);
    }

    #[test]
    fn a_golden_directory_that_does_not_exist_yet_is_simply_empty() {
        let temp = tempfile::tempdir().unwrap();

        assert_eq!(newest(&temp.path().join("never-made"), "linux"), None);
    }
}
