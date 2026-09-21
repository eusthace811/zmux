//! Encoding mouse events for the program running inside a pane.
//!
//! A program asks for the mouse with a DECSET like `?1000h`, and gets told
//! about it in a format it also chose (`?1006h` for SGR, or the original
//! single-byte encoding). vt100 tracks both for us; this turns one event into
//! the bytes that program is expecting, or `None` when it did not ask to hear
//! about this particular thing.

use vt100::{MouseProtocolEncoding as Enc, MouseProtocolMode as Mode};

use crate::protocol::{MouseIn, MouseKind, MOUSE_ALT, MOUSE_CTRL, MOUSE_SHIFT};

/// xterm's button bits for "motion", added to the button number.
const MOTION: u8 = 32;
/// The button number used when nothing is pressed, and - in the original
/// encoding only - for a release, which does not say WHICH button came up.
const NO_BUTTON: u8 = 3;

/// Encode `ev` for a program at pane-local, 0-based `(x, y)`.
///
/// `None` means send nothing: either the program is not listening at all, or
/// it asked for less than this - a program in plain `?1000h` does not want to
/// hear about motion - or the position cannot be expressed in the encoding it
/// chose.
pub fn encode(mode: Mode, enc: Enc, ev: MouseIn, x: u16, y: u16) -> Option<Vec<u8>> {
    let (button, motion, release) = parts(ev.kind);
    if !wanted(mode, ev.kind) {
        return None;
    }
    let mut bits = button | ev.mods_bits();
    if motion {
        bits |= MOTION;
    }
    match enc {
        // SGR reports which button was released, and says "release" with a
        // lowercase terminator instead of by forgetting the button.
        Enc::Sgr => {
            let end = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{};{}{}", bits, x + 1, y + 1, end).into_bytes())
        }
        Enc::Default | Enc::Utf8 => {
            if release {
                bits = NO_BUTTON | ev.mods_bits() | if motion { MOTION } else { 0 };
            }
            // Every field is a printable character offset by 32, so a
            // coordinate past 222 has nowhere to go. Dropping the event is
            // what xterm itself does; the alternative is reporting a
            // position somewhere else entirely.
            let (cx, cy) = (u32::from(x) + 33, u32::from(y) + 33);
            if cx > 255 || cy > 255 {
                return None;
            }
            let mut out = vec![0x1b, b'[', b'M'];
            for v in [u32::from(bits) + 32, cx, cy] {
                match enc {
                    Enc::Utf8 => {
                        let c = char::from_u32(v)?;
                        let mut buf = [0u8; 4];
                        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                    _ => out.push(v as u8),
                }
            }
            Some(out)
        }
    }
}

impl MouseIn {
    /// The modifier bits xterm adds to the button number.
    fn mods_bits(&self) -> u8 {
        let mut b = 0;
        if self.mods & MOUSE_SHIFT != 0 { b |= 4 }
        if self.mods & MOUSE_ALT != 0 { b |= 8 }
        if self.mods & MOUSE_CTRL != 0 { b |= 16 }
        b
    }
}

/// (button number, is it motion, is it a release).
fn parts(kind: MouseKind) -> (u8, bool, bool) {
    match kind {
        MouseKind::Press(b) => (b, false, false),
        MouseKind::Release(b) => (b, false, true),
        MouseKind::Drag(b) => (b, true, false),
        MouseKind::Move => (NO_BUTTON, true, false),
        // The wheel is reported as a press of a button that is never released.
        MouseKind::WheelUp => (64, false, false),
        MouseKind::WheelDown => (65, false, false),
    }
}

/// Did a program in this mode ask to hear about this?
///
/// Sending more than was asked for is not harmless: a program in `?1000h`
/// parses a motion report as a press somewhere it never happened.
fn wanted(mode: Mode, kind: MouseKind) -> bool {
    match mode {
        Mode::None => false,
        // X10: presses only. No releases, no motion.
        Mode::Press => matches!(
            kind,
            MouseKind::Press(_) | MouseKind::WheelUp | MouseKind::WheelDown
        ),
        Mode::PressRelease => !matches!(kind, MouseKind::Drag(_) | MouseKind::Move),
        Mode::ButtonMotion => !matches!(kind, MouseKind::Move),
        Mode::AnyMotion => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: MouseKind) -> MouseIn {
        MouseIn { x: 0, y: 0, kind, mods: 0 }
    }

    #[test]
    fn a_program_that_never_asked_hears_nothing() {
        assert_eq!(
            encode(Mode::None, Enc::Sgr, ev(MouseKind::Press(0)), 0, 0),
            None
        );
    }

    /// The coordinates a program is told are its OWN, not the screen's -
    /// otherwise every click in a right-hand pane lands off the end of what
    /// the program thinks it is drawing on. They are also 1-based on the
    /// wire while everything inside zmux counts from 0.
    #[test]
    fn sgr_reports_one_based_pane_local_coordinates() {
        let out = encode(Mode::AnyMotion, Enc::Sgr, ev(MouseKind::Press(0)), 4, 9);
        assert_eq!(out.as_deref(), Some(&b"\x1b[<0;5;10M"[..]));
    }

    /// SGR says "released" with a lowercase terminator, which is the whole
    /// reason it exists: the original encoding cannot say WHICH button.
    #[test]
    fn sgr_says_which_button_came_up_and_the_old_encoding_cannot() {
        let sgr = encode(Mode::PressRelease, Enc::Sgr, ev(MouseKind::Release(2)), 0, 0);
        assert_eq!(sgr.as_deref(), Some(&b"\x1b[<2;1;1m"[..]));
        let old = encode(Mode::PressRelease, Enc::Default, ev(MouseKind::Release(2)), 0, 0);
        assert_eq!(old.as_deref(), Some(&b"\x1b[M\x23\x21\x21"[..]), "button 3 means 'some button'");
    }

    #[test]
    fn the_wheel_is_a_button_that_is_never_released() {
        let up = encode(Mode::Press, Enc::Sgr, ev(MouseKind::WheelUp), 0, 0);
        let down = encode(Mode::Press, Enc::Sgr, ev(MouseKind::WheelDown), 0, 0);
        assert_eq!(up.as_deref(), Some(&b"\x1b[<64;1;1M"[..]));
        assert_eq!(down.as_deref(), Some(&b"\x1b[<65;1;1M"[..]));
    }

    /// Sending more than was asked for is not harmless: a program in `?1000h`
    /// reads a motion report as a press somewhere it never happened.
    #[test]
    fn each_mode_hears_only_what_it_asked_for() {
        let cases = [
            (Mode::Press, MouseKind::Release(0), false),
            (Mode::Press, MouseKind::Drag(0), false),
            (Mode::PressRelease, MouseKind::Release(0), true),
            (Mode::PressRelease, MouseKind::Drag(0), false),
            (Mode::ButtonMotion, MouseKind::Drag(0), true),
            (Mode::ButtonMotion, MouseKind::Move, false),
            (Mode::AnyMotion, MouseKind::Move, true),
        ];
        for (mode, kind, heard) in cases {
            let got = encode(mode, Enc::Sgr, ev(kind), 0, 0).is_some();
            assert_eq!(got, heard, "{mode:?} {kind:?}");
        }
    }

    #[test]
    fn motion_sets_the_motion_bit_and_a_bare_move_has_no_button() {
        let drag = encode(Mode::ButtonMotion, Enc::Sgr, ev(MouseKind::Drag(1)), 0, 0);
        assert_eq!(drag.as_deref(), Some(&b"\x1b[<33;1;1M"[..]), "1 | 32");
        let mv = encode(Mode::AnyMotion, Enc::Sgr, ev(MouseKind::Move), 0, 0);
        assert_eq!(mv.as_deref(), Some(&b"\x1b[<35;1;1M"[..]), "3 | 32");
    }

    #[test]
    fn modifiers_ride_on_the_button_number() {
        let e = MouseIn { x: 0, y: 0, kind: MouseKind::Press(0), mods: MOUSE_CTRL | MOUSE_SHIFT };
        assert_eq!(
            encode(Mode::AnyMotion, Enc::Sgr, e, 0, 0).as_deref(),
            Some(&b"\x1b[<20;1;1M"[..]),
            "4 shift | 16 ctrl"
        );
    }

    /// `?1005` exists to lift the 223-column ceiling, and here it does not.
    ///
    /// Pinned as a DECISION rather than left as an oversight. UTF-8 mode
    /// encodes each field as a codepoint, so it could describe column 2015 -
    /// but it is ambiguous by construction (a reader cannot tell a two-byte
    /// sequence from two one-byte fields without knowing the mode in
    /// advance), it is deprecated in xterm's own documentation, and the
    /// programs that ask for a mouse at all ask for SGR. Emitting bytes a
    /// confused reader turns into a click somewhere else is worse than
    /// emitting none, which is the same rule `wanted()` follows. SGR, which
    /// is unambiguous decimal text, has no ceiling and is what a wide pane
    /// should be using.
    #[test]
    fn utf8_mode_encodes_as_codepoints_and_keeps_the_old_ceiling() {
        // Below the ceiling it is the ordinary encoding, one byte per field.
        let low = encode(Mode::Press, Enc::Utf8, ev(MouseKind::Press(0)), 4, 9);
        assert_eq!(low.as_deref(), Some(&b"\x1b[M\x20\x25\x2a"[..]));

        // Above 127 a field becomes a two-byte UTF-8 sequence, which is the
        // whole difference from `Default`.
        let high = encode(Mode::Press, Enc::Utf8, ev(MouseKind::Press(0)), 200, 0);
        let default = encode(Mode::Press, Enc::Default, ev(MouseKind::Press(0)), 200, 0);
        assert_ne!(high, default, "utf8 mode encoded a high column as a raw byte");
        assert_eq!(
            high.as_deref(),
            Some("\x1b[M\u{20}\u{e9}\u{21}".as_bytes()),
            "a field past 127 must be its codepoint, UTF-8 encoded"
        );

        // And the ceiling is the same as the old encoding's, on purpose.
        assert!(encode(Mode::Press, Enc::Utf8, ev(MouseKind::Press(0)), 222, 0).is_some());
        assert!(encode(Mode::Press, Enc::Utf8, ev(MouseKind::Press(0)), 223, 0).is_none());
        assert!(
            encode(Mode::Press, Enc::Sgr, ev(MouseKind::Press(0)), 999, 0).is_some(),
            "a pane wider than the old ceiling needs SGR, which has none"
        );
    }

    /// The original encoding offsets every field by 32 into a single byte, so
    /// a pane wider than 223 columns has positions it cannot describe. xterm
    /// drops those reports rather than naming the wrong cell, and so do we -
    /// while SGR, which is decimal text, keeps going.
    #[test]
    fn the_old_encoding_gives_up_past_the_column_it_cannot_reach() {
        assert!(encode(Mode::Press, Enc::Default, ev(MouseKind::Press(0)), 222, 0).is_some());
        assert!(encode(Mode::Press, Enc::Default, ev(MouseKind::Press(0)), 223, 0).is_none());
        assert!(encode(Mode::Press, Enc::Sgr, ev(MouseKind::Press(0)), 223, 0).is_some());
    }
}
