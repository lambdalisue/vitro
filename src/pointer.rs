//! Mouse buttons and wheel directions, as the GUI commands take them and as
//! each way into a guest needs them.
//!
//! QEMU names buttons (`left`, `wheel-up`); VNC packs them into one byte, a
//! bit per button, and a wheel notch is a press and release of its bit. Both
//! translations live here so neither transport has to know the other's.

use clap::ValueEnum;

/// A mouse button that can be clicked or held through a drag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Button {
    Left,
    Middle,
    Right,
}

impl Button {
    /// QEMU's name for the button in an `input-send-event` call.
    pub fn qemu_name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Middle => "middle",
            Button::Right => "right",
        }
    }

    /// The button's bit in an RFB pointer event.
    pub fn vnc_mask(self) -> u8 {
        match self {
            Button::Left => 1,
            Button::Middle => 1 << 1,
            Button::Right => 1 << 2,
        }
    }
}

/// Which way a wheel notch turns the view.
///
/// Vertical only: QEMU accepts `wheel-left` and `wheel-right`, but its USB
/// tablet reports one wheel axis and drops them, so a guest never sees them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Wheel {
    Up,
    Down,
}

impl Wheel {
    /// QEMU's name for the notch, which it treats as a button.
    pub fn qemu_name(self) -> &'static str {
        match self {
            Wheel::Up => "wheel-up",
            Wheel::Down => "wheel-down",
        }
    }

    /// The notch's bit in an RFB pointer event: buttons 4 and 5.
    pub fn vnc_mask(self) -> u8 {
        match self {
            Wheel::Up => 1 << 3,
            Wheel::Down => 1 << 4,
        }
    }
}

/// The points a drag passes through after `from`, ending exactly on `to`.
///
/// A drag that jumps straight to its end is a press here and a release there,
/// which many toolkits read as a click rather than a drag: they want to see
/// the pointer travel with the button down.
pub fn travel(from: (u32, u32), to: (u32, u32), steps: u32) -> Vec<(u32, u32)> {
    let steps = steps.max(1);
    let along = |a: u32, b: u32, i: u32| {
        let (a, b) = (i64::from(a), i64::from(b));
        (a + (b - a) * i64::from(i) / i64::from(steps)) as u32
    };
    (1..=steps)
        .map(|i| (along(from.0, to.0, i), along(from.1, to.1, i)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_have_qemus_names() {
        assert_eq!(Button::Left.qemu_name(), "left");
        assert_eq!(Button::Middle.qemu_name(), "middle");
        assert_eq!(Button::Right.qemu_name(), "right");
    }

    #[test]
    fn wheel_notches_have_qemus_button_names() {
        assert_eq!(Wheel::Up.qemu_name(), "wheel-up");
        assert_eq!(Wheel::Down.qemu_name(), "wheel-down");
    }

    #[test]
    fn buttons_take_the_first_three_bits_of_an_rfb_pointer_event() {
        assert_eq!(Button::Left.vnc_mask(), 0b0000_0001);
        assert_eq!(Button::Middle.vnc_mask(), 0b0000_0010);
        assert_eq!(Button::Right.vnc_mask(), 0b0000_0100);
    }

    #[test]
    fn wheel_notches_take_buttons_four_and_five() {
        assert_eq!(Wheel::Up.vnc_mask(), 0b0000_1000);
        assert_eq!(Wheel::Down.vnc_mask(), 0b0001_0000);
    }

    #[test]
    fn a_drag_travels_in_even_steps_and_ends_where_it_was_asked_to() {
        assert_eq!(
            travel((0, 100), (40, 60), 4),
            [(10, 90), (20, 80), (30, 70), (40, 60)]
        );
    }

    #[test]
    fn a_drag_of_no_steps_still_reaches_its_end() {
        assert_eq!(travel((5, 5), (9, 9), 0), [(9, 9)]);
    }
}
