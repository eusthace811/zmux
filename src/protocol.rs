//! Wire protocol between client and daemon.
//!
//! DESIGN.md §3: versioned, bounded, length-prefixed. `interprocess` is
//! consumed as a byte stream, so message boundaries are ours to define.
//! Enum variants are APPEND-ONLY once a build is in use - an old decoder
//! cannot understand a new variant, so never reorder or reinterpret.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change. Handshake rejects a mismatch.
/// v2 added `ServerMsg::Delta`. An old v1 client would accept the handshake and
/// then fail to decode the new variant, so the version MUST move with the wire.
/// v3 added the window commands: a v3 client sending `Cmd::NewWindow` to a v2
/// daemon would be a decode error mid-stream, which the handshake catches first.
/// v4 added `Cmd::SelectWindow`, for the same reason.
/// v5 added the control connection - `ClientMsg::Control`, `Query`, `InputTo`
/// and the `Info`/`Capture` replies.
/// v6 added `PaneInfo::command`, `busy` and `last_exit`. `PaneInfo` is a
/// bincode STRUCT, and bincode does not tag fields - so unlike appending an
/// enum variant, a new field changes the encoding for every existing reader.
/// The handshake already refuses a mismatch with instructions, which is why
/// this costs a version rather than a corrupt frame.
/// v7 added `Frame::mouse` and `FrameDelta::mouse` - the same kind of struct
/// change - and `ClientMsg::Mouse`, which is only an appended variant.
/// v8 is the agent-coordination surface (DESIGN.md §12). `PaneInfo` gained
/// `window`, and `SessionInfo` gained `focus_id` - struct fields again, so
/// again a version. `Target`, `InputAt` and `Query::CaptureAt` are appended.
/// v9 added `ClientMsg::Paste`. Only an appended variant, which is free for a
/// NEW daemon reading an old client and fatal the other way: an old daemon
/// cannot decode it and drops the connection mid-session, on the first paste.
/// The handshake has to catch that before anyone pastes.
pub const PROTOCOL_VERSION: u32 = 9;

/// Refuse to allocate for a frame larger than this. Checked BEFORE allocating.
pub const MAX_FRAME_BYTES: u32 = 8 * 1024 * 1024;

/// Hard bounds on client-supplied geometry.
///
/// Without these, `Hello { cols: 65535, rows: 65535 }` reaches
/// `vt100::Screen::set_size` and asks for ~4 billion cells. A peer on a local
/// named pipe is not automatically trusted.
pub const MAX_COLS: u16 = 1000;
pub const MAX_ROWS: u16 = 500;
pub const MIN_USABLE_COLS: u16 = 8;
pub const MIN_USABLE_ROWS: u16 = 3;

/// Cap a single `Input` payload. Typing is small, and the client splits
/// anything larger - a paste from a host that does not bracket pastes comes
/// through as typing. Past this, `Input` is a bug or an attack.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

/// Cap one `Paste`. Separate from `MAX_INPUT_BYTES` because a paste cannot be
/// split: two bracketed pastes are two blocks to the program receiving them.
/// A megabyte is a very long prompt, and well inside `MAX_FRAME_BYTES`.
pub const MAX_PASTE_BYTES: usize = 1024 * 1024;

/// How far back a history read goes when the caller named no line count.
///
/// This is not only `capture`'s default: it is the window `Until::Output`
/// searches, so it is the observable bound on what a wait can notice. Enough
/// to hold an agent's last answer, small enough that omitting `-n` is not a
/// way to ask for a million rows by accident.
pub const RECENT_LINES_DEFAULT: usize = 200;

/// Most lines one `capture` will return, whatever it was asked for.
///
/// A pane may hold a million rows of scrollback (`Config::scrollback`), and
/// building that into one `String` on the daemon's heap to push down a pipe
/// is a memory spike a caller can ask for by typing a large number.
pub const MAX_CAPTURE_LINES: usize = 10_000;

/// Largest number of panes one session will tile.
pub const MAX_PANES: usize = 64;

/// Cap the reported command line. It comes from the shell, so it is as long as
/// whatever someone pasted; the status bar shows a word of it and `info`
/// carries the rest, and neither wants an unbounded string on the wire.
pub const MAX_COMMAND_CHARS: usize = 200;

