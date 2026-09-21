//! Reading an application's own history, by scrolling the application.
//!
//! A full-screen program draws into the ALTERNATE screen, which has no
//! scrollback: the rows it has scrolled past are gone as far as the terminal
//! is concerned, and `capture --source recent` cannot reach them. Measured on
//! a real agent: 100 lines printed, 19 reachable. For a caller trying to read
//! an agent's answer, that is the whole feature failing.
//!
//! The application still HAS those rows - it redraws them when you scroll it.
//! So the only way to read them is to be a user: send the wheel, photograph
//! each screen, and put the viewport back. herdr does this in
//! `src/server/alt_screen_read.rs` and it is 996 lines, which is the honest
//! measure of how much care it takes.
//!
//! **This writes into somebody's live pane.** Everything here is built around
//! that: it refuses rather than guesses, it bails to the ordinary read on
//! anything it does not recognise, and it spends its last effort putting the
//! viewport back. The worst outcome it allows is a plain screen read; the
//! outcome it must never produce is a pane left scrolled somewhere the person
//! using it did not put it.
//!
//! The driving is separated from the deciding on purpose. `merge_up` and
//! `Harvester` know nothing about ptys, so the part where the bugs live is
//! unit-testable against a fake, without a TUI, a console or a timing window.

use std::time::{Duration, Instant};

/// What one scroll-up did to the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Merge {
    /// The screen moved down by this many rows, so that many are new.
    Advanced(usize),
    /// The same screen. Scrolling up changed nothing, so this is the top.
    Unchanged,
    /// Not a shifted version of the last screen at all. Something else
    /// redrew - a spinner at the top, a resize, the user typing - and the
    /// alignment this whole method rests on is gone.
    Unaligned,
}

/// Compare ignoring trailing blanks.
///
/// A redraw pads its rows to the width with spaces and an earlier one may
/// not have, which is not a difference anybody means.
fn same(a: &str, b: &str) -> bool {
    a.trim_end() == b.trim_end()
}

/// Fold a scrolled-up screen into the rows collected so far.
///
/// `next` should be `previous` moved DOWN the screen by some number of rows,
/// with that many older rows revealed at the top. Finding that number by
/// looking rather than by trusting the wheel is the point: how far one wheel
/// event scrolls is the application's decision, not ours, and it varies
/// between applications and with how close to the top they already are.
///
/// The smallest shift that fits wins. A screen of mostly blank lines can
/// align at several offsets, and the smallest is the one that keeps the most
/// text - taking a larger one would invent rows that were never revealed.
pub fn merge_up(history: &mut Vec<String>, previous: &[String], next: &[String]) -> Merge {
    if next.len() != previous.len() || next.is_empty() {
        return Merge::Unaligned;
    }
    let n = next.len();
    if next.iter().zip(previous).all(|(a, b)| same(a, b)) {
        return Merge::Unchanged;
    }
    // shift k: next[k..] is previous[..n-k].
    for k in 1..n {
        if next[k..].iter().zip(&previous[..n - k]).all(|(a, b)| same(a, b)) {
            history.splice(0..0, next[..k].iter().cloned());
            return Merge::Advanced(k);
        }
    }
    Merge::Unaligned
}

/// The pane, reduced to the three things the driver needs from it.
///
/// A trait so the state machine can be tested against a fake screen. The
/// real one is a pty and a vt100 parser; neither is needed to get the
/// alignment, the budgets or the restore wrong.
pub trait Screen {
    /// The visible rows, or `None` if this is no longer something we can
    /// harvest - it left the alternate screen, changed shape, or went away.
    fn rows(&self) -> Option<Vec<String>>;
    /// Send `n` wheel events. `false` if the pane will not take them, which
    /// ends the harvest: an application that is not listening to the mouse
    /// cannot be scrolled and must not be poked further.
    fn wheel(&self, up: bool, n: usize) -> bool;
    /// Milliseconds since the pane last produced any output.
    fn quiet_ms(&self) -> u64;
    /// A marker that CHANGES whenever the pane produces output.
    ///
    /// Needed beside `quiet_ms` because quiet is not evidence of having
    /// reacted. A pane that has been silent for a minute is still silent a
    /// millisecond after the wheel reaches it, so a settle that only waits
    /// for quiet returns at once and photographs the screen from before its
    /// own event - which is exactly what it did, and why the first harvest
    /// against a real application reported the top of its history
    /// immediately and read nothing.
    fn output_mark(&self) -> u64;
    /// Wait a little. Separated so a test does not sleep.
    fn pause(&self);
}

