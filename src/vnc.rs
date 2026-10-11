//! A minimal VNC client: enough to press keys and click on a lume guest, and
//! to look at its screen.
//!
//! A lume guest has no QEMU monitor; the screen and keyboard Virtualization.
//! framework offers from outside are its VNC server. Keys sent this way arrive
//! as a keyboard's would, so the guest needs no permission to receive them —
//! unlike synthesised events from inside macOS, which need Accessibility
//! access that only a click in System Settings can grant.
//!
//! Only what that needs is spoken: RFB 3.8, VNC authentication, raw encoding,
//! key events and pointer events.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};

const TIMEOUT: Duration = Duration::from_secs(10);

/// How long a key stays down. Down and up in the same instant read as no
/// press at all to some guests.
const HOLD: Duration = Duration::from_millis(20);

const SECURITY_NONE: u8 = 1;
const SECURITY_VNC_AUTH: u8 = 2;

const ENCODING_RAW: i32 = 0;
const ENCODING_DESKTOP_SIZE: i32 = -223;
const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;

/// How often the screen is asked for again when it changes size meanwhile.
const CAPTURE_ATTEMPTS: usize = 4;

/// Where a VNC server listens, and the password it wants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
}

/// Read `vnc://:password@host:port`, the shape lume reports.
pub fn parse_url(url: &str) -> Result<Target> {
    let Some(rest) = url.strip_prefix("vnc://") else {
        bail!("{url:?} is not a vnc:// URL");
    };
    let (userinfo, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hostport)) => (Some(userinfo), hostport),
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let Some((host, port)) = hostport.rsplit_once(':') else {
        bail!("{url:?} names no port");
    };
    let port = port
        .parse()
        .with_context(|| format!("{url:?} has no valid port"))?;
    let password = userinfo
        .map(|info| info.split_once(':').map_or(info, |(_, password)| password))
        .map(percent_decode)
        .filter(|password| !password.is_empty());
    Ok(Target {
        host: host.to_string(),
        port,
        password,
    })
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = bytes
            .get(i + 1..i + 3)
            .and_then(|pair| std::str::from_utf8(pair).ok())
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[i], hex) {
            (b'%', Some(byte)) => {
                out.push(byte);
                i += 3;
            }
            (byte, _) => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The answer to a VNC authentication challenge: the challenge encrypted with
/// DES, keyed by the password's first eight bytes with each byte's bits
/// reversed — a quirk of the original implementation every server expects.
pub fn auth_response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    use des::cipher::{BlockCipherEncrypt, KeyInit};

    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password.bytes()) {
        *slot = byte.reverse_bits();
    }
    let cipher = des::Des::new(&key.into());
    let mut response = *challenge;
    for block in response.chunks_exact_mut(8) {
        let block: &mut [u8; 8] = block.try_into().expect("chunks are eight bytes");
        cipher.encrypt_block(block.into());
    }
    response
}

/// A screen as RGB rows, top to bottom.
pub struct Screen {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// An open connection to a guest's screen and keyboard.
pub struct Session {
    stream: TcpStream,
    width: u16,
    height: u16,
}

impl Session {
    pub fn connect(target: &Target) -> Result<Self> {
        let address = format!("{}:{}", target.host, target.port);
        let stream = TcpStream::connect(&address)
            .with_context(|| format!("cannot connect to the VNC server at {address}"))?;
        stream.set_read_timeout(Some(TIMEOUT))?;
        stream.set_write_timeout(Some(TIMEOUT))?;
        let mut session = Self {
            stream,
            width: 0,
            height: 0,
        };
        session
            .handshake(target.password.as_deref())
            .with_context(|| format!("VNC handshake with {address} failed"))?;
        Ok(session)
    }