/// Clamp client geometry into something we are willing to allocate.
pub fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(MIN_USABLE_COLS, MAX_COLS), rows.clamp(MIN_USABLE_ROWS, MAX_ROWS))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello { version: u32, cols: u16, rows: u16 },
    Input { data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Detach,
    // --- appended after v1 shipped. Append-only: never reorder the above. ---
    /// Ask the daemon to terminate the session and exit.
    Shutdown,
    /// A pane-management command, issued after the client's prefix key.
    Command(Cmd),
    /// Mouse click at a screen position: focus the pane under it.
    ///
    /// Superseded by `Mouse`, which can also reach a program that wants the
    /// mouse for itself. Nothing sends this any more; the daemon still
    /// answers it because variants are append-only and removing it would
    /// renumber every one below.
    Click { x: u16, y: u16 },
    // --- appended. Append-only. ---
    /// Open a CONTROL connection instead of attaching a terminal.
    ///
    /// Must be the first message, in place of `Hello`. The peer receives no
    /// frames and is NOT registered as a client, deliberately: a script
    /// driving the session has no viewport, so counting it would let it
    /// shrink the session to a size it invented, and would hold the idle
    /// timer open for as long as it stayed connected.
    ///
    /// Afterwards every message gets exactly one reply, except `Detach` and
    /// `Shutdown`, which end the connection - `Shutdown` after a final
    /// `ShuttingDown`, `Detach` immediately, because there is nothing left to
    /// say to a peer that is leaving.
    Control { version: u32 },
    /// Read session state or pane text. Control connections only.
    Query(Query),
    /// Write to a pane by POSITION in the current window, 1-based, without
    /// moving focus - the caller's reason for existing is to work in the
    /// background while somebody else uses the session.
    InputTo { pane: u8, data: Vec<u8> },
    // --- appended. Append-only. ---
    /// The mouse did something, in screen coordinates.
    ///
    /// Supersedes `Click`, which could only say "focus what is here" and so
    /// could never reach a program that wanted the mouse for itself.
    Mouse(MouseIn),
    // --- appended. Append-only. ---
    /// Write to a pane named by `Target`, which unlike `InputTo` can reach a
    /// pane in a window that is not on screen, and can name one by an id that
    /// does not move when the layout does.
    ///
    /// Supersedes `InputTo`. That one still answers - variants are
    /// append-only - but it can only say "the Nth pane of whatever window is
    /// current", which is the wrong handle for anything long-running.
    InputAt {
        target: Target,
        data: Vec<u8>,
        /// Wrap the payload in a bracketed paste, IF the program in that pane
        /// has asked for one. Only the daemon's vt100 state knows whether it
        /// has, which is why this is a request rather than a decision.
        paste: bool,
        /// Append Enter, in the SAME write as the text. A program that reads
        /// a line must not be able to see the text without the newline that
        /// submits it.
        enter: bool,
    },
    /// Block until something is true of a pane, or the timeout runs out.
    ///
    /// Server-side because the alternative is every caller writing its own
    /// polling loop over `info`, each with a different idea of how often to
    /// ask and what counts as done - and each of them holding a pane's
    /// position across the wait, which is the handle that moves.
    Wait {
        target: Target,
        until: Until,
        timeout_ms: u32,
        /// Ignore `until` until the pane has been seen doing something, or
        /// this many milliseconds have passed. 0 disables the gate.
        ///
        /// Closes the race that makes a naive "run it, then wait for quiet"
        /// return instantly: the shell has not yet read the line, so it is
        /// not busy yet, and the wait is satisfied by the quiet BEFORE the
        /// command rather than the quiet after it. herdr solves the same
        /// problem by requiring observed activity after a prompt.
        settle_ms: u32,
    },
    /// A pane saying what it is doing.
    ///
    /// The other way in is an OSC on the pane's own pty, which costs no
    /// process and needs neither this binary on PATH nor the session's name.
    /// This exists for a reporter whose output is captured and cannot reach
    /// the terminal - a hook run as a subprocess, or a script on another
    /// machine's behalf.
    Report { target: Target, report: Report },
    // --- appended. Append-only. ---
    /// Text the host terminal says was pasted, for the focused pane.
    ///
    /// Not `Input`, because only the daemon knows whether the program in that
    /// pane asked for bracketed paste. It wraps the text in the markers when
    /// the program did, and types it plainly when it did not - the same rule
    /// as `InputAt { paste: true }`, reached from an attached terminal.
    Paste { data: Vec<u8> },
}

/// Which pane a control command means.
///
/// `Position` is what the status bar shows and what a human would say, and it
/// is what the older commands take. It is also the one that MOVES: closing a
/// pane renumbers every pane after it, so anything that outlives a single
/// round trip should hold an `Id` instead.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    /// Wherever the user's keystrokes are going.
    Focused,
    /// 1-based position within the CURRENT window.
    Position(u8),
    /// The daemon's stable handle. Never reused, and valid in any window,
    /// including one that is not on screen. This is what `ZMUX_PANE` holds.
    Id(u32),
    /// A name the pane reported for itself. Unique among live panes.
    Name(String),
}

