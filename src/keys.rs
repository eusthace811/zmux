//! Keyboard encoding: crossterm events -> the bytes a terminal program expects.
//!
//! The Phase 1 encoder handled about twenty keys, which is fine for a shell
//! prompt and immediately annoying in anything else - no Alt, no function
//! keys, no modified navigation, and no application cursor mode, so arrow keys
//! misbehave in vim and readline.
//!
//! Encoding follows xterm's conventions, which is what `TERM=xterm-256color`
//! promises and what ncurses, readline and vim expect.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// xterm's modifier parameter: 1 + bitmask(shift=1, alt=2, ctrl=4).
fn modifier_param(m: KeyModifiers) -> u8 {
    1 + u8::from(m.contains(KeyModifiers::SHIFT))
        + 2 * u8::from(m.contains(KeyModifiers::ALT))
        + 4 * u8::from(m.contains(KeyModifiers::CONTROL))
}

/// `CSI <final>` unmodified, `CSI 1 ; <mod> <final>` when modified.
fn csi_letter(final_byte: char, m: KeyModifiers, app_cursor: bool) -> Vec<u8> {
    let p = modifier_param(m);
    if p == 1 {
        // Application cursor mode swaps CSI for SS3 on the arrows and Home/End.
        // Without this, arrows are wrong in vim and in readline's vi mode.
        let intro = if app_cursor { "\x1bO" } else { "\x1b[" };
        format!("{intro}{final_byte}").into_bytes()
    } else {
        format!("\x1b[1;{p}{final_byte}").into_bytes()
    }
}

/// `CSI <n> ~` unmodified, `CSI <n> ; <mod> ~` when modified.
fn csi_tilde(n: u8, m: KeyModifiers) -> Vec<u8> {
    let p = modifier_param(m);
    if p == 1 {
        format!("\x1b[{n}~").into_bytes()
    } else {
        format!("\x1b[{n};{p}~").into_bytes()
    }
}

