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
    /// The last state this pane reported. See `protocol::PaneState`.
    pub state: crate::protocol::PaneState,
    /// What the pane says is running in it.
    pub agent: Option<String>,
    /// Highest `seq` accepted so far, or `None` when none has been.
    ///
    /// An `Option` rather than a counter starting at zero, because the rule is
    /// "at or below the last one ACCEPTED" and before the first report there
    /// is no last one. A plain `u64` made zero behave as though it had already
    /// been accepted, so an agent numbering its reports from 0 lost its first
    /// one - and lost one again after every command boundary, since
    /// `forget_reports` puts the counter back.
    pub report_seq: Option<u64>,
}

impl Signals {
    /// Take a report, or refuse it. True when anything changed.
    ///
    /// Out-of-order reports are DROPPED rather than merged. Two hooks firing
    /// close together can arrive backwards, and taking the later-arriving one
    /// would leave a pane reading `working` after it had already gone idle -
    /// which is the exact lie a caller waiting on that pane cannot survive.
    pub fn record(&mut self, r: &crate::protocol::Report) -> bool {
        if let Some(seq) = r.seq {
            if self.report_seq.is_some_and(|last| seq <= last) {
                return false;
            }
            self.report_seq = Some(seq);
        }
        let mut changed = false;
        if let Some(state) = r.state {
            changed |= self.state != state;
            self.state = state;
        }
        if let Some(agent) = r.agent.as_deref().and_then(sanitise) {
            changed |= self.agent.as_deref() != Some(agent.as_str());
            self.agent = Some(agent);
        }
        // Checked here as well as at every parse, because this is the one
        // place both the OSC and the control message go through.
        if let Some(name) = r.name.as_deref().filter(|n| crate::protocol::is_valid_name(n)) {
            changed |= self.name.as_deref() != Some(name);
            self.name = Some(name.to_string());
        }
        changed
    }

    /// Forget everything the last program said about itself.
    ///
    /// Called when a foreground command starts or ends, which is what makes
    /// staleness exact rather than a guess with a timeout attached: OSC 133
    /// brackets the command, so every report an agent makes arrives strictly
    /// between the two, and when the second one lands the agent is gone.
    fn forget_reports(&mut self) {
        self.state = crate::protocol::PaneState::Unknown;
        self.agent = None;
        self.name = None;
        // So an agent that restarts and counts from the beginning is heard,
        // whichever number it starts from.
        self.report_seq = None;
    }
}

/// What a pane is doing. One lock, every fact the bar and `info` need.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Activity {
    pub command: Option<String>,
    pub busy: bool,
    pub last_exit: Option<i32>,
    /// What the pane last said it was doing.
    pub state: crate::protocol::PaneState,
    /// What the pane says is running in it.
    pub agent: Option<String>,
    /// The name the pane claimed.
    pub name: Option<String>,
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

