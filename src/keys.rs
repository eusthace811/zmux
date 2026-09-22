//! Keystrokes as the host terminal sent them, and the two things the client
//! still has to do with one.
//!
//! The console is read in VT input mode (`console.rs`), so a keystroke
//! arrives already xterm-encoded - which is what `TERM=xterm-256color`
//! promises the pane's program - and the client forwards it untouched. There
//! used to be an encoder here, from crossterm key events back to those same
//! bytes; with the terminal doing the encoding there is nothing to rebuild.
//!
//! What is left:
//! - `decode`, for the handful of keys zmux acts on itself: the commands
//!   after the prefix, and scroll mode.
//! - `for_pane`, because only the DAEMON knows whether the focused pane asked
//!   for application cursor keys, and the host does not.

use std::borrow::Cow;

/// A keystroke zmux might act on itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Esc,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// Anything else. Forwarded when typed, ignored where a command is read.
    Other,
}

/// A keystroke and whether Ctrl was held.
///
/// Ctrl is kept apart from the key rather than folded into it because the
/// prefix commands take either: `C-b C-x` closes a pane as `C-b x` does, for
/// whoever is still holding Ctrl from the prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stroke {
    pub key: Key,
    pub ctrl: bool,
}

/// Interpret one keystroke's bytes, as `input::Decoder` framed them.
pub fn decode(bytes: &[u8]) -> Stroke {
    let plain = |key| Stroke { key, ctrl: false };
    match bytes {
        b"\r" | b"\n" => plain(Key::Enter),
        b"\x1b" => plain(Key::Esc),
        // Ctrl-letter is the letter's position in the alphabet. Enter and Tab
        // are two of those and are matched above or left as Other.
        [b @ 0x01..=0x1a] if *b != b'\t' && *b != 0x08 => {
            Stroke { key: Key::Char((b'a' + b - 1) as char), ctrl: true }
        }
        [0x1b, b'[' | b'O', rest @ ..] => sequence(rest),
        _ => match std::str::from_utf8(bytes).ok().and_then(single_char) {
            Some(c) if !c.is_control() => plain(Key::Char(c)),
            _ => plain(Key::Other),
        },
    }
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

/// What follows `CSI` or `SS3`: an optional `1;<mod>` or `<n>;<mod>`, then
/// a letter or `~`.
fn sequence(rest: &[u8]) -> Stroke {
    let Some((&last, params)) = rest.split_last() else {
        return Stroke { key: Key::Other, ctrl: false };
    };
    let params = std::str::from_utf8(params).unwrap_or("");
    let mut fields = params.split(';');
    let number = fields.next().unwrap_or("");
    // xterm's modifier parameter: 1 + (shift=1, alt=2, ctrl=4).
    let ctrl = fields
        .next()
        .and_then(|m| m.parse::<u8>().ok())
        .is_some_and(|m| m.saturating_sub(1) & 4 != 0);
    let key = match (last, number) {
        (b'A', _) => Key::Up,
        (b'B', _) => Key::Down,
        (b'C', _) => Key::Right,
        (b'D', _) => Key::Left,
        (b'H', _) | (b'~', "1" | "7") => Key::Home,
        (b'F', _) | (b'~', "4" | "8") => Key::End,
        (b'~', "5") => Key::PageUp,
        (b'~', "6") => Key::PageDown,
        _ => Key::Other,
    };
    Stroke { key, ctrl }
}

/// The bytes to send the focused pane for one keystroke.
///
/// Application cursor mode (DECCKM) swaps `CSI` for `SS3` on the unmodified
/// arrows and Home/End. The PANE's program set it, so the host never heard
/// about it and sends the normal form - and without the swap, arrows are
/// wrong in vim and in readline's vi mode. Both directions, so a host that
/// is itself in application mode for some reason cannot confuse a pane that
/// is not. Modified keys have one form either way and pass untouched.
pub fn for_pane(bytes: &[u8], app_cursor: bool) -> Cow<'_, [u8]> {
    match bytes {
        [0x1b, intro @ (b'[' | b'O'), f @ (b'A' | b'B' | b'C' | b'D' | b'H' | b'F')] => {
            let want = if app_cursor { b'O' } else { b'[' };
            if *intro == want {
                Cow::Borrowed(bytes)
            } else {
                Cow::Owned(vec![0x1b, want, *f])
            }
        }
        _ => Cow::Borrowed(bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(k: Key) -> Stroke {
        Stroke { key: k, ctrl: false }
    }
    fn ctrl(k: Key) -> Stroke {
        Stroke { key: k, ctrl: true }
    }

    #[test]
    fn characters_and_control_letters() {
        assert_eq!(decode(b"d"), key(Key::Char('d')));
        assert_eq!(decode(b"%"), key(Key::Char('%')));
        assert_eq!(decode("é".as_bytes()), key(Key::Char('é')));
        // Ctrl-x after the prefix is still `x`, for a thumb still on Ctrl.
        assert_eq!(decode(b"\x18"), ctrl(Key::Char('x')));
        assert_eq!(decode(b"\x02"), ctrl(Key::Char('b')));
    }

    #[test]
    fn enter_escape_and_tab_are_not_ctrl_letters() {
        assert_eq!(decode(b"\r"), key(Key::Enter));
        assert_eq!(decode(b"\x1b"), key(Key::Esc));
        assert_eq!(decode(b"\t"), key(Key::Other));
    }

    #[test]
    fn navigation_in_both_encodings() {
        assert_eq!(decode(b"\x1b[A"), key(Key::Up));
        assert_eq!(decode(b"\x1bOA"), key(Key::Up));
        assert_eq!(decode(b"\x1b[H"), key(Key::Home));
        assert_eq!(decode(b"\x1b[1~"), key(Key::Home));
        assert_eq!(decode(b"\x1b[4~"), key(Key::End));
        assert_eq!(decode(b"\x1b[5~"), key(Key::PageUp));
        assert_eq!(decode(b"\x1b[6~"), key(Key::PageDown));
    }

    /// `C-b C-<arrow>` resizes where `C-b <arrow>` moves focus, so Ctrl on an
    /// arrow is the one modifier that must survive.
    #[test]
    fn ctrl_arrows_say_so() {
        assert_eq!(decode(b"\x1b[1;5C"), ctrl(Key::Right));
        assert_eq!(decode(b"\x1b[1;2C"), key(Key::Right), "shift is not ctrl");
        assert_eq!(decode(b"\x1b[1;6D"), ctrl(Key::Left), "ctrl-shift is still ctrl");
    }

    #[test]
    fn anything_else_is_other() {
        assert_eq!(decode(b"\x1b[15~"), key(Key::Other));
        assert_eq!(decode(b"\x1bb"), key(Key::Other));
        assert_eq!(decode(b"ab"), key(Key::Other));
    }

    /// The reason arrows misbehave in vim without DECCKM support.
    #[test]
    fn application_cursor_mode_changes_the_arrows() {
        assert_eq!(&*for_pane(b"\x1b[A", false), b"\x1b[A");
        assert_eq!(&*for_pane(b"\x1b[A", true), b"\x1bOA");
        assert_eq!(&*for_pane(b"\x1b[H", true), b"\x1bOH");
        assert_eq!(&*for_pane(b"\x1bOB", false), b"\x1b[B");
    }

    #[test]
    fn a_modified_arrow_ignores_application_mode() {
        assert_eq!(&*for_pane(b"\x1b[1;5A", true), b"\x1b[1;5A");
        assert_eq!(&*for_pane(b"\x1bOP", false), b"\x1bOP", "F1 is not an arrow");
        assert_eq!(&*for_pane(b"x", true), b"x");
    }
}
