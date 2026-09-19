//! Terminal state: vt100 parser plus the terminal-query responder.
//!
//! vt100 handles the grid, scrollback and alt-screen. It does NOT generate
//! replies to device queries, so we do that here (DESIGN.md §0.3, §2.5).

use std::sync::{Arc, Mutex};

use vt100::Parser;

/// Scans for terminal queries and produces the replies the pane expects.
///
/// Keeps a small rolling tail because a query can straddle a read boundary -
/// a real hazard found while running Probe B, not a theoretical one.
pub struct QueryResponder {
    tail: Vec<u8>,
}

const MAX_TAIL: usize = 16;

impl Default for QueryResponder {
    fn default() -> Self {
        Self { tail: Vec::with_capacity(MAX_TAIL) }
    }
}

impl QueryResponder {
    /// `cursor` is (row, col), 0-based, as vt100 reports it.
    pub fn scan(&mut self, chunk: &[u8], cursor: (u16, u16)) -> Vec<u8> {
        let mut hay = std::mem::take(&mut self.tail);
        hay.extend_from_slice(chunk);

        let mut out = Vec::new();
        let mut i = 0;
        while i < hay.len() {
            if hay[i] != 0x1b {
                i += 1;
                continue;
            }
            match classify(&hay[i..]) {
                Query::Incomplete => break, // keep the rest as tail
                Query::None(len) => i += len,
                Query::Found(len, kind) => {
                    match kind {
                        QueryKind::CursorPosition => {
                            // CPR is 1-based.
                            out.extend_from_slice(
                                format!("\x1b[{};{}R", cursor.0 + 1, cursor.1 + 1).as_bytes(),
                            );
                        }
                        // VT102 with no options - deliberately modest.
                        QueryKind::PrimaryDa => out.extend_from_slice(b"\x1b[?6c"),
                        QueryKind::SecondaryDa => out.extend_from_slice(b"\x1b[>0;0;0c"),
                        QueryKind::DeviceStatus => out.extend_from_slice(b"\x1b[0n"),
                    }
                    i += len;
                }
            }
        }

        // Preserve a possible partial escape for the next chunk.
        let keep = hay.len().saturating_sub(i).min(MAX_TAIL);
        self.tail = hay[hay.len() - keep..].to_vec();
        out
    }
}

enum QueryKind {
    CursorPosition,
    PrimaryDa,
    SecondaryDa,
    DeviceStatus,
}

enum Query {
    /// A complete sequence that is a query: (length, kind).
    Found(usize, QueryKind),
    /// A complete sequence that is not a query: (length).
    None(usize),
    /// Truncated - wait for more bytes.
    Incomplete,
}

fn classify(s: &[u8]) -> Query {
    if s.len() < 2 {
        return Query::Incomplete;
    }
    if s[1] != b'[' {
        return Query::None(1);
    }
    let mut j = 2;
    while j < s.len() && (s[j].is_ascii_digit() || s[j] == b';' || s[j] == b'?' || s[j] == b'>') {
        j += 1;
    }
    if j >= s.len() {
        return Query::Incomplete;
    }
    let params = &s[2..j];
    let final_byte = s[j];
    let len = j + 1;
    match final_byte {
        b'n' if params == b"6" => Query::Found(len, QueryKind::CursorPosition),
        b'n' if params == b"5" => Query::Found(len, QueryKind::DeviceStatus),
        b'c' if params.is_empty() || params == b"0" => Query::Found(len, QueryKind::PrimaryDa),
        b'c' if params == b">" || params == b">0" => Query::Found(len, QueryKind::SecondaryDa),
        _ => Query::None(len),
    }
}

/// Side-channel facts vt100 surfaces through callbacks rather than the grid.
#[derive(Default)]
pub struct Signals {
    /// Latest OSC 7 working directory, as the shell reported it (MSYS form).
    pub cwd: Option<String>,
    /// Latest OSC 0/2 window title.
    pub title: Option<String>,
    /// Bell since last checked.
    pub bell: bool,
}

/// vt100 callback sink. OSC 7 is not a sequence vt100 handles itself, so it
/// arrives via `unhandled_osc` - which is how we get cwd at all on Windows
/// (DESIGN.md §4).
pub struct Hooks {
    pub signals: Arc<Mutex<Signals>>,
}

impl vt100::Callbacks for Hooks {
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        // OSC 7 ; file://host/path
        if params.len() >= 2 && params[0] == b"7" {
            if let Ok(payload) = std::str::from_utf8(params[1]) {
                if let Some(path) = crate::cwd::parse_osc7(payload) {
                    if let Ok(mut s) = self.signals.lock() {
                        s.cwd = Some(path);
                    }
                }
            }
        }
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        if let Ok(t) = std::str::from_utf8(title) {
            if let Ok(mut s) = self.signals.lock() {
                s.title = Some(t.to_string());
            }
        }
    }

    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        if let Ok(mut s) = self.signals.lock() {
            s.bell = true;
        }
    }
}

pub struct Term {
    pub parser: Parser<Hooks>,
    pub signals: Arc<Mutex<Signals>>,
    pub responder: QueryResponder,
    /// Rows scrolled back from the live screen. 0 = following output.
    scroll: usize,
}

impl Term {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let signals = Arc::new(Mutex::new(Signals::default()));
        Self {
            parser: Parser::new_with_callbacks(
                rows,
                cols,
                scrollback,
                Hooks { signals: Arc::clone(&signals) },
            ),
            signals,
            responder: QueryResponder::default(),
            scroll: 0,
        }
    }

    /// Latest reported working directory, converted to a Windows path.
    pub fn cwd_windows(&self) -> Option<String> {
        let s = self.signals.lock().ok()?;
        s.cwd.as_deref().map(crate::cwd::msys_to_windows)
    }

    pub fn scroll_offset(&self) -> usize {
        self.scroll
    }

    /// Scroll back by `lines`, clamped by vt100 to the real history length.
    pub fn scroll_up(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_add(lines);
        self.apply_scroll();
    }

    pub fn scroll_down(&mut self, lines: usize) {
        self.scroll = self.scroll.saturating_sub(lines);
        self.apply_scroll();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll = 0;
        self.apply_scroll();
    }

    fn apply_scroll(&mut self) {
        self.parser.screen_mut().set_scrollback(self.scroll);
        // vt100 clamps to the available history, so read back what it accepted
        // rather than trusting our own counter.
        self.scroll = self.parser.screen().scrollback();
    }

    /// New output should snap the view back to the live screen, exactly as a
    /// terminal does - otherwise typing while scrolled back looks frozen.
    pub fn follow_output(&mut self) {
        if self.scroll != 0 {
            self.scroll_to_bottom();
        }
    }

    pub fn cursor(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_cursor_position_request() {
        let mut r = QueryResponder::default();
        assert_eq!(r.scan(b"\x1b[6n", (4, 9)), b"\x1b[5;10R".to_vec());
    }

    #[test]
    fn handles_query_split_across_reads() {
        let mut r = QueryResponder::default();
        assert!(r.scan(b"hello\x1b[", (0, 0)).is_empty());
        assert_eq!(r.scan(b"6n", (0, 0)), b"\x1b[1;1R".to_vec());
    }

    #[test]
    fn ignores_non_query_sequences() {
        let mut r = QueryResponder::default();
        assert!(r.scan(b"\x1b[2J\x1b[1;1H\x1b[?25l", (0, 0)).is_empty());
    }

    #[test]
    fn answers_primary_device_attributes() {
        let mut r = QueryResponder::default();
        assert_eq!(r.scan(b"\x1b[c", (0, 0)), b"\x1b[?6c".to_vec());
    }
}