/// What the program in a pane says it is doing.
///
/// REPORTED, never inferred (DESIGN.md §12). zmux does not match a pane's
/// screen against patterns to work this out - that is how herdr does it, and
/// it costs 22 versioned regex manifests that break silently whenever some
/// other project changes a spinner. A pane that says nothing is `Unknown`,
/// and `Unknown` does not mean finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaneState {
    /// Nothing has been reported. NOT a claim that the pane is free - only
    /// that nobody said. `quiet_ms`, `command` and `bells` are what is left.
    #[default]
    Unknown,
    /// Ready for input, with nothing outstanding.
    Idle,
    /// Working on something.
    Working,
    /// Stopped, waiting for a person: an approval, a question, a choice.
    /// Sending it a prompt would answer the wrong question.
    Blocked,
    /// Finished what it was asked, and nobody has looked yet.
    Done,
}

impl PaneState {
    /// The name the report channel uses, and what `info` prints.
    pub fn as_str(self) -> &'static str {
        match self {
            PaneState::Unknown => "unknown",
            PaneState::Idle => "idle",
            PaneState::Working => "working",
            PaneState::Blocked => "blocked",
            PaneState::Done => "done",
        }
    }

    /// Parse a reported state. `None` for anything unrecognised, and the
    /// caller must then ignore the whole report rather than store `Unknown` -
    /// a typo in a hook must not erase a state that was true.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "idle" => PaneState::Idle,
            "working" => PaneState::Working,
            "blocked" => PaneState::Blocked,
            "done" => PaneState::Done,
            "unknown" => PaneState::Unknown,
            _ => return None,
        })
    }

    /// Nothing more is going to happen without someone's input.
    ///
    /// `Blocked` counts: it is settled in the sense that matters to a caller
    /// waiting on the pane - it has stopped, and it is not going to start
    /// again on its own.
    pub fn is_settled(self) -> bool {
        matches!(self, PaneState::Idle | PaneState::Done | PaneState::Blocked)
    }
}

/// What a pane says about itself.
///
/// Every field is optional because a report says only what changed: a turn
/// boundary sets `state` and nothing else, while a startup report sets
/// `agent` and `name` once and never again.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Report {
    pub state: Option<PaneState>,
    /// What is running, in its own words - `ampere`, `pi`, anything. Free
    /// text on purpose: an enum of known agents is a list to maintain, and
    /// zmux has no reason to have an opinion about which agents exist.
    pub agent: Option<String>,
    /// A handle for this pane, so a caller can say `reviewer` rather than
    /// track an id. Must satisfy `is_valid_name`.
    pub name: Option<String>,
    /// Monotonic per pane. A report at or below the last one accepted is
    /// DROPPED: two hooks firing out of order would otherwise leave the pane
    /// reading `working` after it went idle. Omit it and reports are taken in
    /// arrival order.
    pub seq: Option<u64>,
}

/// What to wait for. Append-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Until {
    /// Shell integration says no command is running. The right wait for an
    /// ordinary command; useless for an agent, which is ONE long command.
    Quiet,
    /// The pane reports exactly this state.
    State(PaneState),
    /// The pane reports any state that has stopped: idle, done or blocked.
    ///
    /// `blocked` counts. A caller waiting on an agent wants to know when it
    /// stops, and "it stopped to ask you something" is stopping - being told
    /// only about a clean finish means waiting out the timeout on the one
    /// case that needed a person.
    Settled,
    /// The pane has produced no output for this many milliseconds.
    ///
    /// The honest wait for a program that reports nothing about itself. It is
    /// a measurement of silence, not a claim that the work is done, and the
    /// caller chooses what length of silence it is willing to believe.
    QuietFor(u32),
    /// This text has appeared in the pane's recent output.
    ///
    /// A literal substring, not a pattern. A regex engine is a dependency and
    /// a binary this project has reasons to keep small (DESIGN.md §6.1), and
    /// `capture | grep` already exists for the cases a substring cannot
    /// express.
    ///
    /// Matches text that is ALREADY there, as herdr's `wait-output` does - so
    /// a caller that asks after the fact is not left waiting for something
    /// that has happened.
    ///
    /// BOUNDED, and the bound is worth knowing: each poll searches the last
    /// `RECENT_LINES_DEFAULT` lines, not the whole scrollback. A pane that
    /// produces more than that between two polls can carry a match past
    /// unseen. Reading further is linear and does not fix it - measured at
    /// 80x24, 200 lines costs 68us a poll and 5000 costs 1.6ms, which moves
    /// the threshold without closing it, and any burst can beat whatever it
    /// is moved to. Wait for something a command prints at the END, which is
    /// what "it finished" looks like anyway.
    Output(String),
    /// The pane rings the bell. Works for a program that reports nothing,
    /// because plenty of them ring it when they stop to ask.
    Bell,
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaitOutcome {
    /// The condition came true.
    Met,
    /// The time ran out first. NOT an error: a caller that asked to wait 30
    /// seconds and waited 30 seconds got what it asked for, and has to be
    /// able to tell that from a broken pipe.
    Timeout,
    /// The pane went away while we were waiting on it.
    Gone,
}

