//! What the host terminal typed, decoded from the bytes it sent.
//!
//! The client reads its console in VT input mode (`console.rs`), so input
//! arrives the way a terminal sends it rather than as Win32 key records. That
//! is the whole reason for the switch: a key record has no way to say "this
//! was pasted", so under crossterm a paste arrived as one keystroke per
//! character - streamed into the pane letter by letter, every newline a
//! submit, and never the single block a program like an agent's prompt box
//! turns into `[Pasted N lines]`. In VT mode the host brackets a paste in
//! `ESC[200~ … ESC[201~` once asked to (DEC 2004), and this module finds it.
//! herdr does the same (`raw_input.rs`); this is the subset zmux needs.
//!
//! The decoder only FRAMES: it cuts the stream into keystrokes, pastes and
//! mouse reports. A keystroke keeps the exact bytes the terminal sent, which
//! are already the xterm encoding the pane's program expects, so the client
//! forwards them rather than re-encoding. `keys.rs` interprets the few the
//! client acts on itself.

use crate::protocol::{MouseIn, MouseKind, MOUSE_ALT, MOUSE_CTRL, MOUSE_SHIFT};

const ESC: u8 = 0x1b;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Longest a CSI may run before it is given up on as not a sequence at all.
/// Real ones are a dozen bytes; this only bounds a malformed one.
const MAX_CSI: usize = 64;

/// How much an unterminated paste may hold before it is delivered anyway.
///
/// A paste ends only at its end marker, and a timeout must not cut one short:
/// a large paste can arrive over many reads. But a start marker whose end
/// never comes would otherwise swallow every later keystroke, so past this
/// the text so far goes out as a paste and decoding carries on.
const MAX_PENDING_PASTE: usize = 2 * crate::protocol::MAX_PASTE_BYTES;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Token {
    /// One keystroke, exactly as the terminal sent it: a character, a control
    /// byte, or a complete escape sequence.
    Key(Vec<u8>),
    /// The text between a paste's markers, markers removed.
    Paste(Vec<u8>),
    /// An SGR mouse report, in 0-based screen coordinates.
    Mouse(MouseIn),
}

/// Cuts a byte stream into tokens, holding back an unfinished one until the
/// rest of it arrives.
#[derive(Debug, Default)]
pub struct Decoder {
    buf: Vec<u8>,
}

impl Decoder {
    /// Add what was just read, and take every token it completes.
    pub fn push(&mut self, data: &[u8]) -> Vec<Token> {
        self.buf.extend_from_slice(data);
        self.drain(false)
    }

    /// Nothing more has arrived for a while: what is held back is not the
    /// start of something longer after all. A lone ESC is the Escape key.
    ///
    /// An unfinished PASTE is kept - see `waiting_for_paste`.
    pub fn flush(&mut self) -> Vec<Token> {
        self.drain(true)
    }

    /// True when bytes are held back waiting for more.
    pub fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// True when what is held back is a paste still waiting for its end.
    ///
    /// The caller must not time this out: a paste ends at its marker, however
    /// long the host takes to deliver the text before it. herdr pins the same
    /// rule with a test (`incomplete_bracketed_paste_is_not_flushed_on_timeout`).
    pub fn waiting_for_paste(&self) -> bool {
        self.buf.starts_with(PASTE_START)
    }

    fn drain(&mut self, timed_out: bool) -> Vec<Token> {
        let mut out = Vec::new();
        let mut at = 0;
        while at < self.buf.len() {
            let Some((token, used)) = next(&self.buf[at..], timed_out) else { break };
            out.extend(token);
            at += used;
        }
        self.buf.drain(..at);
        out
    }
}

/// The first token in `buf` and how many bytes it used, or `None` when `buf`
/// holds only the beginning of one. `timed_out` turns "wait for more" into
/// "take what is there".
fn next(buf: &[u8], timed_out: bool) -> Option<(Option<Token>, usize)> {
    let whole = |n: usize| Some((Some(Token::Key(buf[..n].to_vec())), n));
    let partial = || timed_out.then(|| (Some(Token::Key(buf.to_vec())), buf.len()));

    if buf[0] != ESC {
        let n = utf8_len(buf[0]);
        return if buf.len() >= n { whole(n) } else { partial() };
    }
    if let Some(body) = buf.strip_prefix(PASTE_START) {
        return match find(body, PASTE_END) {
            Some(end) => Some((
                Some(Token::Paste(body[..end].to_vec())),
                PASTE_START.len() + end + PASTE_END.len(),
            )),
            None if buf.len() > MAX_PENDING_PASTE => {
                Some((Some(Token::Paste(body.to_vec())), buf.len()))
            }
            None => None,
        };
    }
    let Some(&second) = buf.get(1) else { return partial() };
    match second {
        b'[' => csi(buf, timed_out),
        // SS3: F1-F4, and the arrows when the host is in application mode.
        b'O' => {
            if buf.len() >= 3 { whole(3) } else { partial() }
        }
        // ESC ESC is Escape, then whatever the second one starts.
        ESC => whole(1),
        // Alt is an ESC in front of the character it modifies.
        _ => {
            let n = 1 + utf8_len(second);
            if buf.len() >= n { whole(n) } else { partial() }
        }
    }
}

