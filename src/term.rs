//! Terminal state: vt100 parser plus the terminal-query responder.
//!
//! vt100 handles the grid, scrollback and alt-screen. It does NOT generate
//! replies to device queries, so we do that here (DESIGN.md §0.3, §2.5).

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

pub struct Term {
    pub parser: Parser,
    pub responder: QueryResponder,
}

impl Term {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        Self { parser: Parser::new(rows, cols, scrollback), responder: QueryResponder::default() }
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