/// Longest text `Until::Output` will look for.
pub const MAX_MATCH_CHARS: usize = 200;

/// Longest name a pane may claim. Long enough to be descriptive, short enough
/// that a status bar and an error message can both show it whole.
pub const MAX_NAME_CHARS: usize = 32;

impl Target {
    /// Parse the `-p` argument: `3` a position, `@3` an id, `current` this
    /// pane, anything else a name.
    ///
    /// Positions are bare digits and names may not start with one, so the two
    /// cannot collide. `current` is reserved for the same reason.
    pub fn parse(s: &str, current: Option<u32>) -> Result<Self, String> {
        if s == "current" {
            return current.map(Target::Id).ok_or_else(|| {
                "`-p current` needs ZMUX_PANE, which is set only inside a zmux pane"
                    .to_string()
            });
        }
        if let Some(rest) = s.strip_prefix('@') {
            return rest.parse().map(Target::Id).map_err(|_| format!("'{s}' is not a pane id"));
        }
        if s.chars().all(|c| c.is_ascii_digit()) && !s.is_empty() {
            let n: u8 = s.parse().map_err(|_| format!("'{s}' is not a pane position"))?;
            if n == 0 {
                return Err("pane positions start at 1".into());
            }
            return Ok(Target::Position(n));
        }
        if !is_valid_name(s) {
            return Err(format!(
                "'{s}' is not a pane position (1), an id (@1) or a name \
                 (a-z, then a-z 0-9 _ -, up to {MAX_NAME_CHARS})"
            ));
        }
        Ok(Target::Name(s.to_string()))
    }
}

/// herdr's rule (`skills/herdr/SKILL.md`), and for its reason: a name has to be
/// typeable, unambiguous against a position, and safe in a shell argument.
pub fn is_valid_name(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_lowercase())
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        && s.chars().count() <= MAX_NAME_CHARS
        && s != "current"
}

/// What a control connection can ask for. Append-only.
///
/// Not `Copy` any more: `Target::Name` owns a string. Nothing passed one by
/// value in a hot path, so this costs nothing but a few `&`s.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    /// Windows, panes, focus and geometry.
    Info,
    /// The visible screen of one pane of the current window.
    ///
    /// `pane` is a 1-based position, `None` meaning the focused pane.
    /// `lines` keeps only that many lines from the bottom; 0 means all of
    /// them. Only what is on screen: scrollback is not captured.
    Capture { pane: Option<u8>, lines: u16 },
    // --- appended. Append-only. ---
    /// The visible screen of the pane named by `Target`.
    ///
    /// Supersedes `Capture`, for the same reason `InputAt` supersedes
    /// `InputTo`: a position cannot name a pane in a background window, and
    /// does not survive the layout changing under it.
    CaptureAt { target: Target, lines: u16, source: Source },
}

/// How much of a pane to read, and in what shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The rows on screen, and only those. What the user can see.
    #[default]
    Screen,
    /// Recent output including scrollback, so a long answer that has already
    /// scrolled past is still readable.
    Recent,
    /// `Recent`, with rows that only ended because they ran out of width
    /// joined back together.
    ///
    /// The one to use for a transcript. A pane is as wide as its share of the
    /// window, so a 60-column pane breaks every long line - and those breaks
    /// are an artefact of the layout, not of what was written.
    RecentUnwrapped,
}

/// The answer to `Query::Info`, and to every control mutation - a caller sees
/// the state its own command produced without a second round trip.
///
/// A SNAPSHOT, not a transaction. It is read immediately after the command,
/// before the frame is composited, but nothing locks the session across the
/// two: an attached human moving focus in that instant is reported as though
/// the command had done it. `PaneInfo::id` is the handle that survives a
/// focus change; `focus` is only as fresh as the moment it was read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session: String,
    /// Composited size: the smallest attached viewport, or 80x24 with nobody
    /// attached.
    pub cols: u16,
    pub rows: u16,
    /// 0-based, exactly what the status bar shows and `SelectWindow` takes.
    pub window: usize,
    pub windows: usize,
    /// 1-based position of the focused pane WITHIN ITS WINDOW, 0 if there is
    /// none. Not an index into `panes`, which now spans every window.
    pub focus: usize,
    /// `PaneInfo::id` of the focused pane, 0 if there is none.
    ///
    /// The unambiguous handle. `focus` needs `window` beside it to mean
    /// anything now that `panes` is not one window's worth, and a caller that
    /// wants "where are the keystrokes going" should not have to pair two
    /// fields and scan a list to find out.
    pub focus_id: u32,
    /// EVERY pane in the session, each tagged with the window it is in,
    /// ordered by window and then by the position the status bar counts.
    ///
    /// This used to be the current window only, which made a background
    /// window's panes unreachable and undiscoverable - you had to switch to a
    /// window, moving the user's screen, merely to find out what was in it.
    pub panes: Vec<PaneInfo>,
    /// The pane this command created, if it created one.
    ///
    /// A focus-taking split announces itself through `focus`; a background
    /// one cannot, because the whole point is that the focus did not move.
    /// Diffing the pane list against a previous `info` would race the user,
    /// so the daemon says it outright.
    ///
    /// NOTE for anyone tempted to tidy this with `skip_serializing_if`:
    /// bincode 2 goes through the `Serialize` impl, so a skipped field is
    /// skipped ON THE WIRE and the decoder then reads the next field's bytes
    /// as this one. It stays a plain `Option`, and JSON shows `null`.
    pub created: Option<u32>,
}

