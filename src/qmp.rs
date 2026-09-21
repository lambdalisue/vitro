//! A minimal QMP client: enough to stop a guest and to look at its screen.
//!
//! Two things about QMP are easy to get wrong and expensive to debug. There is
//! no `sendkey` command — that name belongs to the human monitor, and QMP
//! spells it `input-send-event` with an explicit press and release per key. And
//! a press and release batched into one event arrives as a single HID report,
//! which a guest UI ignores: a click has to be three separate calls.
//!
//! Unix sockets only. Windows hosts would need the named-pipe transport, which
//! vitro does not implement; there a guest is stopped over SSH instead.

use std::path::Path;

use anyhow::{bail, Result};

// Everything below is the wire protocol itself, which only the Unix-socket
// transport speaks. On Windows it would all be dead code, and dead code that
// looks like an oversight is worse than none.
#[cfg(unix)]
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::time::Duration;

#[cfg(unix)]
use anyhow::Context;
#[cfg(unix)]
use serde::Deserialize;

#[cfg(unix)]
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(unix)]
#[derive(Debug, Deserialize)]
struct Reply {
    #[serde(default)]
    error: Option<QmpError>,
}

#[cfg(unix)]
#[derive(Debug, Deserialize)]
struct QmpError {
    class: String,
    desc: String,
}

/// Send one command and wait for its reply.
///
/// Events can arrive interleaved with replies, so anything without a `return`
/// or `error` member is skipped rather than treated as the answer.
pub fn execute(socket: &Path, command: &str, arguments: Option<serde_json::Value>) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let stream = UnixStream::connect(socket)
            .with_context(|| format!("cannot connect to {}", socket.display()))?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT))?;
        stream.set_write_timeout(Some(REPLY_TIMEOUT))?;

        let mut reader = BufReader::new(stream.try_clone()?);
        let mut writer = stream;

        // The greeting comes first, and nothing is accepted until capabilities
        // are negotiated.
        let mut greeting = String::new();
        reader.read_line(&mut greeting)?;
        writeln!(writer, r#"{{"execute":"qmp_capabilities"}}"#)?;
        writer.flush()?;
        read_reply(&mut reader, "qmp_capabilities")?;

        let request = match arguments {
            Some(args) => {
                serde_json::json!({ "execute": command, "arguments": args })
            }
            None => serde_json::json!({ "execute": command }),
        };
        writeln!(writer, "{request}")?;
        writer.flush()?;
        read_reply(&mut reader, command)
    }
    #[cfg(not(unix))]
    {
        let _ = (socket, command, arguments);
        bail!("QMP over a named pipe is not implemented; stop the guest over SSH")
    }
}

#[cfg(unix)]
fn read_reply(reader: &mut impl BufRead, command: &str) -> Result<()> {
    for _ in 0..64 {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            bail!("the QMP connection closed while waiting for {command}");
        }
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        // Only a reply carries `return` or `error`. Treating anything else
        // as success would report a command that never ran as having worked.
        if value.get("return").is_none() && value.get("error").is_none() {
            continue;
        }
        let reply: Reply = serde_json::from_value(value)?;
        return match reply.error {
            Some(e) => bail!("QMP {command} failed: {} ({})", e.desc, e.class),
            None => Ok(()),
        };
    }
    bail!("no QMP reply to {command}")
}

/// Press the virtual power button.
///
/// A Linux guest treats this as an ACPI shutdown request and is gone in
/// seconds. Windows on the `virt` machine ignores it: the button arrives as an
/// ACPI GPIO key and Windows on ARM has no driver that answers one. Callers
/// that may be talking to Windows must not rely on this.
pub fn power_down(socket: &Path) -> Result<()> {
    execute(socket, "system_powerdown", None)
}

/// Write the guest's framebuffer to `out` as a PNG.
pub fn screendump(socket: &Path, out: &Path) -> Result<()> {
    execute(
        socket,
        "screendump",
        Some(serde_json::json!({
            "filename": out.to_string_lossy(),
            "format": "png",
        })),
    )
}

/// Press and release one key, with optional modifiers held around it.
pub fn send_key(socket: &Path, keys: &[&str]) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let mut events = Vec::new();
    for key in keys {
        events.push(key_event(key, true));
    }
    // Modifiers are released after the key they modify.
    for key in keys.iter().rev() {
        events.push(key_event(key, false));
    }
    execute(
        socket,
        "input-send-event",
        Some(serde_json::json!({ "events": events })),
    )
}

fn key_event(key: &str, down: bool) -> serde_json::Value {
    serde_json::json!({
        "type": "key",
        "data": { "down": down, "key": { "type": "qcode", "data": key } }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_press_releases_modifiers_after_the_key() {
        // Releasing in order would let go of shift while the key is still
        // down, which the guest sees as a different character.
        let mut events = Vec::new();
        for key in ["shift", "semicolon"] {
            events.push(key_event(key, true));
        }
        for key in ["shift", "semicolon"].iter().rev() {
            events.push(key_event(key, false));
        }

        let order: Vec<(String, bool)> = events
            .iter()
            .map(|e| {
                (
                    e["data"]["key"]["data"].as_str().unwrap().to_string(),
                    e["data"]["down"].as_bool().unwrap(),
                )
            })
            .collect();

        assert_eq!(
            order,
            [
                ("shift".to_string(), true),
                ("semicolon".to_string(), true),
                ("semicolon".to_string(), false),
                ("shift".to_string(), false),
            ]
        );
    }

    #[test]
    fn a_key_event_uses_qcode_names() {
        let event = key_event("ret", true);

        assert_eq!(event["type"], "key");
        assert_eq!(event["data"]["key"]["type"], "qcode");
        assert_eq!(event["data"]["key"]["data"], "ret");
    }

    #[cfg(unix)]
    #[test]
    fn a_qmp_error_reply_is_reported_with_its_description() {
        let reply: Reply = serde_json::from_str(
            r#"{"error":{"class":"CommandNotFound","desc":"no such command"}}"#,
        )
        .unwrap();
        let e = reply.error.unwrap();

        assert_eq!(e.class, "CommandNotFound");
        assert_eq!(e.desc, "no such command");
    }

    #[cfg(unix)]
    #[test]
    fn events_are_skipped_while_waiting_for_a_reply() {
        let stream = concat!(
            r#"{"timestamp":{"seconds":1},"event":"POWERDOWN"}"#,
            "\n",
            r#"{"return":{}}"#,
            "\n"
        );

        read_reply(&mut stream.as_bytes(), "system_powerdown").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_closed_connection_is_not_mistaken_for_success() {
        let err = read_reply(&mut "".as_bytes(), "system_powerdown")
            .unwrap_err()
            .to_string();

        assert!(err.contains("closed"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn an_error_reply_fails_the_call() {
        let stream = concat!(
            r#"{"error":{"class":"CommandNotFound","desc":"The command sendkey has not been found"}}"#,
            "\n"
        );

        let err = read_reply(&mut stream.as_bytes(), "sendkey")
            .unwrap_err()
            .to_string();

        assert!(err.contains("has not been found"), "{err}");
    }
}