    fn handshake(&mut self, password: Option<&str>) -> Result<()> {
        let mut version = [0u8; 12];
        self.stream.read_exact(&mut version)?;
        if !version.starts_with(b"RFB 003.") {
            bail!(
                "the server speaks {:?}, not RFB",
                String::from_utf8_lossy(&version)
            );
        }
        self.stream.write_all(b"RFB 003.008\n")?;

        let count = self.u8()?;
        if count == 0 {
            bail!("the server refused: {}", self.reason()?);
        }
        let mut offered = vec![0u8; usize::from(count)];
        self.stream.read_exact(&mut offered)?;
        let chosen = match password {
            Some(_) if offered.contains(&SECURITY_VNC_AUTH) => SECURITY_VNC_AUTH,
            _ if offered.contains(&SECURITY_NONE) => SECURITY_NONE,
            _ => bail!("no security type vitro speaks is offered (offered {offered:?})"),
        };
        self.stream.write_all(&[chosen])?;
        if chosen == SECURITY_VNC_AUTH {
            let mut challenge = [0u8; 16];
            self.stream.read_exact(&mut challenge)?;
            let response = auth_response(password.unwrap_or_default(), &challenge);
            self.stream.write_all(&response)?;
        }
        if self.u32()? != 0 {
            bail!("authentication failed: {}", self.reason()?);
        }

        // Shared, so a viewer someone already has open stays connected.
        self.stream.write_all(&[1])?;
        self.width = self.u16()?;
        self.height = self.u16()?;
        let mut format = [0u8; 16];
        self.stream.read_exact(&mut format)?;
        let name_length = self.u32()?;
        let mut name = vec![0u8; name_length as usize];
        self.stream.read_exact(&mut name)?;

        // Ask for 32-bit little-endian pixels with red at 16, green at 8 and
        // blue at 0, whatever the server prefers: one format to decode.
        let mut message = vec![0u8, 0, 0, 0];
        message.extend_from_slice(&[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]);
        self.stream.write_all(&message)?;
        // Raw, which every server must support and the only pixels decoded,
        // and the two ways of announcing a new screen size. Offered raw alone,
        // the server in Virtualization.framework aborts — and the VM with it.
        let encodings = [
            ENCODING_RAW,
            ENCODING_DESKTOP_SIZE,
            ENCODING_EXTENDED_DESKTOP_SIZE,
        ];
        let mut message = vec![2u8, 0];
        message.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for encoding in encodings {
            message.extend_from_slice(&encoding.to_be_bytes());
        }
        self.stream.write_all(&message)?;
        Ok(())
    }

    /// Press the keys together and let go in reverse, as fingers would.
    pub fn press(&mut self, keysyms: &[u32]) -> Result<()> {
        for keysym in keysyms {
            self.stream.write_all(&key_event(*keysym, true))?;
        }
        self.stream.flush()?;
        std::thread::sleep(HOLD);
        for keysym in keysyms.iter().rev() {
            self.stream.write_all(&key_event(*keysym, false))?;
        }
        self.stream.flush()?;
        Ok(())
    }

    /// Click the left button at `(x, y)`, in the screen's pixels.
    pub fn click(&mut self, x: u16, y: u16) -> Result<()> {
        for buttons in [0, 1, 0] {
            self.stream.write_all(&pointer_event(buttons, x, y))?;
            self.stream.flush()?;
            std::thread::sleep(HOLD);
        }
        Ok(())
    }

    /// The screen's size in pixels, which is what a click is placed in.
    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// The whole screen as it is now.
    ///
    /// An update that announces a new size was drawn partly at the old one, so
    /// the screen is asked for again until one arrives whole.
    pub fn capture(&mut self) -> Result<Screen> {
        for _ in 0..CAPTURE_ATTEMPTS {
            if let Some(screen) = self.capture_once()? {
                return Ok(screen);
            }
        }
        bail!("the screen kept changing size")
    }

    fn capture_once(&mut self) -> Result<Option<Screen>> {
        let (width, height) = (self.width, self.height);
        let mut request = vec![3u8, 0];
        for value in [0, 0, width, height] {
            request.extend_from_slice(&value.to_be_bytes());
        }
        self.stream.write_all(&request)?;

        let mut rgb = self.blank();
        loop {
            match self.u8()? {
                0 => {
                    self.u8()?;
                    let rectangles = self.u16()?;
                    let mut resized = false;
                    for _ in 0..rectangles {
                        if self.read_rectangle(&mut rgb)? {
                            resized = true;
                            rgb = self.blank();
                        }
                    }
                    if resized {
                        return Ok(None);
                    }
                    return Ok(Some(Screen {
                        width: u32::from(width),
                        height: u32::from(height),
                        rgb,
                    }));
                }
                // Colour map entries: never asked for, but skipped if sent.
                1 => {
                    let mut header = [0u8; 5];
                    self.stream.read_exact(&mut header)?;
                    let entries = u16::from_be_bytes([header[3], header[4]]);
                    self.skip(usize::from(entries) * 6)?;
                }
                // Bell.
                2 => {}
                // Clipboard text.
                3 => {
                    self.skip(3)?;
                    let length = self.u32()?;
                    self.skip(length as usize)?;
                }
                other => bail!("the server sent message type {other}, which vitro does not read"),
            }
        }
    }

    fn blank(&self) -> Vec<u8> {
        vec![0u8; usize::from(self.width) * usize::from(self.height) * 3]
    }

    /// Read one rectangle into `rgb`; whether it announced a new screen size.
    fn read_rectangle(&mut self, rgb: &mut [u8]) -> Result<bool> {
        let x = usize::from(self.u16()?);
        let y = usize::from(self.u16()?);
        let (w16, h16) = (self.u16()?, self.u16()?);
        let (w, h) = (usize::from(w16), usize::from(h16));
        match self.u32()? as i32 {
            ENCODING_RAW => {}
            ENCODING_DESKTOP_SIZE => return Ok(self.resize(w16, h16)),
            ENCODING_EXTENDED_DESKTOP_SIZE => {
                let screens = self.u8()?;
                self.skip(3 + usize::from(screens) * 16)?;
                return Ok(self.resize(w16, h16));
            }
            other => bail!("the server sent encoding {other}, which vitro did not ask for"),
        }
        let mut pixels = vec![0u8; w * h * 4];
        self.stream.read_exact(&mut pixels)?;
        let width = usize::from(self.width);
        for row in 0..h {
            for column in 0..w {
                let (px, py) = (x + column, y + row);
                if px >= width || py >= usize::from(self.height) {
                    continue;
                }
                let source = (row * w + column) * 4;
                let target = (py * width + px) * 3;
                rgb[target..target + 3].copy_from_slice(&rgb_of(&pixels[source..source + 4]));
            }
        }
        Ok(false)
    }