/// One pane of the current window.
///
/// `position` is what a human would say, because it is what the status bar
/// shows - but it is scoped to one window and it MOVES when a pane closes.
/// `id` is the daemon's handle: stable for the pane's lifetime, never reused,
/// valid in any window, and what `ZMUX_PANE` holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub position: usize,
    /// 0-based index of the window this pane is in, matching the status bar
    /// and `select-window`. `position` is within THIS window, so the two are
    /// needed together - or `id` on its own.
    pub window: usize,
    pub id: u32,
    pub cols: u16,
    pub rows: u16,
    /// Last directory the shell reported through OSC 7, if it reported one.
    pub cwd: Option<String>,
    /// The last command the shell reported starting, if shell integration is
    /// live. Kept after it finishes: with `busy` false and `last_exit` set,
    /// this says what just ran and how it went.
    pub command: Option<String>,
    /// Running something right now. A caller can poll this instead of
    /// sleeping and hoping.
    pub busy: bool,
    /// Exit status of the last finished command.
    pub last_exit: Option<i32>,
    // --- what the pane said about itself (DESIGN.md §12) ---
    /// The last state this pane reported. `Unknown` if it reported none.
    pub state: PaneState,
    /// What the pane says is running in it.
    pub agent: Option<String>,
    /// The name this pane claimed, usable as a `Target`.
    pub name: Option<String>,
    // --- measured facts, never inferences (DESIGN.md §12) ---
    /// Milliseconds since this pane last produced ANY output.
    ///
    /// zmux does not read a pane's screen to decide whether the program in
    /// it has finished. This is the honest substitute: a program that has
    /// been silent for a while is probably waiting, and the caller gets to
    /// decide what "a while" means rather than being handed a verdict.
    pub quiet_ms: u64,
    /// How many times this pane has rung the bell, ever.
    ///
    /// Only useful compared against a value read earlier - an increase means
    /// the pane asked for attention since then. See `Signals::bells` for why
    /// it is a count and not a flag.
    pub bells: u32,
    /// The pane's window title, as it set it.
    ///
    /// What the pane SAYS it is doing. Reported verbatim, because deciding
    /// what a title means about an agent's state is exactly the pattern
    /// matching this design refuses to ship.
    pub title: Option<String>,
}

/// A mouse event, in SCREEN coordinates.
///
/// The client knows where the pointer is; only the daemon knows which pane
/// that lands in, and what the program inside that pane has asked to be told.
/// So the raw event travels and the daemon decides - the mirror of
/// `Frame::app_cursor`, which travels the other way for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseIn {
    pub x: u16,
    pub y: u16,
    pub kind: MouseKind,
    /// `MOUSE_SHIFT`, `MOUSE_ALT`, `MOUSE_CTRL`.
    pub mods: u8,
}

pub const MOUSE_SHIFT: u8 = 1;
pub const MOUSE_ALT: u8 = 2;
pub const MOUSE_CTRL: u8 = 4;

/// What the mouse did. Append-only, like every other enum here.
///
/// The button is xterm's numbering - 0 left, 1 middle, 2 right - because that
/// is what goes on the wire to the program in the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseKind {
    Press(u8),
    Release(u8),
    /// Moved with a button held.
    Drag(u8),
    /// Moved with no button held.
    Move,
    WheelUp,
    WheelDown,
}

