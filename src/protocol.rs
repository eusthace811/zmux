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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClientMsg {
    Hello { version: u32, cols: u16, rows: u16 },
    Input { data: Vec<u8> },
    Resize { cols: u16, rows: u16 },
    Detach,
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
}

impl Frame {
    pub fn blank(cols: u16, rows: u16) -> Self {
        Self { cols, rows, cells: vec![Cell::default(); cols as usize * rows as usize], cursor: None }
    }
}
