//! vt100 grid -> wire Frame -> ratatui Buffer.
//!
//! DESIGN.md §0.4: we read `screen.cell(row, col)` and never vt100's own ANSI
//! output. ratatui composites and diffs; there is no hand-written blitter.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

use crate::protocol::{Cell, Frame, COLOR_INDEXED, COLOR_RESET, MOD_BOLD, MOD_DIM, MOD_ITALIC,
                      MOD_REVERSE, MOD_UNDERLINE};

fn pack(c: vt100::Color) -> u32 {
    match c {
        vt100::Color::Default => COLOR_RESET,
        vt100::Color::Idx(i) => COLOR_INDEXED | i as u32,
        vt100::Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
    }
}

fn unpack(v: u32) -> Color {
    if v == COLOR_RESET {
        Color::Reset
    } else if v & COLOR_INDEXED != 0 {
        Color::Indexed((v & 0xFF) as u8)
    } else {
        Color::Rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
    }
}

/// Snapshot the visible screen into a wire Frame.
pub fn frame_from_screen(screen: &vt100::Screen) -> Frame {
    let (rows, cols) = screen.size();
    let mut cells = Vec::with_capacity(rows as usize * cols as usize);
    for row in 0..rows {
        for col in 0..cols {
            cells.push(match screen.cell(row, col) {
                Some(c) => {
                    let mut mods = 0u16;
                    if c.bold() { mods |= MOD_BOLD }
                    if c.dim() { mods |= MOD_DIM }
                    if c.italic() { mods |= MOD_ITALIC }
                    if c.underline() { mods |= MOD_UNDERLINE }
                    if c.inverse() { mods |= MOD_REVERSE }
                    let ch = c.contents().chars().next().unwrap_or(' ');
                    Cell { ch, fg: pack(c.fgcolor()), bg: pack(c.bgcolor()), mods }
                }
                None => Cell::default(),
            });
        }
    }
    let cursor = if screen.hide_cursor() {
        None
    } else {
        let (r, c) = screen.cursor_position();
        Some((c, r))
    };
    Frame { cols, rows, cells, cursor }
}

/// Paint a Frame into a ratatui Buffer at an offset.
///
/// The offset is what makes this compose: in Phase 3 each pane paints into its
/// own sub-Rect of one shared Buffer, and nothing else changes.
pub fn paint(buf: &mut Buffer, frame: &Frame, ox: u16, oy: u16) {
    let area = *buf.area();
    for row in 0..frame.rows {
        for col in 0..frame.cols {
            let (x, y) = (ox + col, oy + row);
            if x >= area.width || y >= area.height {
                continue;
            }
            let src = &frame.cells[row as usize * frame.cols as usize + col as usize];
            let Some(dst) = buf.cell_mut((x, y)) else { continue };
            let mut m = Modifier::empty();
            if src.mods & MOD_BOLD != 0 { m |= Modifier::BOLD }
            if src.mods & MOD_DIM != 0 { m |= Modifier::DIM }
            if src.mods & MOD_ITALIC != 0 { m |= Modifier::ITALIC }
            if src.mods & MOD_UNDERLINE != 0 { m |= Modifier::UNDERLINED }
            if src.mods & MOD_REVERSE != 0 { m |= Modifier::REVERSED }
            dst.set_char(src.ch);
            dst.set_style(Style::default().fg(unpack(src.fg)).bg(unpack(src.bg)).add_modifier(m));
        }
    }
}

/// Draw our own cursor as a reversed cell.
///
/// DESIGN.md §2.3: ConPTY exposes intermediate cursor positions during repaint,
/// so the native cursor flickers. herdr confirmed this is unfixable and draws
/// its own on Windows. Costs nothing.
pub fn paint_cursor(buf: &mut Buffer, frame: &Frame, ox: u16, oy: u16) {
    let Some((cx, cy)) = frame.cursor else { return };
    let (x, y) = (ox + cx, oy + cy);
    let area = *buf.area();
    if x >= area.width || y >= area.height {
        return;
    }
    if let Some(c) = buf.cell_mut((x, y)) {
        c.set_style(c.style().add_modifier(Modifier::REVERSED));
    }
}
