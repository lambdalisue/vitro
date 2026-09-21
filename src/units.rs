//! The small value types the configuration is written in.
//!
//! Sizes and resolutions arrive as strings a person typed, so every parse
//! failure has to name the key and show what was wrong with it — the
//! configuration file is the first thing a new user gets wrong.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A byte count written the way people write them: `8`, `8G`, `8GB`, `8192MB`.
///
/// A bare number means gibibytes, matching how `qemu` and `lume` read the same
/// kind of value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ByteSize(u64);

impl ByteSize {
    pub const fn from_bytes(bytes: u64) -> Self {
        Self(bytes)
    }

    pub const fn from_gib(gib: u64) -> Self {
        Self(gib * 1024 * 1024 * 1024)
    }

    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// QEMU's `-m` takes mebibytes, and so does `lume --memory`.
    pub const fn mib(self) -> u64 {
        self.0 / (1024 * 1024)
    }
}

impl FromStr for ByteSize {
    type Err = ParseSizeError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        let digits_end = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (number, suffix) = text.split_at(digits_end);

        if number.is_empty() {
            return Err(ParseSizeError(s.to_string()));
        }
        let value: u64 = number.parse().map_err(|_| ParseSizeError(s.to_string()))?;

        let multiplier = match suffix.trim().to_ascii_uppercase().as_str() {
            "" | "G" | "GB" | "GIB" => 1024 * 1024 * 1024,
            "M" | "MB" | "MIB" => 1024 * 1024,
            "K" | "KB" | "KIB" => 1024,
            "T" | "TB" | "TIB" => 1024_u64.pow(4),
            "B" => 1,
            _ => return Err(ParseSizeError(s.to_string())),
        };

        value
            .checked_mul(multiplier)
            .map(ByteSize)
            .ok_or_else(|| ParseSizeError(s.to_string()))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNIT: u64 = 1024;
        let (value, suffix) = match self.0 {
            b if b >= UNIT.pow(4) => (b as f64 / UNIT.pow(4) as f64, "T"),
            b if b >= UNIT.pow(3) => (b as f64 / UNIT.pow(3) as f64, "G"),
            b if b >= UNIT.pow(2) => (b as f64 / UNIT.pow(2) as f64, "M"),
            b if b >= UNIT => (b as f64 / UNIT as f64, "K"),
            b => return write!(f, "{b}B"),
        };
        if (value.fract()).abs() < f64::EPSILON {
            write!(f, "{value:.0}{suffix}")
        } else {
            write!(f, "{value:.1}{suffix}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseSizeError(String);

impl fmt::Display for ParseSizeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid size {:?}: expected a number optionally followed by K, M, G or T (a bare number means GiB)",
            self.0
        )
    }
}

impl std::error::Error for ParseSizeError {}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // TOML lets `memory = 8` and `memory = "8G"` both be natural to write.
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Number(u64),
            Text(String),
        }
        match Raw::deserialize(d)? {
            Raw::Number(gib) => Ok(ByteSize::from_gib(gib)),
            Raw::Text(text) => text.parse().map_err(serde::de::Error::custom),
        }
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

/// The guest's display size, written `1280x800`.
///
/// This reaches the guest as a QEMU argument on the virtio-gpu device, so the
/// guest never has to be told what size its screen is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

impl Resolution {
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }
}

impl FromStr for Resolution {
    type Err = ParseResolutionError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let text = s.trim();
        let (w, h) = text
            .split_once(['x', 'X'])
            .ok_or_else(|| ParseResolutionError(s.to_string()))?;
        let width = w
            .trim()
            .parse()
            .map_err(|_| ParseResolutionError(s.to_string()))?;
        let height = h
            .trim()
            .parse()
            .map_err(|_| ParseResolutionError(s.to_string()))?;
        if width == 0 || height == 0 {
            return Err(ParseResolutionError(s.to_string()));
        }
        Ok(Self { width, height })
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}x{}", self.width, self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResolutionError(String);

impl fmt::Display for ParseResolutionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid resolution {:?}: expected WIDTHxHEIGHT, for example 1280x800",
            self.0
        )
    }
}

impl std::error::Error for ParseResolutionError {}

impl<'de> Deserialize<'de> for Resolution {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for Resolution {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_number_is_gibibytes() {
        assert_eq!("8".parse::<ByteSize>().unwrap(), ByteSize::from_gib(8));
    }

    #[test]
    fn suffixes_are_case_insensitive_and_binary() {
        for text in ["8G", "8g", "8GB", "8gb", "8GiB"] {
            assert_eq!(
                text.parse::<ByteSize>().unwrap(),
                ByteSize::from_gib(8),
                "{text}"
            );
        }
        assert_eq!("8192MB".parse::<ByteSize>().unwrap(), ByteSize::from_gib(8));
        assert_eq!(
            "512B".parse::<ByteSize>().unwrap(),
            ByteSize::from_bytes(512)
        );
    }

    #[test]
    fn qemu_wants_mebibytes() {
        assert_eq!("8G".parse::<ByteSize>().unwrap().mib(), 8192);
    }

    #[test]
    fn a_size_without_digits_is_rejected_by_name() {
        let err = "lots".parse::<ByteSize>().unwrap_err().to_string();
        assert!(err.contains("\"lots\""), "{err}");
        assert!(err.contains("K, M, G or T"), "{err}");
    }

    #[test]
    fn an_unknown_suffix_is_rejected() {
        assert!("8Q".parse::<ByteSize>().is_err());
        assert!("".parse::<ByteSize>().is_err());
    }

    #[test]
    fn whole_sizes_round_trip_through_display() {
        for text in ["8G", "512M", "64K"] {
            let parsed: ByteSize = text.parse().unwrap();
            assert_eq!(parsed.to_string(), text);
        }
    }

    #[test]
    fn display_is_for_people_and_may_be_fractional() {
        // Only the parser has to be exact; `ls` shows sizes it never re-reads.
        assert_eq!(ByteSize::from_bytes(1_610_612_736).to_string(), "1.5G");
    }

    #[test]
    fn resolutions_parse_and_render() {
        let r: Resolution = "1280x800".parse().unwrap();
        assert_eq!(r, Resolution::new(1280, 800));
        assert_eq!(r.to_string(), "1280x800");
    }

    #[test]
    fn a_resolution_needs_both_halves_and_no_zeroes() {
        for text in ["1280", "1280x", "x800", "0x800", "1280x0", "wide"] {
            assert!(text.parse::<Resolution>().is_err(), "{text}");
        }
    }
}