/// How much of a pane's own history to take, and how long to spend.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Rows wanted. Collection stops once there are this many.
    pub lines: usize,
    /// The whole harvest, scrolling up. Restoring gets its own budget on
    /// top, because giving up on putting the viewport back is not one of the
    /// things a budget is allowed to buy.
    pub total: Duration,
    pub restore: Duration,
    /// How long one redraw may take before the step is called done anyway.
    pub step: Duration,
    /// How quiet the pane must be for its redraw to count as finished.
    pub settle_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            lines: crate::protocol::RECENT_LINES_DEFAULT,
            total: Duration::from_secs(10),
            restore: Duration::from_secs(5),
            // Generous, because a step is several wheel events and a slow
            // application redraws once per event rather than coalescing
            // them. Cutting a step short is not a wrong answer - `merge_up`
            // measures whatever movement actually happened - but it is a
            // wasted round trip, and enough of those spend the whole budget
            // without reaching the top.
            step: Duration::from_millis(1_000),
            settle_ms: 40,
        }
    }
}

/// Wheel events in the first step, and in the bottom probe.
///
/// ONE, because the step size cannot be assumed - it has to be learned. How
/// far an application scrolls per event is its own decision: three lines is
/// a common default, one is not unusual, and five exists. Send three events
/// to something that moves five lines each and a twelve-row screen has
/// jumped fifteen - no overlap, nothing to align against, and the whole
/// method collapses on the first step. That is not hypothetical; it is what
/// the fake at `per_event = 5` did.
///
/// So the first step sends one, `merge_up` measures what that was worth, and
/// `calibrate` picks a step that stays inside a screen from then on.
const FIRST_STEP: usize = 1;

/// The most events one step may send, however little each one moves.
///
/// A ceiling on how wrong calibration can go, and on how long one redraw is
/// asked to be.
const MAX_STEP: usize = 16;

/// How many events to send per step, given what one event was worth.
///
/// Aims just under a screenful, so consecutive screens always share at least
/// one row - which is what `merge_up` aligns on. Without the overlap there
/// is no way to know how far the application actually went, and guessing is
/// how rows get duplicated or dropped.
fn calibrate(rows: usize, per_event: usize) -> usize {
    if per_event == 0 {
        return FIRST_STEP;
    }
    rows.saturating_sub(1).saturating_div(per_event).clamp(1, MAX_STEP)
}

/// The result of trying to read a pane's own history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harvest {
    /// Oldest first, including the screen that was visible to begin with.
    pub rows: Vec<String>,
    /// The top of the application's history was reached, so `rows` is all of
    /// it. False means the budget ran out or the alignment was lost first,
    /// and the caller is holding a window rather than a transcript.
    pub complete: bool,
    /// The viewport is back where it started. **False is the bad one**: the
    /// person using that pane is now looking somewhere they did not choose.
    pub restored: bool,
}