    fn resize(&mut self, width: u16, height: u16) -> bool {
        let changed = (width, height) != (self.width, self.height);
        self.width = width;
        self.height = height;
        changed
    }

    fn u8(&mut self) -> Result<u8> {
        let mut buffer = [0u8; 1];
        self.stream.read_exact(&mut buffer)?;
        Ok(buffer[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let mut buffer = [0u8; 2];
        self.stream.read_exact(&mut buffer)?;
        Ok(u16::from_be_bytes(buffer))
    }

    fn u32(&mut self) -> Result<u32> {
        let mut buffer = [0u8; 4];
        self.stream.read_exact(&mut buffer)?;
        Ok(u32::from_be_bytes(buffer))
    }

    fn skip(&mut self, count: usize) -> Result<()> {
        let mut buffer = vec![0u8; count];
        self.stream.read_exact(&mut buffer)?;
        Ok(())
    }

    fn reason(&mut self) -> Result<String> {
        let length = self.u32()?;
        let mut reason = vec![0u8; length as usize];
        self.stream.read_exact(&mut reason)?;
        Ok(String::from_utf8_lossy(&reason).into_owned())
    }
}

fn pointer_event(buttons: u8, x: u16, y: u16) -> [u8; 6] {
    let [x1, x2] = x.to_be_bytes();
    let [y1, y2] = y.to_be_bytes();
    [5, buttons, x1, x2, y1, y2]
}

fn key_event(keysym: u32, down: bool) -> [u8; 8] {
    let mut message = [4u8, u8::from(down), 0, 0, 0, 0, 0, 0];
    message[4..].copy_from_slice(&keysym.to_be_bytes());
    message
}

/// One pixel in the format asked for: little-endian, red at bit 16.
fn rgb_of(pixel: &[u8]) -> [u8; 3] {
    [pixel[2], pixel[1], pixel[0]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lumes_url_gives_the_address_and_the_password() {
        let target = parse_url("vnc://:sheep-rabbit-moon@127.0.0.1:56123").unwrap();

        assert_eq!(
            target,
            Target {
                host: "127.0.0.1".into(),
                port: 56123,
                password: Some("sheep-rabbit-moon".into()),
            }
        );
    }

    #[test]
    fn a_url_without_a_password_asks_for_none() {
        let target = parse_url("vnc://localhost:5900").unwrap();

        assert_eq!(target.password, None);
        assert_eq!(target.port, 5900);
    }

    #[test]
    fn an_escaped_password_is_read_as_written() {
        let target = parse_url("vnc://:a%40b@127.0.0.1:5900").unwrap();

        assert_eq!(target.password.as_deref(), Some("a@b"));
    }

    #[test]
    fn something_that_is_not_a_vnc_url_is_refused() {
        assert!(parse_url("http://127.0.0.1:5900").is_err());
        assert!(parse_url("vnc://127.0.0.1").is_err());
    }

    #[test]
    fn the_challenge_is_answered_with_the_bit_reversed_password_as_the_key() {
        // Computed independently: `openssl enc -des-ecb -nopad` with the key
        // 0e86ceceeef64e26, which is "password" with every byte reversed.
        let response = auth_response("password", b"0123456789abcdef");

        assert_eq!(
            response,
            [
                0x56, 0x45, 0xab, 0xeb, 0x5f, 0x1e, 0x64, 0x75, 0xe8, 0xfe, 0xb1, 0x1b, 0xeb, 0x66,
                0xea, 0x19
            ]
        );
    }

    #[test]
    fn a_key_event_carries_its_direction_and_its_keysym() {
        assert_eq!(key_event(0xffe2, true), [4, 1, 0, 0, 0, 0, 0xff, 0xe2]);
        assert_eq!(key_event(0x61, false), [4, 0, 0, 0, 0, 0, 0, 0x61]);
    }

    #[test]
    fn a_pointer_event_carries_the_buttons_and_the_position() {
        assert_eq!(
            pointer_event(1, 300, 0x0102),
            [5, 1, 0x01, 0x2c, 0x01, 0x02]
        );
    }

    #[test]
    fn a_pixel_is_read_in_the_format_vitro_asked_for() {
        // Little-endian 0x00RRGGBB: blue first in memory.
        assert_eq!(rgb_of(&[0x30, 0x20, 0x10, 0x00]), [0x10, 0x20, 0x30]);
    }
}