/// `ESC [` parameters, intermediates, one final byte.
fn csi(buf: &[u8], timed_out: bool) -> Option<(Option<Token>, usize)> {
    for (i, &b) in buf.iter().enumerate().skip(2) {
        match b {
            0x20..=0x3f => {
                if i + 1 >= MAX_CSI {
                    return Some((Some(Token::Key(buf[..=i].to_vec())), i + 1));
                }
            }
            0x40..=0x7e => {
                let seq = &buf[..=i];
                if buf.get(2) == Some(&b'<') && matches!(b, b'M' | b'm') {
                    // A report nothing can use - horizontal wheel, say - is
                    // consumed and dropped rather than typed into the pane.
                    return Some((sgr_mouse(&seq[3..seq.len() - 1], b == b'm').map(Token::Mouse), i + 1));
                }
                return Some((Some(Token::Key(seq.to_vec())), i + 1));
            }
            // Not a CSI after all. Hand over what came before the stray byte
            // and let it start the next token.
            _ => return Some((Some(Token::Key(buf[..i].to_vec())), i)),
        }
    }
    timed_out.then(|| (Some(Token::Key(buf.to_vec())), buf.len()))
}

/// `Cb ; Cx ; Cy`, 1-based, from `ESC [ < … M` (press, motion) or `… m`
/// (release). xterm's button byte: low two bits the button, +4 shift, +8
/// alt, +16 ctrl, +32 motion, +64 wheel.
fn sgr_mouse(params: &[u8], release: bool) -> Option<MouseIn> {
    let text = std::str::from_utf8(params).ok()?;
    let mut fields = text.split(';').map(|f| f.parse::<u16>().ok());
    let (cb, x, y) = (fields.next()??, fields.next()??, fields.next()??);
    if fields.next().is_some() {
        return None;
    }
    let mut mods = 0;
    if cb & 4 != 0 {
        mods |= MOUSE_SHIFT;
    }
    if cb & 8 != 0 {
        mods |= MOUSE_ALT;
    }
    if cb & 16 != 0 {
        mods |= MOUSE_CTRL;
    }
    let button = (cb & 3) as u8;
    let kind = if cb & 64 != 0 {
        match button {
            0 => MouseKind::WheelUp,
            1 => MouseKind::WheelDown,
            // Horizontal: no xterm button number, nothing to forward.
            _ => return None,
        }
    } else if release {
        MouseKind::Release(button)
    } else if cb & 32 != 0 {
        if button == 3 { MouseKind::Move } else { MouseKind::Drag(button) }
    } else if button == 3 {
        return None;
    } else {
        MouseKind::Press(button)
    };
    Some(MouseIn { x: x.saturating_sub(1), y: y.saturating_sub(1), kind, mods })
}