/// Scroll a pane back through its own history, collecting what it reveals.
///
/// `None` means "this cannot be harvested, use the ordinary read" - which is
/// every case where guessing would be worse than declining: a pane that is
/// not on the alternate screen, one that will not take the wheel, one that
/// was already scrolled somewhere by the person using it.
pub fn harvest(s: &impl Screen, limits: Limits) -> Option<Harvest> {
    let initial = s.rows()?;
    if initial.is_empty() {
        return None;
    }

    // Probe the bottom BEFORE touching anything, by scrolling DOWN.
    //
    // If the screen moves, this pane was not at the bottom - somebody is
    // reading it, scrolled back. Harvesting would drag their view to the top
    // and hand it back, and even a perfect restore would flicker through
    // their screen. So: undo the probe and decline. herdr probes the same
    // way and for the same reason.
    if !s.wheel(false, FIRST_STEP) {
        return None;
    }
    settle(s, &limits);
    let probed = s.rows()?;
    if !rows_match(&probed, &initial) {
        // Put their view back and leave.
        s.wheel(true, FIRST_STEP);
        settle(s, &limits);
        return None;
    }

    let mut history = initial.clone();
    let mut previous = initial.clone();
    let mut ups = 0usize;
    let mut complete = false;
    let mut aligned = true;
    let mut step = FIRST_STEP;
    let mut calibrated = false;
    let started = Instant::now();

    while history.len() < limits.lines {
        if started.elapsed() >= limits.total {
            break;
        }
        if !s.wheel(true, step) {
            break;
        }
        ups += step;
        settle(s, &limits);
        let Some(next) = s.rows() else {
            // The pane stopped being harvestable underneath us. Still put
            // the viewport back - that obligation does not depend on
            // getting an answer.
            aligned = false;
            break;
        };
        match merge_up(&mut history, &previous, &next) {
            Merge::Advanced(k) => {
                if !calibrated {
                    // What one event was worth, now that it has been seen.
                    step = calibrate(initial.len(), k / step.max(1));
                    calibrated = true;
                }
                previous = next;
            }
            Merge::Unchanged => {
                complete = true;
                break;
            }
            Merge::Unaligned => {
                // Something other than our scrolling redrew the screen. Every
                // row from here is suspect, so stop and keep what is already
                // known good.
                //
                // On the FIRST step this means something else: one event
                // moved further than a whole screen, so no two screens will
                // ever overlap and this application cannot be read this way
                // at all. Same handling - stop, restore - but worth knowing
                // that it is a property of the application, not a glitch.
                aligned = false;
                break;
            }
        }
    }

    let restored = restore(s, &initial, ups, &limits);
    // Keep the NEWEST rows, not the oldest. Collection works backwards from
    // the bottom, so the last step usually overshoots the asked-for count -
    // and `truncate` would drop the far end of the overshoot, which is the
    // most recent output and the part a caller reading an answer wants
    // most. `-n` means "the last N lines" everywhere else in zmux.
    if history.len() > limits.lines {
        history.drain(..history.len() - limits.lines);
    }
    Some(Harvest { rows: history, complete: complete && aligned, restored })
}

/// Put the viewport back, and say whether it got there.
///
/// Sends back exactly what was sent up, then checks, then keeps nudging
/// while the budget lasts. Counting is not enough on its own: the last step
/// up may have hit the top and scrolled less than it was asked to, so the
/// same count coming down overshoots - which is harmless, because the bottom
/// clamps - while an application that scrolled MORE than asked would come up
/// short, which is not. So it verifies rather than trusting the arithmetic.
fn restore(s: &impl Screen, initial: &[String], ups: usize, limits: &Limits) -> bool {
    if ups == 0 {
        return true;
    }
    let started = Instant::now();
    s.wheel(false, ups);
    settle(s, limits);
    while started.elapsed() < limits.restore {
        match s.rows() {
            Some(now) if rows_match(&now, initial) => return true,
            // Unreadable now: nothing useful left to compare against, and
            // continuing to send wheel events into something we cannot see
            // is exactly the poking this module exists to avoid.
            None => return false,
            Some(_) => {}
        }
        // Coarse on the way back. Overshooting down is free - the bottom
        // clamps - and the only thing that matters here is arriving.
        if !s.wheel(false, MAX_STEP) {
            return false;
        }
        settle(s, limits);
    }
    false
}

fn rows_match(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y))
}

