//! Key names, as `vitro key` takes them and as each way into a guest needs them.
//!
//! The names are QEMU's key codes (`shift`, `ret`, `a`), because QEMU is the
//! one consumer that takes names at all: they go to it as they are. VNC, the
//! way into a lume guest, takes X keysyms instead, and the table below is the
//! translation. A name with no keysym is refused before anything is pressed,
//! so a chord never arrives half typed.

use anyhow::{bail, Result};

/// One chord: the keys held together, modifiers first, in the order given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord(Vec<&'static str>);

impl Chord {
    /// Read `shift-a` or `ctrl-alt-delete`. A lone `-` names the minus key,
    /// so `shift--` is Shift with minus.
    pub fn parse(text: &str) -> Result<Self> {
        let mut names = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            let (name, next) = match rest.strip_prefix('-') {
                // A leading dash here is the minus key itself, never a separator.
                Some(after) => ("-", after.strip_prefix('-').unwrap_or(after)),
                None => match rest.split_once('-') {
                    Some((name, after)) => (name, after),
                    None => (rest, ""),
                },
            };
            let canonical = match name {
                "-" => "minus",
                name => name,
            };
            let Some(&(known, _)) = KEYS.iter().find(|(key, _)| *key == canonical) else {
                bail!("{text:?}: no key is called {name:?}; names are QEMU's, such as shift, ret, a, f1");
            };
            names.push(known);
            rest = next;
        }
        if names.is_empty() {
            bail!("an empty chord presses nothing");
        }
        Ok(Self(names))
    }

    /// QEMU's names for the keys, in the order they go down.
    pub fn names(&self) -> &[&'static str] {
        &self.0
    }

    /// The X keysyms for the keys, in the order they go down.
    pub fn keysyms(&self) -> Vec<u32> {
        self.0
            .iter()
            .map(|name| {
                KEYS.iter()
                    .find(|(key, _)| key == name)
                    .map(|(_, keysym)| *keysym)
                    .expect("a parsed chord holds only known names")
            })
            .collect()
    }
}

/// Every key `vitro key` knows: its QEMU name and its X keysym.
///
/// A letter's keysym is the lowercase letter whatever the modifiers, as on a
/// physical keyboard; Shift is pressed as its own key.
const KEYS: &[(&str, u32)] = &[
    ("a", 0x61),
    ("b", 0x62),
    ("c", 0x63),
    ("d", 0x64),
    ("e", 0x65),
    ("f", 0x66),
    ("g", 0x67),
    ("h", 0x68),
    ("i", 0x69),
    ("j", 0x6a),
    ("k", 0x6b),
    ("l", 0x6c),
    ("m", 0x6d),
    ("n", 0x6e),
    ("o", 0x6f),
    ("p", 0x70),
    ("q", 0x71),
    ("r", 0x72),
    ("s", 0x73),
    ("t", 0x74),
    ("u", 0x75),
    ("v", 0x76),
    ("w", 0x77),
    ("x", 0x78),
    ("y", 0x79),
    ("z", 0x7a),
    ("0", 0x30),
    ("1", 0x31),
    ("2", 0x32),
    ("3", 0x33),
    ("4", 0x34),
    ("5", 0x35),
    ("6", 0x36),
    ("7", 0x37),
    ("8", 0x38),
    ("9", 0x39),
    ("minus", 0x2d),
    ("equal", 0x3d),
    ("bracket_left", 0x5b),
    ("bracket_right", 0x5d),
    ("backslash", 0x5c),
    ("semicolon", 0x3b),
    ("apostrophe", 0x27),
    ("grave_accent", 0x60),
    ("comma", 0x2c),
    ("dot", 0x2e),
    ("slash", 0x2f),
    ("spc", 0x20),
    ("ret", 0xff0d),
    ("tab", 0xff09),
    ("esc", 0xff1b),
    ("backspace", 0xff08),
    ("delete", 0xffff),
    ("insert", 0xff63),
    ("home", 0xff50),
    ("end", 0xff57),
    ("pgup", 0xff55),
    ("pgdn", 0xff56),
    ("left", 0xff51),
    ("up", 0xff52),
    ("right", 0xff53),
    ("down", 0xff54),
    ("shift", 0xffe1),
    ("shift_r", 0xffe2),
    ("ctrl", 0xffe3),
    ("ctrl_r", 0xffe4),
    // Option and Command as the VNC server in Virtualization.framework reads
    // them — the one VNC server vitro talks to: it takes Meta for Option and
    // Alt for Command, so a Mac key arrives as the key QEMU would press.
    ("alt", 0xffe7),
    ("alt_r", 0xffe8),
    // The Windows key, and Command on a Mac.
    ("meta_l", 0xffe9),
    ("meta_r", 0xffea),
    ("caps_lock", 0xffe5),
    ("f1", 0xffbe),
    ("f2", 0xffbf),
    ("f3", 0xffc0),
    ("f4", 0xffc1),
    ("f5", 0xffc2),
    ("f6", 0xffc3),
    ("f7", 0xffc4),
    ("f8", 0xffc5),
    ("f9", 0xffc6),
    ("f10", 0xffc7),
    ("f11", 0xffc8),
    ("f12", 0xffc9),
    ("henkan", 0xff23),
    ("muhenkan", 0xff22),
    ("katakanahiragana", 0xff27),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chord_names_its_keys_in_the_order_they_go_down() {
        let chord = Chord::parse("ctrl-alt-delete").unwrap();

        assert_eq!(chord.names(), ["ctrl", "alt", "delete"]);
        assert_eq!(chord.keysyms(), [0xffe3, 0xffe7, 0xffff]);
    }

    #[test]
    fn a_single_key_is_a_chord_of_one() {
        assert_eq!(Chord::parse("shift_r").unwrap().names(), ["shift_r"]);
    }

    #[test]
    fn a_dash_on_its_own_is_the_minus_key() {
        // Otherwise there would be no way to name it in a chord at all.
        assert_eq!(Chord::parse("-").unwrap().names(), ["minus"]);
        assert_eq!(Chord::parse("shift--").unwrap().names(), ["shift", "minus"]);
    }

    #[test]
    fn a_letter_is_its_lowercase_keysym_even_with_shift() {
        // Shift is its own key going down, as on a keyboard; sending `A`
        // would ask the guest for a key that does not exist.
        assert_eq!(Chord::parse("shift-k").unwrap().keysyms(), [0xffe1, 0x6b]);
    }

    #[test]
    fn command_and_option_are_sent_as_a_macs_vnc_server_reads_them() {
        // Sent as Super, Command arrives as nothing at all; sent as Meta, it
        // arrives as Option. Alt is what that server turns into Command.
        assert_eq!(Chord::parse("meta_l-s").unwrap().keysyms(), [0xffe9, 0x73]);
        assert_eq!(Chord::parse("alt").unwrap().keysyms(), [0xffe7]);
    }

    #[test]
    fn an_unknown_name_is_refused_and_named() {
        let err = Chord::parse("shift-enter").unwrap_err().to_string();

        assert!(err.contains("\"enter\""), "{err}");
    }

    #[test]
    fn an_empty_chord_is_refused() {
        assert!(Chord::parse("").is_err());
    }

    #[test]
    fn every_name_is_unique() {
        let mut names: Vec<_> = KEYS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), KEYS.len());
    }
}