/// Pane management. Append-only, like everything else on this wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cmd {
    SplitHorizontal,
    SplitVertical,
    ClosePane,
    FocusLeft,
    FocusRight,
    FocusUp,
    FocusDown,
    ResizeLeft,
    ResizeRight,
    ResizeUp,
    ResizeDown,
    // --- appended. Append-only. ---
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollToBottom,
    // --- appended. Append-only. ---
    /// Create a window and switch to it.
    NewWindow,
    NextWindow,
    PrevWindow,
    // --- appended. Append-only. ---
    /// Switch to the window at this index, ignored if there is none.
    ///
    /// Indices are what the status bar shows and what `C-b <digit>` sends, so
    /// they are 0-based - `C-b 1` is the second window, as the bar says.
    SelectWindow(u8),
    // --- appended. Append-only. ---
    /// Split, but leave the focus where the user had it.
    ///
    /// No key is bound to these: someone at the keyboard splitting a pane
    /// wants to be in it. They exist for a caller making a pane to work in
    /// while somebody else is typing. `SessionInfo::created` says which pane
    /// appeared, because with the focus unmoved nothing else in the reply
    /// can.
    SplitHorizontalBackground,
    SplitVerticalBackground,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    Hello { version: u32, session: String },
    /// Rejected handshake; carries a human-readable reason.
    Refused { reason: String },
    /// A full grid snapshot. Phase 1 sends these; deltas come later.
    Frame(Frame),
    PaneExited { code: i32 },
    /// Reserved. Nothing sends this: a detach is complete when the daemon
    /// drops the connection, and the client has already stopped reading by
    /// then. Kept because variants are append-only - removing it would
    /// renumber every variant below.
    Detached,
    // --- appended after v1 shipped. Append-only. ---
    /// Daemon is shutting down.
    ShuttingDown,
    // --- appended. Append-only. ---
    /// Only the rows that changed since `base_revision`.
    ///
    /// Typing changes one or two rows out of thirty, so sending the whole grid
    /// every tick is almost all waste. A client whose current revision is not
    /// `base_revision` must discard the delta and wait for a full frame.
    Delta(FrameDelta),
    // --- appended. Append-only. ---
    /// Reply to `Query::Info`, and to every control mutation.
    Info(SessionInfo),
    /// Reply to `Query::Capture`.
    Capture { text: String },
    // --- appended. Append-only. ---
    /// Reply to `Wait`. Carries the session state as it was when the wait
    /// ended, so the caller does not need a second round trip to act on it.
    Waited { outcome: WaitOutcome, info: SessionInfo },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameDelta {
    pub base_revision: u64,
    pub revision: u64,
    pub cols: u16,
    pub rows: u16,
    /// (row index, that row's cells).
    pub rows_changed: Vec<(u16, Vec<Cell>)>,
    pub cursor: Option<(u16, u16)>,
    pub app_cursor: bool,
    /// Whether the focused pane's program wants the mouse. See `Frame`.
    pub mouse: bool,
}

impl Frame {
    /// True when the cell count matches the declared geometry.
    ///
    /// A frame is peer-supplied data. A short `cells` vector would index out
    /// of bounds during painting, so it is checked before anything reads it.
    pub fn is_well_formed(&self) -> bool {
        self.cells.len() == self.cols as usize * self.rows as usize
            && self.cols <= MAX_COLS
            && self.rows <= MAX_ROWS
    }

    /// Rows that differ from `prev`, or `None` when the geometry changed and a
    /// delta would be meaningless.
    pub fn delta_from(&self, prev: &Frame) -> Option<FrameDelta> {
        if prev.cols != self.cols || prev.rows != self.rows {
            return None;
        }
        let w = self.cols as usize;
        let mut rows_changed = Vec::new();
        for r in 0..self.rows {
            let a = &self.cells[r as usize * w..(r as usize + 1) * w];
            let b = &prev.cells[r as usize * w..(r as usize + 1) * w];
            if a != b {
                rows_changed.push((r, a.to_vec()));
            }
        }
        Some(FrameDelta {
            base_revision: prev.revision,
            revision: self.revision,
            cols: self.cols,
            rows: self.rows,
            rows_changed,
            cursor: self.cursor,
            app_cursor: self.app_cursor,
            mouse: self.mouse,
        })
    }

    /// Apply a delta in place. Returns false if it does not apply to us, in
    /// which case the caller must wait for a full frame rather than render
    /// something half-updated.
    pub fn apply_delta(&mut self, d: &FrameDelta) -> bool {
        if d.base_revision != self.revision || d.cols != self.cols || d.rows != self.rows {
            return false;
        }
        // Concurrent publishers can snapshot at N, pause, and emit a delta
        // claiming base N+1 -> revision N. Checking the base alone would
        // accept it and walk the client's revision backwards.
        if d.revision < d.base_revision {
            return false;
        }
        let w = self.cols as usize;
        // Validate EVERY row before mutating any of them. Checking as we go
        // would leave the frame half-updated when a later row is malformed -
        // exactly the corrupted screen the base check exists to prevent.
        if d.rows_changed
            .iter()
            .any(|(r, cells)| (*r as usize) >= self.rows as usize || cells.len() != w)
        {
            return false;
        }
        for (r, cells) in &d.rows_changed {
            self.cells[*r as usize * w..(*r as usize + 1) * w].clone_from_slice(cells);
        }
        self.cursor = d.cursor;
        self.app_cursor = d.app_cursor;
        self.mouse = d.mouse;
        self.revision = d.revision;
        true
    }
}

