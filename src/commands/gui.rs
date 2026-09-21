//! `vitro screenshot` and `vitro launch` — checking that a window actually opens.
//!
//! Between them these answer the one question a headless VM cannot otherwise
//! be asked: did the program come up, and what does it look like? The picture
//! comes from the monitor rather than from inside the guest, so it works even
//! when the guest is wedged at a firmware prompt with no network.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{commands::exec, golden, qmp, ssh, Config, Guest, Paths, Store};

pub fn screenshot(paths: &Paths, name: &str, out: Option<&Path>) -> Result<PathBuf> {
    let store = Store::new(paths.state_dir());
    let name = store.resolve(name)?;
    let record = store.load(&name)?;

    let Some(socket) = record.qmp_socket.as_deref() else {
        bail!("VM {name:?} was started without a monitor socket, so its screen cannot be captured");
    };

    // Into the VM's own directory by default: a picture of a guest belongs with
    // the rest of that guest's debris, and is removed with it.
    let out = match out {
        Some(path) => path.to_path_buf(),
        None => record.dir.join(format!(
            "screen-{}.png",
            golden::stamp(time::OffsetDateTime::now_utc())
        )),
    };
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    // QEMU writes the file itself, so the path has to be one it can reach —
    // which is any absolute path on this host.
    let absolute = std::path::absolute(&out).unwrap_or(out);
    qmp::screendump(socket, &absolute)?;

    // A guest that has switched its display off still has a framebuffer, so
    // the capture succeeds and yields a rectangle of one colour. That looks
    // exactly like a GUI that failed to draw, and the difference matters
    // enough to say out loud rather than leave to guesswork.
    if looks_blank(&absolute) {
        eprintln!(
            "vitro: {} is a single colour — the guest may have blanked its display.",
            absolute.display()
        );
        eprintln!("       Send it some input and take another, or disable its display timeout.");
    }

    Ok(absolute)
}

/// Whether a PNG is entirely one colour.
///
/// Deliberately crude: it reads the file QEMU just wrote and gives up quietly
/// on anything it does not understand, because this only ever produces a hint
/// and a wrong hint is worse than none.
fn looks_blank(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    let decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    let Ok(mut reader) = decoder.read_info() else {
        return false;
    };
    let Some(size) = reader.output_buffer_size() else {
        return false;
    };
    let mut buffer = vec![0; size];
    let Ok(info) = reader.next_frame(&mut buffer) else {
        return false;
    };
    let pixels = &buffer[..info.buffer_size()];
    let stride = info.color_type.samples() * usize::from(info.bit_depth as u8) / 8;
    if stride == 0 || pixels.len() < stride {
        return false;
    }
    let first = &pixels[..stride];
    pixels.chunks_exact(stride).all(|pixel| pixel == first)
}

/// Start a program where somebody can see it.
///
/// Not `exec`: a process started by sshd on Windows lands in session 0, which
/// has no desktop, and one started without `DISPLAY` on Linux has no screen to
/// draw on. Nothing is collected from the program — it is left running, which
/// is the point.
pub fn launch(paths: &Paths, config: &Config, name: &str, command: &[String]) -> Result<()> {
    if command.is_empty() {
        bail!("say what to launch");
    }
    let (record, target, ssh_binary) = exec::prepare(paths, config, name)?;

    let code = ssh::run_passthrough(
        &ssh_binary,
        &target,
        &ssh::SshOptions {
            command: launch_command(record.guest, &record.ssh_user, command),
            quoting: record.guest.into(),
            ..ssh::SshOptions::default()
        },
    )?;
    if code != 0 {
        bail!("the guest refused to start it (exit {code})");
    }
    Ok(())
}

fn launch_command(guest: Guest, user: &str, command: &[String]) -> Vec<String> {
    match guest {
        // `:0` is the desktop the golden image is expected to have running.
        // Detached, because the point is to leave it on screen.
        Guest::Unix => vec![
            "sh".into(),
            "-c".into(),
            format!("DISPLAY=:0 nohup {} >/dev/null 2>&1 &", shell_join(command)),
        ],
        // A scheduled task is the way into the interactive session: `/it` means
        // "only when the user is logged on", which puts it on the console
        // desktop where the monitor can see it.
        //
        // Built as a PowerShell call rather than a cmd line, because `cmd` has
        // no escape for a quote inside a quoted argument: a path containing one
        // would break out of `/tr` and run whatever followed as its own
        // command. PowerShell's doubled quote is an escape, and the guest's
        // shell is PowerShell.
        Guest::Windows => {
            let task = "vitro-launch";
            let program = powershell_literal(&command.join(" "));
            vec![format!(
                "schtasks /create /tn {task} /tr {program} /sc once /st 00:00 \
                     /ru {} /it /rl highest /f; schtasks /run /tn {task}",
                powershell_literal(user)
            )]
        }
    }
}

