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
/// A running pane. Green rather than the alert orange: busy is normal, and
/// the bar should not look like something is wrong every time a build runs.
const BUSY_FG: u32 = idx(114);
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
    /// The pane you are on. Deliberately NOT `ActiveWindow`: windows and panes
    /// are both lists of small numbers on one line, and giving them the same
    /// highlight makes the bar unreadable.
    ActivePane,
    /// A pane that is running something.
    Busy,
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
            Role::ActivePane => (ACTIVE_BG, BAR_BG, MOD_BOLD),
            Role::Busy => (BUSY_FG, BAR_BG, MOD_BOLD),
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

/// Everything the bar draws, gathered by the session in one pass.
pub struct BarState<'a> {
    pub name: &'a str,
    /// One label per window, in order: what is running in it, or `None` for
    /// nothing. Named after the focused pane's command, as tmux does.
    pub windows: &'a [Option<String>],
    /// 0-based index of the window being shown.
    pub current: usize,
    /// One flag per pane of the CURRENT window, in the order the bar counts
    /// them: is it running something.
    pub busy: &'a [bool],
    /// POSITION of the focused pane in this window, counting from 1, or 0 when
    /// it cannot be placed. Deliberately not the `PaneId`: that is an
    /// allocator counter, so after a few splits and closes the bar read
    /// "2 panes | pane 7" and invited you to look for panes 3 to 6.
    pub focus: usize,
    /// What to call a window with nothing running - the shell's own name.
    pub idle: &'a str,
    pub scrolled: bool,
}

/// One thing on the bar: a window entry, a pane entry, a divider. Usually one
/// span, but a pane that is running something is its number and a dot in two
/// different colours, and nothing may come between them.
type Token = Vec<Span>;

/// What kind of thing a token is, as far as SPACING is concerned.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Drawn on a background of its own, so it is a block with edges.
    Lit,
    /// Punctuation between sections. It is meant to sit between things.
    Divider,
    /// Ordinary text on the bar's own background.
    Item,
}

fn kind(role: Role) -> Kind {
    match role {
        Role::Name | Role::ActiveWindow => Kind::Lit,
        Role::Sep => Kind::Divider,
        _ => Kind::Item,
    }
}

/// Lay tokens out, spacing them by what they ARE.
///
/// A highlight has EDGES, and that makes it need two different things at
/// once. Cover "work " but not the space in front of it and the text sits
/// hard against the left edge of its block with a column of air inside the
/// right: the label looks displaced though it never moved. So a block owns a
/// column of its OWN colour on each side. But then that coloured column is
/// all that stands between the block and the next entry's text, and the
/// block reads as stuck to a window it has nothing to do with. So a block
/// also keeps a column of the bar's own colour between itself and any
/// neighbouring TEXT - a moat, one column wide, outside the padding.
///
/// Not against a divider, though, and not against the ends of the line: `|`
/// is there to separate things and does not need separating from, and the
/// edge of the screen is not something the highlight could be confused with.
/// Those are the two places a moat would only read as slack.
fn assemble(tokens: Vec<Token>) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut prev: Option<Kind> = None;
    for mut t in tokens.into_iter().filter(|t| !t.is_empty()) {
        let k = kind(t[0].role);
        if gap(prev, Some(k)) {
            out.push(span(Role::Sep, " "));
        }
        if k == Kind::Lit {
            let last = t.len() - 1;
            t[0].text.insert(0, ' ');
            t[last].text.push(' ');
        }
        out.extend(t);
        prev = Some(k);
    }
    if gap(prev, None) {
        out.push(span(Role::Sep, " "));
    }
    out
}

/// Is a column of the bar's own colour wanted between these two? `None` is
/// an end of the line.
///
/// Everything is one column apart, and a block's own padding IS that column
/// where the thing on the other side is a divider or the end of the line.
/// Everywhere else the padding is extra and the column is still wanted.
fn gap(prev: Option<Kind>, next: Option<Kind>) -> bool {
    // An empty bar has no sides to keep apart.
    if prev.is_none() && next.is_none() {
        return false;
    }
    // The two neighbours a block's own padding already separates it from.
    let soft = |k: Option<Kind>| matches!(k, None | Some(Kind::Divider));
    let lit = |k: Option<Kind>| k == Some(Kind::Lit);
    !((lit(prev) && soft(next)) || (soft(prev) && lit(next)))
}

