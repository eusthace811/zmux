//! vt100 grid -> wire Frame -> ratatui Buffer.
//!
//! DESIGN.md §0.4: we read `screen.cell(row, col)` and never vt100's own ANSI
//! output. ratatui composites and diffs; there is no hand-written blitter.

use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier, Style};

use crate::protocol::{Cell, Frame, COLOR_INDEXED, COLOR_RESET, MOD_BOLD, MOD_DIM, MOD_ITALIC,
                      MOD_REVERSE, MOD_UNDERLINE};

// ---------------------------------------------------------------------------
// Chrome: the status bar and dividers we draw ourselves
// ---------------------------------------------------------------------------

/// Pack a 256-colour palette index the way `protocol::Cell` stores colours.
const fn idx(i: u8) -> u32 {
    COLOR_INDEXED | i as u32
}

// Fixed palette indices rather than the terminal's 0-15, which a colour scheme
// remaps: the chrome then looks the same whatever the user's Git Bash theme
// is, and cannot come out invisible because the scheme painted, say, "white"
// onto a light background. 236+ are greys from the 256-colour cube.
const BAR_BG: u32 = idx(236);
const SEP_FG: u32 = idx(240);
const INFO_FG: u32 = idx(250);
const WINDOW_FG: u32 = idx(245);
const NAME_FG: u32 = idx(231);
const NAME_BG: u32 = idx(24);
const ACTIVE_FG: u32 = idx(235);
const ACTIVE_BG: u32 = idx(214);
const ALERT_FG: u32 = idx(214);
/// Dividers recede. They are structure, not content - drawn in the default
/// foreground they were the brightest thing on the screen.
const DIVIDER_FG: u32 = idx(240);

/// What a run of status-bar text MEANS. The session says what it is; this
/// module alone decides what it looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The session name.
    Name,
    /// Punctuation between the parts.
    Sep,
    /// Counts and identifiers.
    Info,
    /// A window you are not on.
    Window,
    /// The window you are on.
    ActiveWindow,
    /// Something that wants noticing, like being scrolled off the live view.
    Alert,
}

impl Role {
    fn style(self) -> (u32, u32, u16) {
        match self {
            Role::Name => (NAME_FG, NAME_BG, MOD_BOLD),
            Role::Sep => (SEP_FG, BAR_BG, 0),
            Role::Info => (INFO_FG, BAR_BG, 0),
            Role::Window => (WINDOW_FG, BAR_BG, 0),
            Role::ActiveWindow => (ACTIVE_FG, ACTIVE_BG, MOD_BOLD),
            Role::Alert => (ALERT_FG, BAR_BG, MOD_BOLD),
        }
    }
}

/// One run of status-bar text with its role.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub role: Role,
}

fn span(role: Role, text: impl Into<String>) -> Span {
    Span { role, text: text.into() }
}

fn width_of(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.chars().count()).sum()
}