/// A single-quoted PowerShell string. Doubling is how a quote is escaped.
fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Quote for the guest's shell, the same way vitro quotes for `exec`.
fn shell_join(command: &[String]) -> String {
    command
        .iter()
        .map(|part| {
            if part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_./:=@,+".contains(&b))
                && !part.is_empty()
            {
                part.clone()
            } else {
                format!("'{}'", part.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linux_program_is_given_a_display_and_left_running() {
        let command = launch_command(Guest::Unix, "dev", &["xterm".into()]).join(" ");

        assert!(command.contains("DISPLAY=:0"), "{command}");
        assert!(command.contains("nohup"), "{command}");
        assert!(command.ends_with('&'), "{command}");
    }

    #[test]
    fn a_linux_program_keeps_its_arguments_intact() {
        let command = launch_command(
            Guest::Unix,
            "dev",
            &["xmessage".into(), "hello there".into()],
        )
        .join(" ");

        assert!(command.contains("'hello there'"), "{command}");
    }

    #[test]
    fn a_windows_program_goes_through_the_interactive_task_flag() {
        // Without /it the process lands in session 0, which has no desktop, and
        // the screenshot shows nothing at all.
        let command = launch_command(Guest::Windows, "vitro", &["notepad.exe".into()]).join(" ");

        assert!(command.contains("schtasks /create"), "{command}");
        assert!(command.contains(" /it "), "{command}");
        assert!(command.contains("/ru 'vitro'"), "{command}");
        assert!(command.contains("schtasks /run"), "{command}");
    }

    #[test]
    fn a_windows_command_with_a_quote_in_it_cannot_escape_the_task_argument() {
        // cmd has no escape for a quote inside a quoted argument, so a path
        // with one in it would otherwise end /tr early and run the rest.
        let command = launch_command(
            Guest::Windows,
            "vitro",
            &["notepad.exe".into(), "C:\\a'b.txt".into()],
        )
        .join(" ");

        assert!(command.contains("'notepad.exe C:\\a''b.txt'"), "{command}");
    }

    #[test]
    fn a_windows_user_name_is_quoted_too() {
        let command = launch_command(Guest::Windows, "o'rvo", &["notepad.exe".into()]).join(" ");

        assert!(command.contains("/ru 'o''rvo'"), "{command}");
    }

    /// Writes an RGB PNG whose pixels come from `fill`, so a test can build
    /// the two cases that matter without carrying image fixtures around.
    fn write_png(path: &Path, width: u32, height: u32, fill: impl Fn(u32, u32) -> [u8; 3]) {
        let file = std::fs::File::create(path).expect("fixture is writable");
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("header");
        let mut data = Vec::with_capacity((width * height * 3) as usize);
        for y in 0..height {
            for x in 0..width {
                data.extend_from_slice(&fill(x, y));
            }
        }
        writer.write_image_data(&data).expect("image data");
    }

    #[test]
    fn a_screen_of_one_colour_is_reported_as_blank() {
        let dir = tempfile::tempdir().unwrap();

        let blank = dir.path().join("blank.png");
        write_png(&blank, 64, 48, |_, _| [0, 0, 0]);
        assert!(looks_blank(&blank), "an all-black capture is blank");

        // Not just black: a display that blanked to white is the same problem.
        let white = dir.path().join("white.png");
        write_png(&white, 64, 48, |_, _| [255, 255, 255]);
        assert!(looks_blank(&white), "an all-white capture is blank too");
    }

    #[test]
    fn a_screen_with_anything_drawn_on_it_is_not_blank() {
        let dir = tempfile::tempdir().unwrap();

        // One pixel of difference is enough: a window on a black desktop is
        // mostly black, and calling that blank would hide the very success
        // the screenshot was taken to confirm.
        let drawn = dir.path().join("drawn.png");
        write_png(&drawn, 64, 48, |x, y| {
            if x == 10 && y == 10 {
                [255, 255, 255]
            } else {
                [0, 0, 0]
            }
        });
        assert!(!looks_blank(&drawn), "one lit pixel means something drew");
    }

    #[test]
    fn a_file_that_is_not_a_png_is_not_called_blank() {
        // The hint must never fire on something it failed to read, or it would
        // blame the guest for a problem on this side.
        let dir = tempfile::tempdir().unwrap();
        let junk = dir.path().join("junk.png");
        std::fs::write(&junk, b"not a png at all").unwrap();

        assert!(!looks_blank(&junk));
        assert!(!looks_blank(&dir.path().join("missing.png")));
    }
}