/// Length of the UTF-8 character `first` starts. An invalid lead byte is a
/// token of its own rather than a reason to wait forever.
fn utf8_len(first: u8) -> usize {
    match first {
        0xc2..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf4 => 4,
        _ => 1,
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: &[u8]) -> Token {
        Token::Key(b.to_vec())
    }

    fn all(data: &[u8]) -> Vec<Token> {
        let mut d = Decoder::default();
        let mut out = d.push(data);
        out.extend(d.flush());
        out
    }

    /// The bug: a paste has to come out as ONE token, not as a keystroke per
    /// character - that is what lets the pane's program see a paste at all.
    #[test]
    fn a_paste_is_one_token() {
        assert_eq!(
            all(b"\x1b[200~line one\rline two\x1b[201~"),
            vec![Token::Paste(b"line one\rline two".to_vec())]
        );
    }

    /// A large paste arrives over many reads, and the pieces fall wherever
    /// the host's buffer happened to end - including inside the end marker.
    #[test]
    fn a_paste_split_across_reads_is_still_one_token() {
        let mut d = Decoder::default();
        assert!(d.push(b"\x1b[20").is_empty());
        assert!(d.push(b"0~first half, ").is_empty());
        assert!(d.push(b"second half\x1b[2").is_empty());
        assert_eq!(d.push(b"01~x"), vec![Token::Paste(b"first half, second half".to_vec()), key(b"x")]);
        assert!(!d.pending());
    }

    /// A timeout means "the lone ESC was the Escape key". It must not mean
    /// "the paste is over": a slow host would split one paste into two, and
    /// the tail would arrive with no start marker, as typing.
    #[test]
    fn a_timeout_does_not_cut_a_paste_short() {
        let mut d = Decoder::default();
        d.push(b"\x1b[200~still arriving");
        assert!(d.waiting_for_paste());
        assert!(d.flush().is_empty(), "a timeout ended the paste");
        assert_eq!(d.push(b"\x1b[201~"), vec![Token::Paste(b"still arriving".to_vec())]);
    }

    /// Past its bound an unterminated paste is delivered rather than left to
    /// swallow every later keystroke.
    #[test]
    fn a_paste_whose_end_never_comes_does_not_eat_the_keyboard() {
        let mut d = Decoder::default();
        let mut data = PASTE_START.to_vec();
        data.resize(MAX_PENDING_PASTE + 1, b'a');
        let out = d.push(&data);
        assert!(matches!(out.as_slice(), [Token::Paste(_)]), "{:?}", out.len());
        assert_eq!(d.push(b"k"), vec![key(b"k")]);
    }

    #[test]
    fn keys_around_a_paste_keep_their_order() {
        assert_eq!(
            all(b"a\x1b[200~p\x1b[201~\r"),
            vec![key(b"a"), Token::Paste(b"p".to_vec()), key(b"\r")]
        );
    }

    #[test]
    fn an_empty_paste_is_still_a_paste() {
        assert_eq!(all(b"\x1b[200~\x1b[201~"), vec![Token::Paste(Vec::new())]);
    }

    /// Escape and the start of a sequence are the same byte. Only waiting
    /// tells them apart, so a lone ESC is held until the caller times out.
    #[test]
    fn a_lone_escape_waits_for_a_timeout() {
        let mut d = Decoder::default();
        assert!(d.push(b"\x1b").is_empty());
        assert!(d.pending() && !d.waiting_for_paste());
        assert_eq!(d.flush(), vec![key(b"\x1b")]);
        assert!(!d.pending());
    }

    #[test]
    fn escape_sequences_are_one_keystroke_each() {
        assert_eq!(
            all(b"\x1b[A\x1b[1;5C\x1b[5~\x1bOP"),
            vec![key(b"\x1b[A"), key(b"\x1b[1;5C"), key(b"\x1b[5~"), key(b"\x1bOP")]
        );
    }

    #[test]
    fn a_sequence_split_across_reads_is_reassembled() {
        let mut d = Decoder::default();
        assert!(d.push(b"\x1b[1;").is_empty());
        assert_eq!(d.push(b"5A"), vec![key(b"\x1b[1;5A")]);
    }

    #[test]
    fn alt_is_an_escape_in_front_of_a_character() {
        assert_eq!(all(b"\x1bb\x1b\xc3\xa9"), vec![key(b"\x1bb"), key("\x1bé".as_bytes())]);
    }

    #[test]
    fn two_escapes_are_escape_then_whatever_follows() {
        assert_eq!(all(b"\x1b\x1b[A"), vec![key(b"\x1b"), key(b"\x1b[A")]);
    }

    /// A character is never split, even when a read ends in the middle of it.
    #[test]
    fn a_multibyte_character_waits_for_its_last_byte() {
        let mut d = Decoder::default();
        assert!(d.push(b"\xe2\x82").is_empty());
        assert_eq!(d.push(b"\xac"), vec![key("€".as_bytes())]);
    }

    #[test]
    fn control_bytes_are_keystrokes_of_their_own() {
        assert_eq!(all(b"\x02%"), vec![key(b"\x02"), key(b"%")]);
    }

    /// Malformed input costs the bytes that were wrong, never the ones after.
    #[test]
    fn a_broken_sequence_does_not_swallow_what_follows() {
        assert_eq!(all(b"\x1b[1\x01x"), vec![key(b"\x1b[1"), key(b"\x01"), key(b"x")]);
        let mut long = b"\x1b[".to_vec();
        long.resize(MAX_CSI + 8, b'1');
        let out = all(&long);
        assert!(out.len() >= 2, "an overlong CSI was held forever");
    }

    #[test]
    fn sgr_mouse_reports_become_mouse_events() {
        let m = |kind, x, y, mods| Token::Mouse(MouseIn { x, y, kind, mods });
        assert_eq!(all(b"\x1b[<0;10;5M"), vec![m(MouseKind::Press(0), 9, 4, 0)]);
        assert_eq!(all(b"\x1b[<2;1;1m"), vec![m(MouseKind::Release(2), 0, 0, 0)]);
        assert_eq!(all(b"\x1b[<32;3;4M"), vec![m(MouseKind::Drag(0), 2, 3, 0)]);
        assert_eq!(all(b"\x1b[<35;3;4M"), vec![m(MouseKind::Move, 2, 3, 0)]);
        assert_eq!(all(b"\x1b[<64;1;1M"), vec![m(MouseKind::WheelUp, 0, 0, 0)]);
        assert_eq!(all(b"\x1b[<65;1;1M"), vec![m(MouseKind::WheelDown, 0, 0, 0)]);
        assert_eq!(
            all(b"\x1b[<20;1;1M"),
            vec![m(MouseKind::Press(0), 0, 0, MOUSE_SHIFT | MOUSE_CTRL)]
        );
    }

    /// Consumed, not typed: a report nothing can use must not turn up in the
    /// pane as `[<66;1;1M`.
    #[test]
    fn an_unusable_mouse_report_is_dropped_whole() {
        assert_eq!(all(b"\x1b[<66;1;1Mx"), vec![key(b"x")]);
        assert_eq!(all(b"\x1b[<0;1Mx"), vec![key(b"x")]);
    }
}
