//! `vitro screenshot`, `vitro key`, the mouse commands (`click`, `scroll`,
//! `hover`, `drag`) and `vitro launch` — checking that a window actually
//! opens, and using it.
//!
//! Between them these answer the one question a headless VM cannot otherwise
//! be asked: did the program come up, and what does it look like? The
//! picture, the keys and the mouse go through the VM's monitor rather than
//! through the guest — a QEMU guest's QMP socket, a lume guest's VNC server —
//! so the picture works even when the guest is wedged at a firmware prompt
//! with no network, and the keys and the mouse arrive as a keyboard's and a
//! mouse's would.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::keys::Chord;
use crate::pointer::{Button, Wheel};
use crate::ssh::powershell_literal;
use crate::{
    commands::exec, golden, lume, process, qmp, signals, ssh, tools, vnc, Backend, Config, Guest,
    Paths, Store, SysinfoProbe, VmRecord,
};

pub fn screenshot(
    paths: &Paths,
    config: &Config,
    name: &str,
    out: Option<&Path>,
) -> Result<PathBuf> {
    let record = running(paths, name)?;

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
    let absolute = std::path::absolute(&out).unwrap_or(out);

    match record.backend {
        // QEMU writes the file itself, so the path has to be one it can reach —
        // which is any absolute path on this host.
        Backend::Qemu => qmp::screendump(monitor(&record)?, &absolute)?,
        Backend::Lume => {
            let screen = vnc::Session::connect(&vnc_target(config, &record)?)?.capture()?;
            write_png(&absolute, &screen)?;
        }
    }

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

/// Press chords on the guest's keyboard, one after another, `delay` apart.
///
/// Every chord is read before the first is pressed, so a misspelt name never
/// leaves the guest with half of what was meant.
pub fn key(
    paths: &Paths,
    config: &Config,
    name: &str,
    chords: &[String],
    delay: Duration,
) -> Result<()> {
    let chords = chords
        .iter()
        .map(|chord| Chord::parse(chord))
        .collect::<Result<Vec<_>>>()?;
    let record = running(paths, name)?;

    match record.backend {
        Backend::Qemu => {
            let socket = monitor(&record)?;
            for (i, chord) in chords.iter().enumerate() {
                pace(i, delay)?;
                qmp::send_key(socket, chord.names())?;
            }
        }
        Backend::Lume => {
            let mut session = vnc::Session::connect(&vnc_target(config, &record)?)?;
            for (i, chord) in chords.iter().enumerate() {
                pace(i, delay)?;
                session.press(&chord.keysyms())?;
            }
        }
    }
    Ok(())
}

/// Wait `delay` before every chord but the first, and stop at a Ctrl-C rather
/// than type the rest of what was asked into a guest nobody wants it in.
///
/// The wait is taken in slices, because a signal only sets a flag: one long
/// sleep would make a Ctrl-C wait out whatever `--delay` was given.
fn pace(index: usize, delay: Duration) -> Result<()> {
    const SLICE: Duration = Duration::from_millis(50);

    if index > 0 {
        let until = std::time::Instant::now() + delay;
        loop {
            signals::check()?;
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(SLICE));
        }
    }
    signals::check()
}

/// Click `button` at `(x, y)`, `count` times in a row, in the pixels of the
/// guest's screen as `vitro screenshot` shows it, with the keys of `with`
/// held down.
pub fn click(
    paths: &Paths,
    config: &Config,
    name: &str,
    at: (u32, u32),
    button: Button,
    count: u32,
    with: Option<&str>,
) -> Result<()> {
    on_screen(
        paths,
        config,
        name,
        &[at],
        with,
        |socket, at| qmp::click(socket, at[0].0, at[0].1, button, count),
        |session, at| session.click(at[0].0, at[0].1, button, count),
    )
}

/// Turn the wheel `count` notches with the pointer at `(x, y)` and the keys of
/// `with` held down — Shift for a sideways scroll in most toolkits.
pub fn scroll(
    paths: &Paths,
    config: &Config,
    name: &str,
    at: (u32, u32),
    wheel: Wheel,
    count: u32,
    with: Option<&str>,
) -> Result<()> {
    on_screen(
        paths,
        config,
        name,
        &[at],
        with,
        |socket, at| qmp::scroll(socket, at[0].0, at[0].1, wheel, count),
        |session, at| session.scroll(at[0].0, at[0].1, wheel, count),
    )
}

/// Put the pointer at `(x, y)` and leave it there, for what only shows while
/// it hovers.
pub fn hover(paths: &Paths, config: &Config, name: &str, at: (u32, u32)) -> Result<()> {
    on_screen(
        paths,
        config,
        name,
        &[at],
        None,
        |socket, at| qmp::move_to(socket, at[0].0, at[0].1),
        |session, at| session.move_to(at[0].0, at[0].1),
    )
}

/// Hold `button` down at `from`, move to `to` and let go there, with the keys
/// of `with` held down throughout.
pub fn drag(
    paths: &Paths,
    config: &Config,
    name: &str,
    from: (u32, u32),
    to: (u32, u32),
    button: Button,
    with: Option<&str>,
) -> Result<()> {
    on_screen(
        paths,
        config,
        name,
        &[from, to],
        with,
        |socket, at| qmp::drag(socket, at[0], at[1], button),
        |session, at| session.drag(at[0], at[1], button),
    )
}

