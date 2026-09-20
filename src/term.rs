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
    /// How many times this pane has rung the terminal bell.
    ///
    /// A COUNT, not a flag. A flag has to be cleared to be useful twice, and
    /// two readers - the status bar and a control caller - would then race to
    /// clear it and each swallow the other's notification. A count that only
    /// ever goes up lets each of them remember what it last saw.
    ///
    /// Worth exposing because it is the one attention signal that costs
    /// nothing and works for a program that reports nothing about itself:
    /// plenty of agents ring the bell when they stop to ask something.
    pub bells: u32,
    /// Last command line the shell reported (OSC 633;E). Kept after the
    /// command ends, so `busy=false` plus this plus `last_exit` reads as
    /// "that finished, this is how".
    pub command: Option<String>,
    /// Between OSC 133;C and OSC 133;D.
    pub busy: bool,
    /// Status carried by the last OSC 133;D.
    pub last_exit: Option<i32>,
    /// Any OSC 133 has arrived, i.e. our rcfile is live in this pane. Until
    /// then the window title is the only clue about what is running, and it
    /// is a poor one - Git Bash points it at the directory.
    pub integrated: bool,
    /// A name the pane claimed for itself, so a caller can say `reviewer`
    /// instead of tracking an id. Set by the report channel (DESIGN.md §12),
    /// and cleared when the command that claimed it ends.
    pub name: Option<String>,
}

/// What a pane is doing. One lock, every fact the bar and `info` need.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    pub command: Option<String>,
    pub busy: bool,
    pub last_exit: Option<i32>,
    /// The OSC 0/2 title, and ONLY when no shell integration has been seen -
    /// otherwise it is just the directory restated.
    ///
    /// This is the STATUS BAR's rule, and it stays that way. A control caller
    /// wants the title whenever there is one, so it reads `raw_title`.
    pub title: Option<String>,
    /// The OSC 0/2 title as it actually arrived, whatever else is known.
    ///
    /// Many TUIs say what they are doing here, and it is the highest-priority
    /// signal in herdr's Claude rules (`claude.toml`, `region = "osc_title"`).
    /// Handed over as a fact the pane stated, not as a conclusion about it.
    pub raw_title: Option<String>,
    /// See `Signals::bells`.
    pub bells: u32,
}

impl Activity {
    /// One word for the window list, the way tmux names a window after what is
    /// running in it. `None` means "nothing to say, use the shell's name".
    pub fn label(&self) -> Option<String> {
        if self.busy {
            if let Some(c) = self.command.as_deref() {
                // The first word, not the whole line: `cargo`, not
                // `cargo test --all -- --nocapture`.
                if let Some(w) = c.split_whitespace().next() {
                    // `./x` and `/usr/bin/foo` read better as the file name.
                    let w = w.rsplit(['/', '\\']).next().unwrap_or(w);
                    if !w.is_empty() {
                        return Some(w.to_string());
                    }
                }
            }
        }
        self.title.clone()
    }
}

/// Keep a shell-supplied string fit for a status bar, a JSON field and a wire.
///
/// Control bytes would move the cursor or break the line; an unbounded length
/// would ride on every frame.
fn sanitise(s: &str) -> Option<String> {
    let out: String = s
        .chars()
        .filter(|c| !c.is_control())
        .take(crate::protocol::MAX_COMMAND_CHARS)
        .collect();
    let out = out.trim();
    (!out.is_empty()).then(|| out.to_string())
}

/// vt100 callback sink. OSC 7 is not a sequence vt100 handles itself, so it
/// arrives via `unhandled_osc` - which is how we get cwd at all on Windows
/// (DESIGN.md §4).
pub struct Hooks {
    pub signals: Arc<Mutex<Signals>>,
}

impl vt100::Callbacks for Hooks {
    fn unhandled_osc(&mut self, _: &mut vt100::Screen, params: &[&[u8]]) {
        if params.len() < 2 {
            return;
        }
        match params[0] {
            // OSC 7 ; file://host/path
            b"7" => {
                if let Ok(payload) = std::str::from_utf8(params[1]) {
                    if let Some(path) = crate::cwd::parse_osc7(payload) {
                        if let Ok(mut s) = self.signals.lock() {
                            s.cwd = Some(path);
                        }
                    }
                }
            }
            // OSC 133 ; C            a command started
            // OSC 133 ; D ; <status> it finished
            b"133" => {
                let Ok(mut s) = self.signals.lock() else { return };
                s.integrated = true;
                match params[1] {
                    b"C" => s.busy = true,
                    b"D" => {
                        s.busy = false;
                        s.last_exit = params
                            .get(2)
                            .and_then(|p| std::str::from_utf8(p).ok())
                            .and_then(|p| p.trim().parse().ok());
                    }
                    _ => {}
                }
            }
            // OSC 633 ; E ; <command line>
            //
            // The payload is REJOINED on ';': OSC parameters are split on it,
            // so `echo "a;b"` arrives as two of them and taking params[2]
            // alone would silently truncate every command with a semicolon in
            // it. Measured against a real Git Bash, not assumed.
            b"633" if params[1] == b"E" => {
                let joined = params[2..]
                    .iter()
                    .filter_map(|p| std::str::from_utf8(p).ok())
                    .collect::<Vec<_>>()
                    .join(";");
                if let Ok(mut s) = self.signals.lock() {
                    s.command = sanitise(&joined);
                }
            }
            _ => {}
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
            s.bells = s.bells.saturating_add(1);
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

    /// What this pane is doing right now.
    ///
    /// The title is handed back ONLY when no OSC 133 has ever arrived. With
    /// our rcfile live the title adds nothing - Git Bash points it at the
    /// directory, which OSC 7 already reports - but in a pane running
    /// something else (`cmd.exe`, a bare `python`) it is the only signal
    /// there is, and `vim`/`ssh`/`less` all set it.
    pub fn activity(&self) -> Activity {
        let Ok(s) = self.signals.lock() else { return Activity::default() };
        let title = s.title.as_deref().and_then(sanitise);
        Activity {
            command: s.command.clone(),
            busy: s.busy,
            last_exit: s.last_exit,
            title: if s.integrated { None } else { title.clone() },
            raw_title: title,
            bells: s.bells,
        }
    }

    /// The name this pane claimed for itself, if it claimed one.
    pub fn name(&self) -> Option<String> {
        self.signals.lock().ok()?.name.clone()
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
