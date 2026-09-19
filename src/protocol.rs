//! Wire protocol between client and daemon.
//!
//! DESIGN.md §3: versioned, bounded, length-prefixed. `interprocess` is
//! consumed as a byte stream, so message boundaries are ours to define.
//! Enum variants are APPEND-ONLY once a build is in use - an old decoder
//! cannot understand a new variant, so never reorder or reinterpret.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change. Handshake rejects a mismatch.
pub const PROTOCOL_VERSION: u32 = 1;

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
    Click { x: u16, y: u16 },
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServerMsg {
    Hello { version: u32, session: String },
    /// Rejected handshake; carries a human-readable reason.
    Refused { reason: String },
    /// A full grid snapshot. Phase 1 sends these; deltas come later.
    Frame(Frame),
    PaneExited { code: i32 },
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
}

impl Frame {
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
        })
    }

    /// Apply a delta in place. Returns false if it does not apply to us, in
    /// which case the caller must wait for a full frame rather than render
    /// something half-updated.
    pub fn apply_delta(&mut self, d: &FrameDelta) -> bool {
        if d.base_revision != self.revision || d.cols != self.cols || d.rows != self.rows {
            return false;
        }
        let w = self.cols as usize;
        for (r, cells) in &d.rows_changed {
            if (*r as usize) >= self.rows as usize || cells.len() != w {
                return false;
            }
            self.cells[*r as usize * w..(*r as usize + 1) * w].clone_from_slice(cells);
        }
        self.cursor = d.cursor;
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
            revision: 0,
        }
    }
}