/// Build the status line.
///
/// Pure, so the degradation ladder can be tested at every width without
/// starting a daemon and a shell - it used to be reachable only through a live
/// session, so only the widest rung was ever exercised.
pub fn status_spans(st: &BarState, width: usize) -> Vec<Span> {
    let panes = st.busy.len();

    // Windows, named after what is running in them. With ONE window the index
    // is noise - there is nothing to switch to and the digit is not a
    // shortcut worth showing - so only the label is drawn.
    let windows = |labelled: bool| -> Vec<Token> {
        let mut v: Vec<Token> = Vec::new();
        if st.windows.len() == 1 {
            if labelled {
                v.push(vec![span(Role::Info, label_of(&st.windows[0], st.idle))]);
            }
            return v;
        }
        for (i, w) in st.windows.iter().enumerate() {
            // The digit is also the key that jumps there: `C-b <digit>`.
            // The `*` is belt and braces - the colour says the same thing,
            // and this keeps it legible if one of them is lost.
            let (role, mark) = if i == st.current {
                (Role::ActiveWindow, "*")
            } else {
                (Role::Window, "")
            };
            let text = if labelled {
                format!("{i}:{}{mark}", label_of(w, st.idle))
            } else {
                format!("{i}{mark}")
            };
            v.push(vec![span(role, text)]);
        }
        v
    };

    // Panes: position, `*` if focused, `•` if running something.
    //
    // With one pane there is nothing to be focused BUT that pane, so its
    // number and star are pure noise - next to a count they read as two
    // numbers that disagreed. Whether it is BUSY is not noise, so that alone
    // survives.
    let panes_list = || -> Vec<Token> {
        let mut v: Vec<Token> = Vec::new();
        if panes == 1 {
            if st.busy[0] {
                v.push(vec![span(Role::Busy, "•")]);
            }
            return v;
        }
        for (i, busy) in st.busy.iter().enumerate() {
            let pos = i + 1;
            let focused = pos == st.focus;
            let role = if focused { Role::ActivePane } else { Role::Info };
            let mut t = vec![span(role, format!("{pos}{}", if focused { "*" } else { "" }))];
            if *busy {
                t.push(span(Role::Busy, "•"));
            }
            v.push(t);
        }
        v
    };

    // Sections are JOINED, not concatenated with their own separators. A
    // section that turns out to be empty - one idle pane draws nothing at all
    // - used to leave the "| " that introduced it dangling at the end.
    let join = |sections: Vec<Vec<Token>>| -> Vec<Span> {
        let mut tokens: Vec<Token> = Vec::new();
        for sec in sections.into_iter().filter(|s| !s.is_empty()) {
            if !tokens.is_empty() {
                tokens.push(vec![span(Role::Sep, "|")]);
            }
            tokens.extend(sec);
        }
        assemble(tokens)
    };
    let alert = || -> Vec<Token> {
        if st.scrolled { vec![vec![span(Role::Alert, "SCROLL")]] } else { Vec::new() }
    };
    let tail = |labelled: bool| -> Vec<Vec<Token>> {
        vec![windows(labelled), panes_list(), alert()]
    };

    // Drop the least useful parts first rather than letting the whole line be
    // truncated. On a narrow pane the old format showed a clipped session name
    // and nothing else - the pane count, which is what you actually need, fell
    // off the end. The labels go before the window list does: knowing WHICH
    // window you are on outranks knowing what is running in the others, and
    // dropping them lands exactly on the bar this had before labels existed.
    let mut with_name = vec![vec![vec![span(Role::Name, st.name)]]];
    with_name.extend(tail(true));
    let full = join(with_name);

    let without_name = join(tail(true));
    let without_labels = join(tail(false));

    let mut count: Vec<Token> = Vec::new();
    if st.windows.len() > 1 {
        count.push(vec![span(
            Role::ActiveWindow,
            format!("{}/{}", st.current, st.windows.len()),
        )]);
    }
    let mut counted: Vec<Token> = vec![vec![span(Role::Info, format!("{panes}p"))]];
    if st.busy.iter().any(|b| *b) {
        counted.push(vec![span(Role::Busy, "•")]);
    }
    let short = join(vec![count, counted, alert()]);

    for rung in [full, without_name, without_labels, short] {
        if width_of(&rung) <= width {
            return rung;
        }
    }
    // Nothing fits; this is the shortest thing that is still true.
    join(vec![vec![vec![span(Role::Info, format!("{panes}p"))]], alert()])
}