/// Parse `key=value` parameters into a report.
///
/// `None` when nothing usable was found, so a malformed sequence leaves the
/// pane's state alone instead of blanking it. A value that is not understood
/// (a misspelt state, a name with a capital in it) drops THAT key, for the
/// same reason: a typo in somebody's hook must not erase a state that was
/// true a moment ago.
///
/// Values may not contain `;`, because OSC parameters are split on it. States
/// and names could not anyway, and an agent's name is a program name.
fn parse_report(params: &[&[u8]]) -> Option<crate::protocol::Report> {
    let mut r = crate::protocol::Report::default();
    let mut any = false;
    for p in params {
        let Ok(text) = std::str::from_utf8(p) else { continue };
        let Some((key, value)) = text.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "state" => {
                r.state = crate::protocol::PaneState::parse(value);
                any |= r.state.is_some();
            }
            "agent" => {
                r.agent = sanitise(value);
                any |= r.agent.is_some();
            }
            "name" => {
                r.name = crate::protocol::is_valid_name(value).then(|| value.to_string());
                any |= r.name.is_some();
            }
            "seq" => r.seq = value.parse().ok(),
            _ => {}
        }
    }
    any.then_some(r)
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
                    b"C" => {
                        s.busy = true;
                        // A new foreground command: whatever the last one
                        // said about itself is now about a dead process.
                        s.forget_reports();
                    }
                    b"D" => {
                        s.busy = false;
                        s.forget_reports();
                        s.last_exit = params
                            .get(2)
                            .and_then(|p| std::str::from_utf8(p).ok())
                            .and_then(|p| p.trim().parse().ok());
                    }
                    _ => {}
                }
            }
            // OSC 1341 ; zmux ; key=value ; ...
            //
            // How a program in a pane says what it is doing (DESIGN.md §12).
            // An OSC rather than a command, because it costs no process, does
            // not need this binary on PATH or the session's name, and any
            // terminal that does not understand it swallows it silently.
            //
            // The literal `zmux` is a guard: the OSC number is arbitrary
            // and unregistered, so this makes a collision with somebody
            // else's 1341 harmless instead of a mis-parse.
            b"1341" if params[1] == b"zmux" => {
                let Some(report) = parse_report(&params[2..]) else { return };
                if let Ok(mut s) = self.signals.lock() {
                    s.record(&report);
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
            state: s.state,
            agent: s.agent.clone(),
            name: s.name.clone(),
            title: if s.integrated { None } else { title.clone() },
            raw_title: title,
            bells: s.bells,
        }
    }

    /// The name this pane claimed for itself, if it claimed one.
    pub fn name(&self) -> Option<String> {
        self.signals.lock().ok()?.name.clone()
    }

    /// Take a report that arrived some way other than the pane's own pty.
    pub fn record(&self, r: &crate::protocol::Report) -> bool {
        self.signals.lock().is_ok_and(|mut s| s.record(r))
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

    /// The last `lines` rows this pane produced, INCLUDING scrollback.
    ///
    /// Reading history means moving vt100's scrollback offset, which is the
    /// pane's own user-visible state - so it is put back before the lock is
    /// released. Every other reader of this screen takes the same mutex, so
    /// nobody can observe the offset mid-read, and the user's scroll position
    /// is exactly where they left it. That is why `capture` could only ever
    /// see the visible rows before: without one lock covering both, borrowing
    /// the scroll position makes somebody's terminal jump.
    ///
    /// `unwrap` joins a row to the next when the first only ended because it
    /// ran out of width. A transcript read at 60 columns is otherwise full of
    /// breaks that are an artefact of the pane's size, not of the text.
    pub fn read_recent(&mut self, lines: usize, unwrap: bool) -> String {
        let saved = self.scroll;
        let (rows, cols) = self.parser.screen().size();
        let rows = rows.max(1) as usize;
        let want = lines.clamp(1, crate::protocol::MAX_CAPTURE_LINES);

        // How far back the history actually goes. `set_scrollback` clamps, so
        // asking for more than exists and reading the answer back is the only
        // way to find out - and much better than trusting a count of our own.
        self.parser.screen_mut().set_scrollback(usize::MAX);
        let oldest = self.parser.screen().scrollback();

        // Offsets from the oldest window we need down to the live screen. An
        // offset of k shows the `rows` rows ending k rows above the bottom,
        // so stepping down by `rows` moves a whole screenful - except the
        // last step, which is short whenever `want` is not a multiple.
        let mut offsets = vec![oldest.min(want.saturating_sub(rows))];
        while *offsets.last().expect("seeded above") > 0 {
            offsets.push(offsets.last().expect("seeded above").saturating_sub(rows));
        }

        let mut out: Vec<(String, bool)> = Vec::new();
        let mut previous: Option<usize> = None;
        for k in offsets {
            self.parser.screen_mut().set_scrollback(k);
            let screen = self.parser.screen();
            let window: Vec<(String, bool)> = screen
                .rows(0, cols)
                .enumerate()
                .map(|(i, text)| (text, screen.row_wrapped(i as u16)))
                .collect();
            // Moving down by d rows reveals exactly d new rows at the bottom.
            // Taking the whole window each time would repeat the rest.
            let fresh = match previous {
                None => window.len(),
                Some(p) => p.saturating_sub(k).min(window.len()),
            };
            // Keep the LAST `fresh` rows of this window; the ones above them
            // are the window before it, already collected.
            let skip = window.len() - fresh;
            out.extend(window.into_iter().skip(skip));
            previous = Some(k);
        }

        // Restore through the same path scrolling uses, so `scroll` and
        // vt100 cannot end up disagreeing about where the user is.
        self.scroll = saved;
        self.apply_scroll();

        // Trailing blanks are the unused bottom of the live screen, not
        // output. Without this every read ends in a wall of empty lines.
        while out.last().is_some_and(|(t, _)| t.trim().is_empty()) {
            out.pop();
        }
        let keep = out.len().saturating_sub(want);
        let out = &out[keep..];

        if !unwrap {
            return out.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join("\n");
        }
        let mut joined = String::new();
        for (i, (text, wrapped)) in out.iter().enumerate() {
            joined.push_str(text);
            // A wrapped row continues into the next, so the break between
            // them is the pane's width talking, not the text.
            if i + 1 < out.len() && !wrapped {
                joined.push('\n');
            }
        }
        joined
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

    /// The other two queries ConPTY and full-screen programs ask.
    ///
    /// Every one of these must be answered or the asking program waits for a
    /// reply that never comes - which for ConPTY's startup `ESC[6n` means the
    /// shell never finishes initialising and teardown deadlocks (DESIGN §2.5).
    /// Only CPR and primary DA had tests, so two of the four arms this module
    /// exists for were never exercised.
    #[test]
    fn answers_secondary_device_attributes_and_device_status() {
        let mut r = QueryResponder::default();
        // Secondary DA, both spellings `classify` accepts.
        assert_eq!(r.scan(b"\x1b[>c", (0, 0)), b"\x1b[>0;0;0c".to_vec());
        assert_eq!(r.scan(b"\x1b[>0c", (0, 0)), b"\x1b[>0;0;0c".to_vec());
        // DSR: "are you there?" - answered with "OK".
        assert_eq!(r.scan(b"\x1b[5n", (0, 0)), b"\x1b[0n".to_vec());
    }

    /// Several queries in one read are all answered, in order, and the text
    /// around them is not mistaken for one.
    ///
    /// A program that asks twice and is answered once waits forever on the
    /// second, which looks exactly like the hang an unanswered query causes.
    #[test]
    fn every_query_in_a_chunk_is_answered_in_order() {
        let mut r = QueryResponder::default();
        let out = r.scan(b"before\x1b[6nmiddle\x1b[c\x1b[5nafter", (2, 3));
        assert_eq!(out, b"\x1b[3;4R\x1b[?6c\x1b[0n".to_vec());
    }

    /// A partial escape longer than the rolling tail is dropped, not answered
    /// wrongly and not left to wedge the scanner.
    ///
    /// `MAX_TAIL` is 16 bytes, which is ample for every query above - the
    /// longest is four. A sequence that straddles a read boundary with more
    /// parameters than that loses its `ESC` when the tail is trimmed, so it
    /// stops looking like a query at all. That is the safe direction: the
    /// parser has already been fed the raw bytes separately, and answering a
    /// sequence we only half saw would be worse than answering nothing.
    #[test]
    fn an_over_long_partial_escape_does_not_confuse_the_scanner() {
        let mut r = QueryResponder::default();
        assert!(r.scan(b"\x1b[1;2;3;4;5;6;7;8;9;10;11;12", (0, 0)).is_empty());
        // Whatever it kept, a REAL query in the next chunk still gets through.
        assert_eq!(r.scan(b"m\x1b[6n", (0, 0)), b"\x1b[1;1R".to_vec());
    }

    /// A history read sees back exactly as far as it was told to, and no
    /// further.
    ///
    /// This is the bound `Until::Output` inherits, so it is a fact about what
    /// a wait can notice rather than a detail of `capture`. The comment on
    /// the wait used to claim a match could not scroll past between two
    /// polls; it can, and the only honest thing to do is say how far.
    #[test]
    fn a_history_read_sees_back_as_far_as_it_was_asked_and_no_further() {
        let mut t = Term::new(80, 24, 10_000);
        t.parser.process(b"MARK_EARLY\r\n");
        for i in 0..1_000 {
            t.parser.process(format!("filler {i}\r\n").as_bytes());
        }
        assert!(
            !t.read_recent(crate::protocol::RECENT_LINES_DEFAULT, true).contains("MARK_EARLY"),
            "a bounded read returned something a thousand lines back"
        );
        assert!(
            t.read_recent(2_000, true).contains("MARK_EARLY"),
            "a read long enough to reach it did not"
        );
        // And it is still where the user left it.
        assert_eq!(t.scroll_offset(), 0, "reading history moved the pane");
    }
}