/// Encode one key press. `app_cursor` reflects DECCKM for the focused pane.
///
/// Returns `None` for keys with no representation, so the caller sends nothing
/// rather than guessing.
pub fn encode(k: KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    // Windows reports press AND release; sending both doubles every keystroke.
    if matches!(k.kind, KeyEventKind::Release) {
        return None;
    }
    let m = k.modifiers;
    let ctrl = m.contains(KeyModifiers::CONTROL);
    let alt = m.contains(KeyModifiers::ALT);

    let bytes = match k.code {
        KeyCode::Char(c) => {
            let mut out = Vec::new();
            // Alt is the ESC prefix, which is how xterm has always sent it.
            if alt {
                out.push(0x1b);
            }
            if ctrl {
                // Ctrl maps @A-Z[\]^_ to 0x00-0x1f; ? to DEL.
                let upper = c.to_ascii_uppercase();
                match upper {
                    '@'..='_' => out.push(upper as u8 - b'@'),
                    '?' => out.push(0x7f),
                    ' ' => out.push(0), // Ctrl-Space is NUL
                    _ => out.extend(c.to_string().into_bytes()),
                }
            } else {
                out.extend(c.to_string().into_bytes());
            }
            out
        }
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Backspace => {
            if alt {
                vec![0x1b, 0x7f]
            } else {
                vec![0x7f]
            }
        }
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => csi_letter('A', m, app_cursor),
        KeyCode::Down => csi_letter('B', m, app_cursor),
        KeyCode::Right => csi_letter('C', m, app_cursor),
        KeyCode::Left => csi_letter('D', m, app_cursor),
        KeyCode::Home => csi_letter('H', m, app_cursor),
        KeyCode::End => csi_letter('F', m, app_cursor),
        KeyCode::Insert => csi_tilde(2, m),
        KeyCode::Delete => csi_tilde(3, m),
        KeyCode::PageUp => csi_tilde(5, m),
        KeyCode::PageDown => csi_tilde(6, m),
        KeyCode::F(n) => match n {
            // F1-F4 are SS3 when unmodified, CSI 1;mod P..S otherwise.
            1..=4 => {
                let final_byte = (b'P' + (n - 1)) as char;
                let p = modifier_param(m);
                if p == 1 {
                    format!("\x1bO{final_byte}").into_bytes()
                } else {
                    format!("\x1b[1;{p}{final_byte}").into_bytes()
                }
            }
            5 => csi_tilde(15, m),
            6..=10 => csi_tilde(17 + (n - 6), m),
            11 => csi_tilde(23, m),
            12 => csi_tilde(24, m),
            _ => return None,
        },
        _ => return None,
    };
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn with(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    #[test]
    fn plain_characters_and_control_codes() {
        assert_eq!(encode(key(KeyCode::Char('a')), false).unwrap(), b"a");
        assert_eq!(
            encode(with(KeyCode::Char('c'), KeyModifiers::CONTROL), false).unwrap(),
            vec![0x03]
        );
        assert_eq!(
            encode(with(KeyCode::Char(' '), KeyModifiers::CONTROL), false).unwrap(),
            vec![0x00]
        );
    }

    #[test]
    fn alt_is_an_escape_prefix() {
        assert_eq!(encode(with(KeyCode::Char('f'), KeyModifiers::ALT), false).unwrap(), b"\x1bf");
        // Alt-b / Alt-f are word motion in readline; without this they are
        // silently dropped.
        assert_eq!(encode(with(KeyCode::Char('b'), KeyModifiers::ALT), false).unwrap(), b"\x1bb");
    }

    #[test]
    fn application_cursor_mode_changes_the_arrows() {
        // The reason arrows misbehave in vim without DECCKM support.
        assert_eq!(encode(key(KeyCode::Up), false).unwrap(), b"\x1b[A");
        assert_eq!(encode(key(KeyCode::Up), true).unwrap(), b"\x1bOA");
        assert_eq!(encode(key(KeyCode::Home), true).unwrap(), b"\x1bOH");
    }

    #[test]
    fn modified_arrows_use_the_xterm_parameter() {
        // Ctrl-Right is word-forward in readline: CSI 1 ; 5 C.
        assert_eq!(
            encode(with(KeyCode::Right, KeyModifiers::CONTROL), false).unwrap(),
            b"\x1b[1;5C"
        );
        assert_eq!(
            encode(with(KeyCode::Left, KeyModifiers::SHIFT), false).unwrap(),
            b"\x1b[1;2D"
        );
        // A modified arrow ignores application mode, as xterm does.
        assert_eq!(
            encode(with(KeyCode::Up, KeyModifiers::CONTROL), true).unwrap(),
            b"\x1b[1;5A"
        );
    }

    #[test]
    fn function_keys() {
        assert_eq!(encode(key(KeyCode::F(1)), false).unwrap(), b"\x1bOP");
        assert_eq!(encode(key(KeyCode::F(5)), false).unwrap(), b"\x1b[15~");
        assert_eq!(encode(key(KeyCode::F(12)), false).unwrap(), b"\x1b[24~");
        assert_eq!(
            encode(with(KeyCode::F(1), KeyModifiers::SHIFT), false).unwrap(),
            b"\x1b[1;2P"
        );
        assert!(encode(key(KeyCode::F(25)), false).is_none());
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(encode(key(KeyCode::Delete), false).unwrap(), b"\x1b[3~");
        assert_eq!(encode(key(KeyCode::PageUp), false).unwrap(), b"\x1b[5~");
        assert_eq!(encode(key(KeyCode::BackTab), false).unwrap(), b"\x1b[Z");
        assert_eq!(encode(key(KeyCode::Backspace), false).unwrap(), vec![0x7f]);
    }

    /// Windows reports press AND release. Encoding both doubles every
    /// keystroke, which is the classic "why is everything typed twice" bug.
    #[test]
    fn key_releases_are_not_encoded() {
        let mut k = KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert!(encode(k, false).is_none());
    }

    #[test]
    fn unicode_characters_survive() {
        assert_eq!(encode(key(KeyCode::Char('é')), false).unwrap(), "é".as_bytes());
    }
}
