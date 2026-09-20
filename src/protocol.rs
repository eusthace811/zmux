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
pub const PROTOCOL_VERSION: u32 = 7;

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

/// Cap a single `Input` payload. Keystrokes and pastes are small; anything
/// larger is a bug or an attack.
pub const MAX_INPUT_BYTES: usize = 64 * 1024;

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
}

/// What a control connection can ask for. Append-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Query {
    /// Windows, panes, focus and geometry.
    Info,
    /// The visible screen of one pane of the current window.
    ///
    /// `pane` is a 1-based position, `None` meaning the focused pane.
    /// `lines` keeps only that many lines from the bottom; 0 means all of
    /// them. Only what is on screen: scrollback is not captured.
    Capture { pane: Option<u8>, lines: u16 },
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
    /// 1-based position of the focused pane within `panes`, 0 if there is
    /// none.
    pub focus: usize,
    /// The current window's panes, in the order the status bar counts them.
    pub panes: Vec<PaneInfo>,
}

/// One pane of the current window.
///
/// `position` is what every control command takes, because it is what the
/// status bar shows. `id` is the daemon's internal handle: stable for the
/// pane's lifetime and never reused, but meaningless to a user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneInfo {
    pub position: usize,
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