fn label_of<'a>(label: &'a Option<String>, idle: &'a str) -> &'a str {
    label.as_deref().unwrap_or(idle)
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
    Frame {
        cols,
        rows,
        cells,
        cursor,
        app_cursor: screen.application_cursor(),
        mouse: screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None,
        revision: 0,
    }
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
                // pane the keystrokes will reach. So does whether the client
                // should hold the mouse: a program in a pane you are not on
                // is not a reason to take selection away from you.
                out.app_cursor = f.app_cursor;
                out.mouse = f.mouse;
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

    fn lab(v: &[&str]) -> Vec<Option<String>> {
        v.iter().map(|s| (!s.is_empty()).then(|| s.to_string())).collect()
    }

    /// Three windows, on the second, showing two panes with the second one
    /// focused and the first one running something.
    fn at(width: usize) -> String {
        let w = lab(&["", "cargo", ""]);
        plain(&status_spans(
            &BarState {
                name: "work",
                windows: &w,
                current: 1,
                busy: &[true, false],
                focus: 2,
                idle: "bash",
                scrolled: false,
            },
            width,
        ))
    }

    /// Build a bar from a compact description, so a test reads as the thing
    /// it is checking rather than seven positional arguments.
    struct B {
        windows: Vec<Option<String>>,
        current: usize,
        busy: Vec<bool>,
        focus: usize,
        scrolled: bool,
    }

    impl B {
        fn new(windows: &[&str], busy: &[bool]) -> Self {
            Self {
                windows: lab(windows),
                current: 0,
                busy: busy.to_vec(),
                focus: 1,
                scrolled: false,
            }
        }
        fn at(&self, width: usize) -> Vec<Span> {
            status_spans(
                &BarState {
                    name: "work",
                    windows: &self.windows,
                    current: self.current,
                    busy: &self.busy,
                    focus: self.focus,
                    idle: "bash",
                    scrolled: self.scrolled,
                },
                width,
            )
        }
        fn text(&self) -> String {
            plain(&self.at(80))
        }
    }

    /// One window has no list worth drawing - the index is not a shortcut to
    /// anywhere - so the space goes to what is actually running.
    #[test]
    fn a_single_window_is_named_not_numbered() {
        let mut b = B::new(&["cargo"], &[false]);
        assert_eq!(b.text(), " work | cargo ");
        b.windows = lab(&[""]);
        assert_eq!(b.text(), " work | bash ", "an idle window falls back to the shell");
    }

    /// tmux names a window after what is running in it; so do we, using the
    /// focused pane's command.
    #[test]
    fn several_windows_are_named_after_what_runs_in_them() {
        let mut b = B::new(&["", "cargo", "vim"], &[false, false]);
        b.current = 1;
        b.focus = 2;
        assert_eq!(b.text(), " work | 0:bash  1:cargo*  2:vim | 1 2* ");
    }

    #[test]
    fn a_running_pane_is_marked() {
        let mut b = B::new(&["cargo"], &[true, false]);
        b.focus = 2;
        assert_eq!(b.text(), " work | cargo | 1• 2* ");
    }

    /// With one pane there is nothing else it could be focused on, so its
    /// number and star are noise. Whether it is BUSY is not.
    #[test]
    fn one_pane_shows_only_whether_it_is_busy() {
        assert_eq!(B::new(&["bash"], &[false]).text(), " work | bash ");
        assert_eq!(B::new(&["cargo"], &[true]).text(), " work | cargo | • ");
    }

    /// A focus that cannot be placed is left out rather than shown as pane 0.
    #[test]
    fn an_unplaceable_focus_is_simply_not_shown() {
        let mut b = B::new(&["bash"], &[false, false]);
        b.focus = 0;
        assert_eq!(b.text(), " work | bash | 1 2 ");
    }

    /// A highlight has EDGES, and that makes it need two things at once.
    ///
    /// It needs a column of its OWN colour on each side, or the text sits
    /// hard against the left edge of its block with a column of air inside
    /// the right and the label looks displaced though it never moved. And it
    /// needs a column of the BAR's colour outside that, or the block is
    /// touching the next window's name and reads as attached to it.
    #[test]
    fn a_highlight_is_padded_and_then_kept_off_its_neighbours() {
        let mut b = B::new(&["", "cargo", "vim"], &[true, false]);
        b.current = 1;
        b.focus = 2;
        let spans = b.at(80);
        let lit: Vec<&Span> = spans.iter().filter(|s| kind(s.role) == Kind::Lit).collect();
        assert_eq!(lit.len(), 2, "the name and the current window: {spans:?}");
        for s in &lit {
            assert!(s.text.starts_with(' '), "no column in front of {s:?}");
            assert!(s.text.ends_with(' '), "no column behind {s:?}");
        }
        // Two columns between the block and the text on either side of it:
        // one its own, one the bar's.
        assert!(b.text().contains("0:bash  1:cargo*  2:vim"), "{}", b.text());
    }

    /// The moat is for TEXT. `|` is already a separator and the edge of the
    /// screen is nothing the highlight could be confused with, so against
    /// those the block's own padding is the whole gap - otherwise the bar
    /// just looks slack at both ends of the name.
    #[test]
    fn a_highlight_does_not_keep_away_from_dividers_or_the_edge() {
        let b = B::new(&["", "x"], &[false]);
        assert!(b.text().starts_with(" work |"), "{}", b.text());
    }

    /// The window you are on is the one thing on the bar you must be able to
    /// find at a glance, so it is the only window drawn in its own colours.
    #[test]
    fn only_the_current_window_gets_the_active_role() {
        let mut b = B::new(&["", "cargo", ""], &[false, false]);
        b.current = 1;
        let spans = b.at(80);
        let active: Vec<&str> = spans
            .iter()
            .filter(|s| s.role == Role::ActiveWindow)
            .map(|s| s.text.as_str())
            .collect();
        // Padded on BOTH sides: see `padded`. The column in front of it is
        // the one that would have separated it from window 0 anyway, so the
        // bar is the same width - only the highlight moved.
        assert_eq!(active, vec![" 1:cargo* "], "{spans:?}");
    }

    /// Windows and panes are both runs of small numbers on one line. Give
    /// them the same highlight and the bar stops being readable at a glance.
    #[test]
    fn the_focused_pane_and_the_current_window_do_not_look_alike() {
        let mut b = B::new(&["", "x"], &[false, false]);
        b.current = 1;
        b.focus = 2;
        let spans = b.at(80);
        assert!(spans.iter().any(|s| s.role == Role::ActiveWindow), "{spans:?}");
        assert!(spans.iter().any(|s| s.role == Role::ActivePane), "{spans:?}");
        assert_ne!(Role::ActiveWindow.style(), Role::ActivePane.style());
    }

    #[test]
    fn scrolling_is_flagged() {
        let mut b = B::new(&["bash"], &[false]);
        b.scrolled = true;
        let spans = b.at(80);
        assert!(plain(&spans).ends_with("| SCROLL "), "{spans:?}");
        assert!(spans.iter().any(|s| s.role == Role::Alert), "SCROLL must stand out: {spans:?}");
    }

    #[test]
    fn the_session_name_goes_first_when_it_will_not_fit() {
        let full = at(80);
        let narrower = at(full.chars().count() - 1);
        assert!(!narrower.contains("work"), "the name should have been dropped: {narrower}");
        assert!(narrower.contains("cargo"), "the labels must survive one more rung: {narrower}");
    }

    /// Knowing WHICH window you are on outranks knowing what is running in the
    /// others, so the labels go before the list does - and what is left is
    /// exactly the bar this had before labels existed.
    #[test]
    fn the_labels_are_dropped_before_the_window_list_is() {
        let widths: Vec<usize> = (8..60).collect();
        let bars: Vec<String> = widths.iter().map(|w| at(*w)).collect();
        // Widest first: the NEXT rung down from the labelled one, not the
        // narrowest bar there is.
        let no_label =
            bars.iter().rev().find(|b| !b.contains("cargo")).expect("a rung without labels");
        assert!(no_label.contains("0  1*  2"), "the window list must outlive the labels: {no_label}");
    }

    #[test]
    fn the_window_position_outlives_the_pane_count() {
        // Getting lost between windows is worse than not knowing how many
        // panes the current one holds.
        assert_eq!(at(14), " 1/3 | 2p • ");
    }

    /// The one thing the narrow rungs must not quietly drop: a pane can be
    /// running a long build, and "is anything happening" is the whole point.
    #[test]
    fn a_busy_pane_is_still_flagged_on_the_short_rung() {
        assert!(at(14).contains('•'), "{}", at(14));
    }

    #[test]
    fn something_true_is_always_returned_however_narrow() {
        for width in 0..40 {
            let s = at(width);
            // Either the panes are listed, or they are counted. Never neither.
            assert!(s.contains("2*") || s.contains("2p"), "width {width}: {s}");
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
        let spans = B::new(&["bash"], &[false]).at(40);
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