/// One cell of the composited screen. Mirrors what a ratatui `Cell` carries,
/// which is what the renderer needs (DESIGN.md §0.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    pub ch: char,
    pub fg: u32,
    pub bg: u32,
    pub mods: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Self { ch: ' ', fg: COLOR_RESET, bg: COLOR_RESET, mods: 0 }
    }
}

/// Sentinel meaning "terminal default", distinct from any indexed/RGB colour.
pub const COLOR_RESET: u32 = 0xFFFF_FFFF;
/// Indexed colours are stored as 0x0100_0000 | index.
pub const COLOR_INDEXED: u32 = 0x0100_0000;

pub const MOD_BOLD: u16 = 1 << 0;
pub const MOD_DIM: u16 = 1 << 1;
pub const MOD_ITALIC: u16 = 1 << 2;
pub const MOD_UNDERLINE: u16 = 1 << 3;
pub const MOD_REVERSE: u16 = 1 << 4;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frame {
    pub cols: u16,
    pub rows: u16,
    pub cells: Vec<Cell>,
    /// (col, row) of the cursor, if it should be shown.
    pub cursor: Option<(u16, u16)>,
    /// Whether the focused pane has DECCKM set (application cursor keys).
    ///
    /// Only the daemon's vt100 state knows this, but the CLIENT encodes keys,
    /// so it has to travel. Without it arrows are wrong in vim and in
    /// readline's vi mode.
    pub app_cursor: bool,
    /// Whether the focused pane's program has asked for the mouse.
    ///
    /// Travels for the same reason as `app_cursor`: only the daemon's vt100
    /// state knows, and only the CLIENT can act on it, by handing the host
    /// terminal's mouse over or taking it back.
    pub mouse: bool,
    /// Monotonic session revision this frame was composed at.
    ///
    /// Arrival order is not trustworthy: a queued broadcast can be delivered
    /// after a newer snapshot written directly on attach. The client keeps the
    /// highest revision it has seen and drops anything older, so the screen
    /// cannot regress.
    pub revision: u64,
}

