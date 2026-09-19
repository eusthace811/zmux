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
    Frame { cols, rows, cells, cursor, app_cursor: screen.application_cursor(), revision: 0 }
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

// ---------------------------------------------------------------------------
// Compositing (Phase 3)
// ---------------------------------------------------------------------------

use crate::layout::{Dir, Divider, Layout, PaneId, Rect as LRect};

/// Copy one pane's grid into `dst` at `at`, clipped to `at`.
///
/// Clipping matters: the pty is resized to the pane rect, but a resize is
/// asynchronous, so a frame produced just before the resize lands can be
/// larger than the rect it must go in. Without clipping that scribbles over
/// the neighbouring pane.
pub fn blit(dst: &mut Frame, src: &Frame, at: LRect) {
    for row in 0..src.rows.min(at.h) {
        for col in 0..src.cols.min(at.w) {
            let (x, y) = (at.x + col, at.y + row);
            if x >= dst.cols || y >= dst.rows {
                continue;
            }
            dst.cells[y as usize * dst.cols as usize + x as usize] =
                src.cells[row as usize * src.cols as usize + col as usize].clone();
        }
    }
}

fn put(dst: &mut Frame, x: u16, y: u16, ch: char, mods: u16) {
    if x < dst.cols && y < dst.rows {
        let c = &mut dst.cells[y as usize * dst.cols as usize + x as usize];
        c.ch = ch;
        c.mods = mods;
    }
}

pub fn draw_dividers(dst: &mut Frame, dividers: &[Divider]) {
    for d in dividers {
        let ch = match d.dir {
            Dir::Horizontal => '│',
            Dir::Vertical => '─',
        };
        for row in 0..d.rect.h {
            for col in 0..d.rect.w {
                put(dst, d.rect.x + col, d.rect.y + row, ch, 0);
            }
        }
    }
}

/// One-line status bar along the bottom.
pub fn draw_status(dst: &mut Frame, text: &str) {
    if dst.rows == 0 {
        return;
    }
    let y = dst.rows - 1;
    for x in 0..dst.cols {
        put(dst, x, y, ' ', MOD_REVERSE);
    }
    for (i, ch) in text.chars().take(dst.cols as usize).enumerate() {
        put(dst, i as u16, y, ch, MOD_REVERSE);
    }
}

/// Mark the focused pane so it is obvious which one takes input.
pub fn highlight_focus(dst: &mut Frame, dividers: &[Divider], focus: LRect) {
    let _ = dividers;
    // A bold top-left corner marker is cheap and does not steal a row.
    if focus.w > 0 && focus.h > 0 {
        let idx = focus.y as usize * dst.cols as usize + focus.x as usize;
        if idx < dst.cells.len() {
            dst.cells[idx].mods |= MOD_BOLD;
        }
    }
}

/// The area available to panes: everything above the status bar.
pub fn pane_area(cols: u16, rows: u16) -> LRect {
    pane_area_with(cols, rows, true)
}

pub fn pane_area_with(cols: u16, rows: u16, status_bar: bool) -> LRect {
    LRect::new(0, 0, cols, if status_bar { rows.saturating_sub(1) } else { rows })
}

/// Compose the whole screen from the layout and each pane's current grid.
///
/// The DAEMON does this, not the client: it owns the layout and every vt100
/// state, so one composited Frame goes over the wire and the client stays
/// dumb. Splits therefore need no protocol change at all.
pub fn composite(
    cols: u16,
    rows: u16,
    layout: &Layout,
    pane_frame: impl Fn(PaneId) -> Option<Frame>,
    status: Option<&str>,
) -> Frame {
    let mut out = Frame::blank(cols, rows);
    let area = pane_area_with(cols, rows, status.is_some());
    let (panes, dividers) = layout.compute(area);
    let focus = layout.focus();
    let mut cursor = None;

    for (id, rect) in &panes {
        if let Some(f) = pane_frame(*id) {
            blit(&mut out, &f, *rect);
            if *id == focus {
                // Key encoding follows the FOCUSED pane's mode - that is the
                // pane the keystrokes will reach.
                out.app_cursor = f.app_cursor;
                if let Some((cx, cy)) = f.cursor {
                    if cx < rect.w && cy < rect.h {
                        cursor = Some((rect.x + cx, rect.y + cy));
                    }
                }
            }
        }
    }
    draw_dividers(&mut out, &dividers);
    if let Some((_, r)) = panes.iter().find(|(p, _)| *p == focus) {
        highlight_focus(&mut out, &dividers, *r);
    }
    if let Some(text) = status {
        draw_status(&mut out, text);
    }
    out.cursor = cursor;
    out
}