/// Build the status line.
///
/// `focus` is the focused pane's POSITION in the current window, counting
/// from 1, or 0 when it cannot be placed. It is deliberately not the `PaneId`:
/// that is an allocator counter, so after a few splits and closes the bar read
/// "2 panes | pane 7" and invited you to look for panes 3 to 6.
///
/// Pure, so the degradation ladder can be tested at every width without
/// starting a daemon and a shell - it used to be reachable only through a live
/// session, so only the widest rung was ever exercised.
#[allow(clippy::too_many_arguments)]
pub fn status_spans(
    name: &str,
    windows: usize,
    current: usize,
    panes: usize,
    focus: usize,
    scrolled: bool,
    width: usize,
) -> Vec<Span> {
    let plural = if panes == 1 { "" } else { "s" };

    // The window list appears only once there IS more than one window. With a
    // single window it would be a constant "0*" taking space from the parts
    // that actually vary on a narrow terminal.
    let list = |v: &mut Vec<Span>| {
        if windows > 1 {
            for i in 0..windows {
                // The digit is also the key that jumps there: `C-b <digit>`.
                // The `*` is belt and braces - the colour says the same thing,
                // and this keeps it legible if one of them is lost.
                let (role, mark) = if i == current {
                    (Role::ActiveWindow, "*")
                } else {
                    (Role::Window, "")
                };
                v.push(span(role, format!("{i}{mark} ")));
            }
            v.push(span(Role::Sep, "| "));
        }
    };
    let alert = |v: &mut Vec<Span>| {
        if scrolled {
            v.push(span(Role::Sep, "| "));
            v.push(span(Role::Alert, "SCROLL "));
        }
    };
    let tail = |v: &mut Vec<Span>| {
        list(v);
        v.push(span(Role::Info, format!("{panes} pane{plural} ")));
        // With one pane there is nothing to be focused BUT that pane, so
        // saying so was pure noise - and next to the count it read as two
        // numbers that disagreed.
        if panes > 1 && focus > 0 {
            v.push(span(Role::Sep, "| "));
            v.push(span(Role::Info, format!("pane {focus} ")));
        }
        alert(v);
    };

    // Drop the least useful parts first rather than letting the whole line be
    // truncated. On a narrow pane the old format showed a clipped session name
    // and nothing else - the pane count, which is what you actually need, fell
    // off the end. Which window you are on outranks the pane count: getting
    // lost between windows is worse than not knowing how many panes this one
    // holds.
    let mut full = vec![span(Role::Name, format!(" {name} ")), span(Role::Sep, "| ")];
    tail(&mut full);

    let mut without_name = vec![span(Role::Sep, " ")];
    tail(&mut without_name);

    let mut short = vec![span(Role::Sep, " ")];
    if windows > 1 {
        short.push(span(Role::ActiveWindow, format!("{current}/{windows} ")));
        short.push(span(Role::Sep, "| "));
    }
    short.push(span(Role::Info, format!("{panes}p ")));
    alert(&mut short);

    for rung in [full, without_name, short] {
        if width_of(&rung) <= width {
            return rung;
        }
    }
    // Nothing fits; this is the shortest thing that is still true.
    let mut last = vec![span(Role::Info, format!(" {panes}p "))];
    alert(&mut last);
    last
}

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
fn blit(dst: &mut Frame, src: &Frame, at: LRect) {
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

fn put(dst: &mut Frame, x: u16, y: u16, cell: Cell) {
    if x < dst.cols && y < dst.rows {
        dst.cells[y as usize * dst.cols as usize + x as usize] = cell;
    }
}

fn draw_dividers(dst: &mut Frame, dividers: &[Divider]) {
    for d in dividers {
        let ch = match d.dir {
            Dir::Horizontal => '│',
            Dir::Vertical => '─',
        };
        for row in 0..d.rect.h {
            for col in 0..d.rect.w {
                let cell = Cell { ch, fg: DIVIDER_FG, bg: COLOR_RESET, mods: 0 };
                put(dst, d.rect.x + col, d.rect.y + row, cell);
            }
        }
    }
}

/// One-line status bar along the bottom, painted span by span.
fn draw_status(dst: &mut Frame, spans: &[Span]) {
    if dst.rows == 0 {
        return;
    }
    let y = dst.rows - 1;
    // Fill first: the bar is a continuous band across the width, so the part
    // past the text has to carry the background too.
    for x in 0..dst.cols {
        put(dst, x, y, Cell { ch: ' ', fg: INFO_FG, bg: BAR_BG, mods: 0 });
    }
    let mut x = 0u16;
    for span in spans {
        let (fg, bg, mods) = span.role.style();
        for ch in span.text.chars() {
            if x >= dst.cols {
                return;
            }
            put(dst, x, y, Cell { ch, fg, bg, mods });
            x += 1;
        }
    }
}

/// Mark the focused pane so it is obvious which one takes input.
fn highlight_focus(dst: &mut Frame, focus: LRect) {
    // A bold top-left corner marker is cheap and does not steal a row.
    if focus.w > 0 && focus.h > 0 {
        let idx = focus.y as usize * dst.cols as usize + focus.x as usize;
        if idx < dst.cells.len() {
            dst.cells[idx].mods |= MOD_BOLD;
        }
    }
}

/// The area available to panes: everything above the status bar.
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
    status: Option<&[Span]>,
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
        highlight_focus(&mut out, *r);
    }
    if let Some(spans) = status {
        draw_status(&mut out, spans);
    }
    out.cursor = cursor;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The text the bar shows, ignoring how it is coloured.
    fn plain(spans: &[Span]) -> String {
        spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// Three windows, on the second, showing two panes with the second one
    /// focused.
    fn at(width: usize) -> String {
        plain(&status_spans("work", 3, 1, 2, 2, false, width))
    }

    #[test]
    fn a_single_window_shows_no_window_list() {
        // The list would be a constant "0*", which tells you nothing.
        assert_eq!(plain(&status_spans("work", 1, 0, 1, 1, false, 80)), " work | 1 pane ");
    }

    /// "1 pane | pane 1" read as two numbers that disagreed. With one pane
    /// there is nothing else it could be focused on.
    #[test]
    fn one_pane_is_not_also_announced_as_the_focused_one() {
        let s = plain(&status_spans("work", 1, 0, 1, 1, false, 80));
        assert_eq!(s.matches("pane").count(), 1, "the bar says it twice: {s}");
    }

    #[test]
    fn several_windows_mark_the_current_one() {
        assert_eq!(plain(&status_spans("work", 3, 1, 2, 2, false, 80)),
                   " work | 0 1* 2 | 2 panes | pane 2 ");
    }

    /// The focused pane is a POSITION in this window. The caller passing a
    /// `PaneId` is what produced "2 panes | pane 7" - a number with nothing
    /// on screen to match it.
    #[test]
    fn the_focused_pane_is_numbered_within_the_window() {
        let s = plain(&status_spans("work", 1, 0, 3, 3, false, 80));
        assert!(s.ends_with("3 panes | pane 3 "), "{s}");
    }

    /// A focus that cannot be placed is left out rather than shown as pane 0.
    #[test]
    fn an_unplaceable_focus_is_simply_not_shown() {
        let s = plain(&status_spans("work", 1, 0, 2, 0, false, 80));
        assert_eq!(s, " work | 2 panes ");
    }

    /// The window you are on is the one thing on the bar you must be able to
    /// find at a glance, so it is the only window drawn in its own colours.
    #[test]
    fn only_the_current_window_gets_the_active_role() {
        let spans = status_spans("work", 3, 1, 2, 2, false, 80);
        let active: Vec<&str> = spans
            .iter()
            .filter(|s| s.role == Role::ActiveWindow)
            .map(|s| s.text.as_str())
            .collect();
        assert_eq!(active, vec!["1* "], "{spans:?}");
    }

    #[test]
    fn scrolling_is_flagged() {
        let spans = status_spans("work", 1, 0, 1, 1, true, 80);
        assert!(plain(&spans).ends_with("| SCROLL "), "{spans:?}");
        assert!(spans.iter().any(|s| s.role == Role::Alert), "SCROLL must stand out: {spans:?}");
    }

    #[test]
    fn the_session_name_goes_first_when_it_will_not_fit() {
        let full = at(80);
        let narrower = at(full.chars().count() - 1);
        assert!(!narrower.contains("work"), "the name should have been dropped: {narrower}");
        assert!(narrower.contains("0 1* 2"), "the window list must survive: {narrower}");
        assert!(narrower.contains("2 panes"), "{narrower}");
    }

    #[test]
    fn the_window_position_outlives_the_pane_count() {
        // Getting lost between windows is worse than not knowing how many
        // panes the current one holds.
        let s = at(12);
        assert_eq!(s, " 1/3 | 2p ");
    }

    #[test]
    fn something_true_is_always_returned_however_narrow() {
        for width in 0..40 {
            let s = at(width);
            assert!(s.contains("2p") || s.contains("2 pane"), "width {width}: {s}");
        }
    }

    /// A wider terminal must never produce a shorter line: if it did, the
    /// ladder would be skipping a rung.
    #[test]
    fn the_ladder_never_gets_longer_as_the_terminal_narrows() {
        let widths: Vec<usize> = (0..60).map(|w| at(w).chars().count()).collect();
        for pair in widths.windows(2) {
            assert!(pair[0] <= pair[1], "a wider terminal produced a shorter line: {widths:?}");
        }
    }

    fn cell_at(f: &Frame, x: u16, y: u16) -> &Cell {
        &f.cells[y as usize * f.cols as usize + x as usize]
    }

    /// The bar is a band, not just coloured text: the empty right-hand end
    /// carries the background too, or the line looks like it stops mid-screen.
    #[test]
    fn the_status_bar_is_painted_across_the_whole_width() {
        let mut f = Frame::blank(40, 3);
        let spans = status_spans("w", 1, 0, 1, 1, false, 40);
        let text = width_of(&spans) as u16;
        draw_status(&mut f, &spans);
        for x in 0..40 {
            let c = cell_at(&f, x, 2);
            assert_ne!(c.bg, COLOR_RESET, "column {x} fell through to the pane background");
            if x >= text {
                assert_eq!(c.bg, BAR_BG, "the empty end of the bar: {c:?}");
            }
        }
    }

    /// Text past the last column must be dropped, not wrapped onto the panes.
    #[test]
    fn a_status_line_longer_than_the_screen_is_clipped() {
        let mut f = Frame::blank(6, 2);
        let long = vec![span(Role::Info, "0123456789")];
        draw_status(&mut f, &long);
        assert_eq!((0..6).map(|x| cell_at(&f, x, 1).ch).collect::<String>(), "012345");
        // The row above is still blank - nothing spilled upwards.
        assert!((0..6).all(|x| cell_at(&f, x, 0).ch == ' '));
    }

    #[test]
    fn dividers_are_drawn_in_their_own_colour() {
        let mut f = Frame::blank(9, 4);
        let dividers = [Divider { dir: Dir::Horizontal, rect: LRect::new(4, 0, 1, 3) }];
        draw_dividers(&mut f, &dividers);
        for y in 0..3 {
            let c = cell_at(&f, 4, y);
            assert_eq!(c.ch, '│');
            assert_eq!(c.fg, DIVIDER_FG, "a divider must not be the brightest thing on screen");
        }
    }
}