/// Check that every point is on the guest's screen, then hand them to the
/// way into this guest in the coordinates it takes — the tablet's for QEMU,
/// pixels for VNC — with the keys of `with` held down meanwhile.
///
/// `with` is read before anything else, so a misspelt key never leaves the
/// pointer moved and the action half done.
fn on_screen(
    paths: &Paths,
    config: &Config,
    name: &str,
    points: &[(u32, u32)],
    with: Option<&str>,
    qemu: impl FnOnce(&Path, &[(u32, u32)]) -> Result<()>,
    lume: impl FnOnce(&mut vnc::Session, &[(u16, u16)]) -> Result<()>,
) -> Result<()> {
    let held = with.map(Chord::parse).transpose()?;
    let record = running(paths, name)?;
    match record.backend {
        Backend::Qemu => {
            let socket = monitor(&record)?;
            // The tablet spans the screen, so the screen's size is needed to
            // place a pixel on it; a capture is the one place that says.
            let (width, height) = {
                let dir = tempfile::tempdir()?;
                let shot = dir.path().join("size.png");
                qmp::screendump(socket, &shot)?;
                png_size(&shot)?
            };
            for &(x, y) in points {
                check_inside(x, y, width, height)?;
            }
            let points: Vec<_> = points
                .iter()
                .map(|&(x, y)| (to_tablet(x, width), to_tablet(y, height)))
                .collect();
            let keys = held.as_ref().map_or(&[][..], Chord::names);
            qmp::with_keys_held(socket, keys, || qemu(socket, &points))
        }
        Backend::Lume => {
            let mut session = vnc::Session::connect(&vnc_target(config, &record)?)?;
            let (width, height) = session.size();
            for &(x, y) in points {
                check_inside(x, y, u32::from(width), u32::from(height))?;
            }
            let points: Vec<_> = points.iter().map(|&(x, y)| (x as u16, y as u16)).collect();
            let keysyms = held.as_ref().map(Chord::keysyms).unwrap_or_default();
            session.with_keys_held(&keysyms, |session| lume(session, &points))
        }
    }
}

fn check_inside(x: u32, y: u32, width: u32, height: u32) -> Result<()> {
    if x >= width || y >= height {
        bail!("({x}, {y}) is off the screen, which is {width}x{height}");
    }
    Ok(())
}

/// A pixel's place on the tablet's axis, which spans `0..=ABS_MAX` end to end.
fn to_tablet(pixel: u32, size: u32) -> u32 {
    if size <= 1 {
        return 0;
    }
    (u64::from(pixel) * u64::from(qmp::ABS_MAX) / u64::from(size - 1)) as u32
}

fn png_size(path: &Path) -> Result<(u32, u32)> {
    let file =
        std::fs::File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let reader = png::Decoder::new(std::io::BufReader::new(file)).read_info()?;
    let info = reader.info();
    Ok((info.width, info.height))
}

fn running(paths: &Paths, name: &str) -> Result<VmRecord> {
    let store = Store::new(paths.state_dir());
    let name = store.resolve(name)?;
    let record = store.load(&name)?;
    if !process::is_running(&SysinfoProbe::new(), record.pid, record.pid_start_time) {
        bail!("VM {name:?} is not running; `vitro run` starts a new one");
    }
    Ok(record)
}

fn monitor(record: &VmRecord) -> Result<&Path> {
    match record.qmp_socket.as_deref() {
        Some(socket) => Ok(socket),
        None => bail!(
            "VM {:?} was started without a monitor socket, so its screen and keyboard cannot be reached",
            record.name
        ),
    }
}

/// Where a lume guest's VNC server is now. Asked of lume each time, because
/// the port and the password change whenever the guest starts.
fn vnc_target(config: &Config, record: &VmRecord) -> Result<vnc::Target> {
    let binary = tools::resolve("lume", config.tools().lume.as_deref(), "tools.lume")?.path;
    let info = lume::get(&binary, &record.name)?;
    match info.vnc_url {
        Some(url) => vnc::parse_url(&url),
        None => bail!("lume reports no VNC server for {:?}", record.name),
    }
}

fn write_png(path: &Path, screen: &vnc::Screen) -> Result<()> {
    use std::io::Write;

    let file =
        std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let mut out = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(&mut out, screen.width, screen.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header()?;
    writer.write_image_data(&screen.rgb)?;
    // Both the encoder and the buffer write their last bytes on drop and
    // discard any error there, which would report a truncated file as saved.
    writer.finish()?;
    out.flush()
        .with_context(|| format!("cannot write {}", path.display()))
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
    fn the_tablet_spans_the_screen_from_edge_to_edge() {
        assert_eq!(to_tablet(0, 1920), 0);
        assert_eq!(to_tablet(1919, 1920), qmp::ABS_MAX);
        assert_eq!(to_tablet(960, 1921), qmp::ABS_MAX / 2);
    }

    #[test]
    fn a_click_off_the_screen_is_refused() {
        assert!(check_inside(1920, 0, 1920, 1080).is_err());
        assert!(check_inside(1919, 1079, 1920, 1080).is_ok());
    }

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