impl Frame {
    pub fn blank(cols: u16, rows: u16) -> Self {
        Self {
            cols,
            rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            cursor: None,
            app_cursor: false,
            mouse: false,
            revision: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A peer on a local pipe is not automatically trusted, and this is the
    /// only thing standing between `Hello { cols: 65535, rows: 65535 }` and
    /// `vt100::Screen::set_size` being asked for four billion cells.
    ///
    /// Untested until now, which is the wrong state for a bound whose whole
    /// job is to survive a hostile number.
    #[test]
    fn client_geometry_is_clamped_into_something_we_will_allocate() {
        assert_eq!(clamp_size(u16::MAX, u16::MAX), (MAX_COLS, MAX_ROWS));
        assert_eq!(clamp_size(0, 0), (MIN_USABLE_COLS, MIN_USABLE_ROWS));
        // A minimised client reporting nothing must not shrink the session to
        // zero; `apply_size` filters those out, and this is the backstop.
        assert_eq!(clamp_size(1, 1), (MIN_USABLE_COLS, MIN_USABLE_ROWS));
        // An ordinary size passes through untouched.
        assert_eq!(clamp_size(120, 40), (120, 40));
        let (c, r) = clamp_size(u16::MAX, u16::MAX);
        assert!(
            (c as usize) * (r as usize) < 1_000_000,
            "the clamped grid is still too large to allocate per pane"
        );
    }

    /// A name becomes a `Target`, so it has to be unambiguous against a
    /// position and safe as a shell argument.
    #[test]
    fn a_pane_name_is_typeable_unambiguous_and_bounded() {
        for good in ["reviewer", "a", "build-2", "x_y", &"a".repeat(MAX_NAME_CHARS)] {
            assert!(is_valid_name(good), "{good:?} should be a legal name");
        }
        for bad in [
            "",                              // nothing to address
            "2nd",                           // a leading digit is a position
            "Reviewer",                      // capitals: a typo, not a pane
            "my pane",                       // spaces need quoting
            "rm -rf /",                      // and so does everything here
            "père",                          // non-ASCII
            "current",                       // reserved for `-p current`
            &"a".repeat(MAX_NAME_CHARS + 1), // unbounded on the wire
        ] {
            assert!(!is_valid_name(bad), "{bad:?} should be refused");
        }
    }

    /// `zmux report --state done` and `zmux wait --until done` have to
    /// mean the same thing, and they meet only through these two functions.
    #[test]
    fn every_state_survives_the_round_trip_through_its_own_name() {
        for state in [
            PaneState::Unknown,
            PaneState::Idle,
            PaneState::Working,
            PaneState::Blocked,
            PaneState::Done,
        ] {
            assert_eq!(PaneState::parse(state.as_str()), Some(state), "{state:?}");
        }
        // Anything else is refused rather than read as `Unknown` - a typo in
        // somebody's hook must not erase a state that was true.
        for junk in ["", "IDLE", "idle ", "finished", "0"] {
            assert_eq!(PaneState::parse(junk), None, "{junk:?} was accepted");
        }
        // What a caller waiting on `settled` is told to expect.
        for settled in [PaneState::Idle, PaneState::Done, PaneState::Blocked] {
            assert!(settled.is_settled(), "{settled:?}");
        }
        for going in [PaneState::Unknown, PaneState::Working] {
            assert!(!going.is_settled(), "{going:?} is not a pane that has stopped");
        }
    }

    /// A frame is peer-supplied data and the client indexes into it.
    #[test]
    fn a_frame_that_does_not_match_its_own_geometry_is_not_well_formed() {
        let good = Frame::blank(10, 3);
        assert!(good.is_well_formed());

        let mut short = Frame::blank(10, 3);
        short.cells.pop();
        assert!(!short.is_well_formed(), "a short cell vector would index out of bounds");

        let mut huge = Frame::blank(10, 3);
        huge.cols = MAX_COLS + 1;
        assert!(!huge.is_well_formed(), "geometry past the clamp was accepted");
    }

    fn delta_of(prev: &Frame, next: &Frame) -> FrameDelta {
        next.delta_from(prev).expect("same geometry")
    }

    /// A delta carries the rows that CHANGED, and only those. That is the
    /// entire reason it exists.
    #[test]
    fn a_delta_carries_only_the_rows_that_moved() {
        let base = Frame::blank(10, 4);
        let mut next = base.clone();
        next.revision = 1;
        assert!(
            delta_of(&base, &next).rows_changed.is_empty(),
            "an unchanged screen produced a delta with rows in it"
        );

        next.cells[2 * 10 + 5].ch = 'x';
        let d = delta_of(&base, &next);
        assert_eq!(d.rows_changed.len(), 1, "one changed cell touched more than its row");
        assert_eq!(d.rows_changed[0].0, 2);

        // And it applies.
        let mut client = base.clone();
        assert!(client.apply_delta(&d));
        assert_eq!(client.cells[2 * 10 + 5].ch, 'x');
        assert_eq!(client.revision, 1);
    }

    /// The guard that exists because it actually happened: two publishers can
    /// snapshot at N, pause, and emit a delta claiming base N+1 -> revision N.
    ///
    /// Checking the base alone accepts it and walks the client's revision
    /// BACKWARDS, after which every later delta is refused and nothing asks
    /// for a resync - the frozen display DESIGN.md §4 describes.
    #[test]
    fn a_delta_that_would_move_the_revision_backwards_is_refused() {
        let mut client = Frame::blank(10, 3);
        client.revision = 5;
        let backwards = FrameDelta {
            base_revision: 5,
            revision: 4, // older than the base it claims
            cols: 10,
            rows: 3,
            rows_changed: vec![(0, vec![Cell::default(); 10])],
            cursor: None,
            app_cursor: false,
            mouse: false,
        };
        assert!(!client.apply_delta(&backwards), "a backwards delta was applied");
        assert_eq!(client.revision, 5, "the client's revision moved anyway");
    }

    /// Rows are validated BEFORE any of them is written.
    ///
    /// Checking as it goes would leave the frame half-updated when a later
    /// row is malformed - a corrupted screen, which is the thing the base
    /// check exists to prevent in the first place.
    #[test]
    fn a_delta_with_one_bad_row_changes_nothing_at_all() {
        let mut client = Frame::blank(4, 3);
        let before = client.cells.clone();
        for bad in [
            // A good row first, then one that is too short: if it applied as
            // it went, row 0 would already be written.
            vec![(0u16, vec![Cell { ch: 'a', ..Cell::default() }; 4]), (1, vec![Cell::default(); 3])],
            // A row index past the end of the frame.
            vec![(0, vec![Cell { ch: 'b', ..Cell::default() }; 4]), (99, vec![Cell::default(); 4])],
        ] {
            let d = FrameDelta {
                base_revision: 0,
                revision: 1,
                cols: 4,
                rows: 3,
                rows_changed: bad,
                cursor: None,
                app_cursor: false,
                mouse: false,
            };
            assert!(!client.apply_delta(&d), "a malformed delta was accepted");
            assert_eq!(client.cells, before, "a refused delta wrote part of itself anyway");
            assert_eq!(client.revision, 0);
        }
    }

    /// Geometry changing means no delta can describe the transition, so one
    /// is not offered at all.
    #[test]
    fn no_delta_is_offered_across_a_resize() {
        let small = Frame::blank(10, 3);
        let big = Frame::blank(20, 3);
        assert!(big.delta_from(&small).is_none());
        assert!(small.delta_from(&big).is_none());
    }
}