/// Wait for the redraw our wheel event provoked to start, and then finish.
///
/// Both halves are load-bearing, and the first one was missing.
///
/// **Started**, because quiet says nothing on its own: an application that
/// has been idle stays idle for the moment after the wheel reaches it, and
/// a settle that only waited for quiet returned instantly and read the
/// screen from before its own event. Every step then looked `Unchanged` and
/// the harvest concluded it was already at the top.
///
/// **Finished**, because half a redraw is not a screen - the alignment would
/// see a torn frame and call it unaligned.
///
/// The step timeout backs both up. An application that redraws continuously,
/// such as a spinner or a clock, is never quiet, and waiting for it to be
/// would hang the harvest rather than end it.
fn settle(s: &impl Screen, limits: &Limits) {
    let deadline = Instant::now() + limits.step;
    let before = s.output_mark();
    while s.output_mark() == before && Instant::now() < deadline {
        s.pause();
    }
    while s.quiet_ms() < limits.settle_ms && Instant::now() < deadline {
        s.pause();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    fn rows(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_screen_that_did_not_move_is_the_top() {
        let mut h = rows(&["a", "b"]);
        assert_eq!(merge_up(&mut h, &rows(&["a", "b"]), &rows(&["a", "b"])), Merge::Unchanged);
        assert_eq!(h, rows(&["a", "b"]), "nothing moved, so nothing is new");
    }

    /// Trailing spaces are a redraw padding its rows, not a change.
    #[test]
    fn padding_is_not_a_difference() {
        let mut h = rows(&["a", "b"]);
        let m = merge_up(&mut h, &rows(&["a", "b"]), &rows(&["a   ", "b "]));
        assert_eq!(m, Merge::Unchanged);
    }

    #[test]
    fn the_rows_a_scroll_revealed_are_the_ones_kept() {
        // Screen was [c d e]; scrolling up by two shows [a b c].
        let mut h = rows(&["c", "d", "e"]);
        let m = merge_up(&mut h, &rows(&["c", "d", "e"]), &rows(&["a", "b", "c"]));
        assert_eq!(m, Merge::Advanced(2));
        assert_eq!(h, rows(&["a", "b", "c", "d", "e"]), "the overlap was duplicated or lost");
    }

    /// How far one wheel event scrolls is the application's business, so the
    /// shift is MEASURED. Trusting a constant would duplicate or drop rows
    /// on every application that disagreed with it.
    #[test]
    fn any_shift_is_found_rather_than_assumed() {
        for k in 1..5usize {
            let before: Vec<String> = (0..5).map(|i| format!("line{}", i + k)).collect();
            let after: Vec<String> = (0..5).map(|i| format!("line{i}")).collect();
            let mut h = before.clone();
            assert_eq!(merge_up(&mut h, &before, &after), Merge::Advanced(k), "shift {k}");
            assert_eq!(h.len(), 5 + k);
            assert_eq!(h[0], "line0");
        }
    }

    /// A screen that is not a shifted version of the last one means
    /// something else redrew, and every row after it is suspect.
    #[test]
    fn a_screen_that_is_not_a_shift_of_the_last_is_refused() {
        let mut h = rows(&["c", "d", "e"]);
        let m = merge_up(&mut h, &rows(&["c", "d", "e"]), &rows(&["x", "y", "z"]));
        assert_eq!(m, Merge::Unaligned);
        assert_eq!(h, rows(&["c", "d", "e"]), "an unaligned screen was taken anyway");
    }

    #[test]
    fn a_screen_that_changed_shape_is_refused() {
        let mut h = rows(&["a", "b"]);
        assert_eq!(merge_up(&mut h, &rows(&["a", "b"]), &rows(&["a"])), Merge::Unaligned);
    }

    /// A blank-heavy screen aligns at more than one offset. The smallest is
    /// the one that does not invent rows the application never revealed.
    #[test]
    fn the_smallest_alignment_wins_when_several_fit() {
        let mut h = rows(&["", "", "x"]);
        let m = merge_up(&mut h, &rows(&["", "", "x"]), &rows(&["", "", ""]));
        assert_eq!(m, Merge::Advanced(1), "a larger shift would invent blank rows");
    }

    /// A fake application with its own history, scrolled by a wheel.
    struct Fake {
        /// Oldest first.
        all: Vec<String>,
        rows: usize,
        /// Rows from the TOP that are currently off screen.
        top: RefCell<usize>,
        /// How far one wheel event moves this application.
        per_event: usize,
        /// Refuse the wheel entirely, like a program not asking for a mouse.
        deaf: bool,
        /// Redraw something unrelated on the Nth read, losing alignment.
        glitch_at: Option<usize>,
        reads: RefCell<usize>,
        wheels: RefCell<usize>,
    }

    impl Fake {
        fn new(lines: usize, rows: usize) -> Self {
            Self {
                all: (0..lines).map(|i| format!("line{i}")).collect(),
                rows,
                top: RefCell::new(lines.saturating_sub(rows)),
                per_event: 1,
                deaf: false,
                glitch_at: None,
                reads: RefCell::new(0),
                wheels: RefCell::new(0),
            }
        }
        fn bottom(&self) -> usize {
            self.all.len().saturating_sub(self.rows)
        }
    }

    impl Screen for Fake {
        fn rows(&self) -> Option<Vec<String>> {
            let n = { let mut r = self.reads.borrow_mut(); *r += 1; *r };
            if self.glitch_at == Some(n) {
                return Some(vec!["SOMETHING ELSE".into(); self.rows]);
            }
            let top = *self.top.borrow();
            Some(self.all[top..(top + self.rows).min(self.all.len())].to_vec())
        }
        fn wheel(&self, up: bool, n: usize) -> bool {
            if self.deaf {
                return false;
            }
            *self.wheels.borrow_mut() += n;
            let mut top = self.top.borrow_mut();
            let delta = n * self.per_event;
            *top = if up { top.saturating_sub(delta) } else { (*top + delta).min(self.bottom()) };
            true
        }
        fn quiet_ms(&self) -> u64 {
            1_000
        }
        fn output_mark(&self) -> u64 {
            // The fake redraws synchronously inside `wheel`, so by the time
            // anyone asks, the redraw this step provoked has happened.
            *self.wheels.borrow() as u64
        }
        fn pause(&self) {}
    }

    fn limits(lines: usize) -> Limits {
        Limits { lines, ..Default::default() }
    }

    #[test]
    fn a_whole_history_comes_back_in_order_and_the_view_goes_home() {
        let f = Fake::new(40, 10);
        let h = harvest(&f, limits(100)).expect("a scrollable pane is harvestable");
        assert!(h.complete, "the top was never reached");
        assert!(h.restored, "the viewport was left somewhere else");
        assert_eq!(h.rows.len(), 40, "rows were duplicated or lost: {:?}", h.rows.len());
        assert_eq!(h.rows[0], "line0");
        assert_eq!(h.rows[39], "line39");
        assert_eq!(*f.top.borrow(), f.bottom(), "the fake was left scrolled");
    }

    /// The line count is a bound on what comes back, not a suggestion.
    #[test]
    fn asking_for_fewer_lines_stops_early_but_still_goes_home() {
        let f = Fake::new(200, 10);
        let h = harvest(&f, limits(25)).expect("harvestable");
        assert_eq!(h.rows.len(), 25);
        assert!(!h.complete, "it claimed the whole history after stopping early");
        assert!(h.restored, "stopping early is no excuse for leaving the view moved");
        assert_eq!(*f.top.borrow(), f.bottom(), "the fake was left scrolled");
    }

    /// However far the application scrolls per event, the rows come back
    /// once each and in order.
    #[test]
    fn an_application_that_scrolls_in_bigger_jumps_is_still_read_correctly() {
        for per_event in [1usize, 2, 3, 5] {
            let mut f = Fake::new(60, 12);
            f.per_event = per_event;
            let h = harvest(&f, limits(100)).unwrap_or_else(|| panic!("{per_event}"));
            let want: Vec<String> = (0..60).map(|i| format!("line{i}")).collect();
            assert_eq!(h.rows, want, "per_event {per_event} duplicated or dropped rows");
            assert!(h.restored, "per_event {per_event} left the view moved");
        }
    }

    /// `-n` means the LAST n lines here as everywhere else. Collection runs
    /// backwards from the bottom and overshoots, and the overshoot is the
    /// oldest end - so trimming the wrong end throws away the most recent
    /// output, which is the part somebody reading an answer came for.
    #[test]
    fn asking_for_fewer_lines_keeps_the_newest_of_them() {
        let f = Fake::new(200, 10);
        let h = harvest(&f, limits(25)).expect("harvestable");
        assert_eq!(h.rows.len(), 25);
        assert_eq!(h.rows.last().map(String::as_str), Some("line199"), "{:?}", h.rows);
        assert_eq!(h.rows[0], "line175");
    }

    /// An application that moves more than a whole screen on ONE event can
    /// never show two overlapping screens, so there is nothing to align and
    /// no way to know what it skipped. Calibration cannot rescue that - one
    /// event is already the smallest step there is - so it declines to
    /// invent the rows and hands back what it could see.
    #[test]
    fn an_application_that_jumps_more_than_a_screen_is_not_guessed_at() {
        let mut f = Fake::new(200, 5);
        f.per_event = 10;
        let h = harvest(&f, limits(100)).expect("harvestable");
        assert!(!h.complete, "it claimed a whole history it could not have seen");
        assert!(h.restored, "the view was left moved");
        assert_eq!(h.rows.len(), 5, "rows appeared that no screen ever showed: {:?}", h.rows);
        assert_eq!(*f.top.borrow(), f.bottom(), "the fake was left scrolled");
    }

    /// A program that never asked for the mouse cannot be scrolled, and must
    /// not be poked at to find that out twice.
    #[test]
    fn a_pane_that_will_not_take_the_wheel_is_declined() {
        let mut f = Fake::new(40, 10);
        f.deaf = true;
        assert_eq!(harvest(&f, limits(100)), None);
        assert_eq!(*f.wheels.borrow(), 0, "a deaf pane was sent wheel events anyway");
    }

    /// Somebody is reading that pane, scrolled back. Harvesting would drag
    /// their view to the top; declining costs them nothing.
    #[test]
    fn a_pane_somebody_has_scrolled_is_left_alone() {
        let f = Fake::new(40, 10);
        *f.top.borrow_mut() = 5;
        assert_eq!(harvest(&f, limits(100)), None, "a scrolled pane was harvested anyway");
        assert_eq!(*f.top.borrow(), 5, "the probe moved somebody's view and left it there");
    }

    /// Something else redrew mid-harvest. Keep what was already good, stop,
    /// and above all put the view back.
    #[test]
    fn losing_the_alignment_stops_the_harvest_rather_than_inventing_rows() {
        let mut f = Fake::new(60, 10);
        // 1 is the initial read, 2 the bottom probe, so 4 is two steps in.
        f.glitch_at = Some(4);
        let h = harvest(&f, limits(100)).expect("harvestable");
        assert!(!h.complete, "a glitched harvest claimed to be the whole history");
        assert!(h.restored, "the view was abandoned after a glitch");
        assert!(
            !h.rows.iter().any(|r| r.contains("SOMETHING ELSE")),
            "the unaligned screen was kept: {:?}",
            h.rows
        );
    }

    /// The budget is real, and running out of it is not an excuse for
    /// leaving somebody's pane scrolled.
    #[test]
    fn running_out_of_time_still_puts_the_view_back() {
        let f = Fake::new(100_000, 10);
        let l = Limits { lines: 90_000, total: Duration::from_millis(0), ..Default::default() };
        let h = harvest(&f, l).expect("harvestable");
        assert!(!h.complete);
        assert!(h.restored, "the budget ran out and the view was left moved");
        assert_eq!(*f.top.borrow(), f.bottom());
    }
}
