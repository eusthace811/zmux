//! Windows: several independent layouts inside one session.
//!
//! tmux's model, kept as small as it can usefully be. A window IS a `Layout` -
//! it carries no other state, so there is no window struct, no window id and
//! no second allocator to keep in step with `PaneId`. Windows are identified
//! by position, which is also what the status bar shows.
//!
//! Panes do NOT move here. They stay in the session's one pane map whichever
//! window displays them, so a background window's shell keeps running and its
//! output keeps being parsed into its own `vt100` grid. Switching windows is
//! therefore only a change of which layout gets composited - no pane is
//! suspended, started or restarted.

use serde::{Deserialize, Serialize};

use crate::layout::{Layout, PaneId};

/// What removing a pane did to the window holding it.
///
/// Three outcomes need different handling and a bool cannot carry them: a
/// closed window is not a closed session, and neither is a pane that had
/// already gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// Removed; its window still has other panes.
    Pane,
    /// It was the window's last pane, so the window went with it.
    Window,
    /// The session's only pane, in its only window. Nothing was removed -
    /// there would be nothing left to show.
    LastPane,
    /// No window holds that pane. Usually a stale exit event for a pane that
    /// was already closed by hand.
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Windows {
    list: Vec<Layout>,
    current: usize,
}

impl Windows {
    pub fn new(first: Layout) -> Self {
        Self { list: vec![first], current: 0 }
    }

    /// Rebuild from disk, repairing anything the file got wrong.
    ///
    /// Returns `None` for a list with no windows: the caller must start fresh
    /// rather than be handed a session with nothing to display.
    fn from_saved(list: Vec<Layout>, current: usize) -> Option<Self> {
        if list.is_empty() {
            return None;
        }
        let current = if current < list.len() { current } else { 0 };
        Some(Self { list, current })
    }

    /// Validate a value that came from `serde`, which bypasses `from_saved`
    /// entirely.
    ///
    /// Two things only `from_saved` does: it advances the `PaneId` allocator
    /// past every restored id, and it repairs focus that names a missing pane.
    /// Deriving `Deserialize` on a struct holding `Layout`s skips both, so a
    /// restored session would hand out pane ids it was already using - the bug
    /// DESIGN.md §2.2 exists to prevent. An out-of-range `current` would also
    /// panic on first index.
    pub fn repaired(self) -> Option<Self> {
        // A pane id in two windows is not repairable. Restore spawns a shell
        // per leaf and keys the pane map by id, so the second spawn would
        // overwrite the first and leave a live shell with nothing owning it -
        // and `window_of` would only ever find one of the two. Unreachable
        // through the API; a hand-edited or corrupted file can say it.
        let mut seen = std::collections::HashSet::new();
        if !self.panes().into_iter().all(|id| seen.insert(id)) {
            return None;
        }
        let current = self.current;
        let list: Vec<Layout> = self
            .list
            .into_iter()
            .map(Layout::repaired)
            .collect();
        Self::from_saved(list, current)
    }

    pub fn current(&self) -> &Layout {
        &self.list[self.current]
    }

    pub fn current_mut(&mut self) -> &mut Layout {
        let i = self.current;
        &mut self.list[i]
    }

    pub fn index(&self) -> usize {
        self.current
    }

    pub fn len(&self) -> usize {
        self.list.len()
    }

    pub fn is_empty(&self) -> bool {
        false // there is always at least one window
    }

    pub fn all(&self) -> &[Layout] {
        &self.list
    }

    /// Every pane in every window. This is the session's real pane count, and
    /// what `MAX_PANES` has to bound - the ptys of a background window cost
    /// exactly as much as those of the visible one.
    pub fn panes(&self) -> Vec<PaneId> {
        self.list.iter().flat_map(|l| l.panes()).collect()
    }

    fn window_of(&self, id: PaneId) -> Option<usize> {
        self.list.iter().position(|l| l.panes().contains(&id))
    }

    /// Append a window and switch to it. Returns its index.
    ///
    /// Appending puts it at the lowest unused index, which is where tmux's
    /// `new-window` lands when indices are contiguous - and ours always are,
    /// because closing a window renumbers the rest.
    pub fn push(&mut self, layout: Layout) -> usize {
        self.list.push(layout);
        self.current = self.list.len() - 1;
        self.current
    }

    /// Switch to the next window, wrapping. False when there is only one.
    pub fn select_next(&mut self) -> bool {
        if self.list.len() <= 1 {
            return false;
        }
        self.current = (self.current + 1) % self.list.len();
        true
    }

    /// Switch to the previous window, wrapping. False when there is only one.
    pub fn select_previous(&mut self) -> bool {
        if self.list.len() <= 1 {
            return false;
        }
        self.current = (self.current + self.list.len() - 1) % self.list.len();
        true
    }

    /// Remove `id` from whichever window holds it, dropping that window if it
    /// was its last pane.
    ///
    /// The caller owns the pty and must close it; this only edits the tree.
    pub fn close_pane(&mut self, id: PaneId) -> Closed {
        let Some(wi) = self.window_of(id) else { return Closed::NotFound };
        if self.list[wi].close(id) {
            return Closed::Pane;
        }
        // close() refused, so `id` is that window's only pane.
        if self.list.len() <= 1 {
            return Closed::LastPane;
        }
        self.list.remove(wi);
        // Removing a window renumbers everything after it. Stay on the same
        // window when one BEFORE us goes, and fall onto the next one (or the
        // new last) when our own does - never off the end.
        if wi < self.current {
            self.current -= 1;
        } else if wi == self.current {
            self.current = self.current.min(self.list.len() - 1);
        }
        Closed::Window
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Dir, Rect};

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    fn one(id: u32) -> Layout {
        Layout::new(PaneId(id))
    }

    #[test]
    fn a_new_session_has_exactly_one_window() {
        let w = Windows::new(one(1));
        assert_eq!(w.len(), 1);
        assert_eq!(w.index(), 0);
        assert_eq!(w.current().focus(), PaneId(1));
    }

    #[test]
    fn pushing_appends_and_switches() {
        let mut w = Windows::new(one(1));
        assert_eq!(w.push(one(2)), 1);
        assert_eq!(w.index(), 1, "a new window takes focus");
        assert_eq!(w.current().focus(), PaneId(2));
    }

    #[test]
    fn next_and_prev_wrap() {
        let mut w = Windows::new(one(1));
        w.push(one(2));
        w.push(one(3)); // on window 2
        assert!(w.select_next());
        assert_eq!(w.index(), 0, "next from the last wraps to the first");
        assert!(w.select_previous());
        assert_eq!(w.index(), 2, "prev from the first wraps to the last");
    }

    #[test]
    fn cycling_one_window_is_a_no_op() {
        let mut w = Windows::new(one(1));
        assert!(!w.select_next(), "nothing to cycle to");
        assert!(!w.select_previous());
        assert_eq!(w.index(), 0);
    }

    #[test]
    fn panes_spans_every_window() {
        let mut w = Windows::new(one(1));
        w.current_mut().split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        w.push(one(3));
        let mut panes = w.panes();
        panes.sort();
        assert_eq!(panes, vec![PaneId(1), PaneId(2), PaneId(3)],
                   "a background window's panes still cost ptys and must be counted");
    }

    #[test]
    fn closing_a_pane_leaves_its_window_when_others_remain() {
        let mut w = Windows::new(one(1));
        w.current_mut().split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        assert_eq!(w.close_pane(PaneId(1)), Closed::Pane);
        assert_eq!(w.len(), 1);
        assert_eq!(w.current().panes(), vec![PaneId(2)]);
    }

    #[test]
    fn closing_the_last_pane_of_a_window_closes_the_window() {
        let mut w = Windows::new(one(1));
        w.push(one(2));
        assert_eq!(w.close_pane(PaneId(2)), Closed::Window);
        assert_eq!(w.len(), 1);
        assert_eq!(w.index(), 0);
        assert_eq!(w.current().focus(), PaneId(1));
    }

    #[test]
    fn the_final_pane_of_the_final_window_is_refused() {
        let mut w = Windows::new(one(1));
        assert_eq!(w.close_pane(PaneId(1)), Closed::LastPane);
        assert_eq!(w.len(), 1, "nothing may be removed - the session needs something to show");
        assert_eq!(w.current().panes(), vec![PaneId(1)]);
    }

    #[test]
    fn a_pane_from_no_window_is_reported_rather_than_guessed_at() {
        let mut w = Windows::new(one(1));
        assert_eq!(w.close_pane(PaneId(99)), Closed::NotFound);
        assert_eq!(w.len(), 1);
    }

    /// A pane can exit in a window you are not looking at. That must not move
    /// you somewhere else.
    #[test]
    fn closing_a_later_window_does_not_move_the_current_one() {
        let mut w = Windows::new(one(1));
        w.push(one(2));
        w.push(one(3));
        w.select_next(); // wrap from the third window round to the first
        assert_eq!(w.close_pane(PaneId(3)), Closed::Window);
        assert_eq!(w.index(), 0, "closing window 2 must not move us");
        assert_eq!(w.current().focus(), PaneId(1));
    }

    /// Windows before the current one renumber it downwards; staying on the
    /// same INDEX would silently switch which window you are looking at.
    #[test]
    fn closing_an_earlier_window_keeps_us_on_the_same_window() {
        let mut w = Windows::new(one(1));
        w.push(one(2));
        w.push(one(3)); // on window 2
        assert_eq!(w.close_pane(PaneId(1)), Closed::Window);
        assert_eq!(w.index(), 1, "we were on the third window; it is now the second");
        assert_eq!(w.current().focus(), PaneId(3), "and it is still the same window");
    }

    #[test]
    fn closing_the_last_window_in_the_list_falls_back_one() {
        let mut w = Windows::new(one(1));
        w.push(one(2)); // on window 1, the last
        assert_eq!(w.close_pane(PaneId(2)), Closed::Window);
        assert_eq!(w.index(), 0, "focus cannot be left off the end of the list");
    }

    #[test]
    fn closing_the_current_window_lands_on_the_next() {
        let mut w = Windows::new(one(1));
        w.push(one(2));
        w.push(one(3));
        w.select_previous();
        assert_eq!(w.close_pane(PaneId(2)), Closed::Window);
        assert_eq!(w.index(), 1, "the window that was after us takes our index");
        assert_eq!(w.current().focus(), PaneId(3));
    }

    #[test]
    fn restoring_repairs_a_current_index_that_is_out_of_range() {
        let w = Windows::from_saved(vec![one(1), one(2)], 9).expect("two windows is restorable");
        assert_eq!(w.index(), 0, "an impossible index must not panic on first use");
    }

    #[test]
    fn restoring_no_windows_at_all_is_refused() {
        assert!(Windows::from_saved(Vec::new(), 0).is_none(),
                "a session with nothing to display must start fresh instead");
    }

    /// Deriving `Deserialize` skips `Layout::from_saved`, so nothing advances
    /// the pane allocator - and a restored session then hands out ids it is
    /// already using. `repaired()` is what closes that.
    #[test]
    fn restoring_windows_advances_the_pane_allocator() {
        let saved = Windows::new(one(50_000));
        let json = serde_json::to_string(&saved).unwrap();
        let back: Windows = serde_json::from_str(&json).unwrap();
        let back = back.repaired().expect("restorable");
        assert_eq!(back.panes(), vec![PaneId(50_000)]);
        let fresh = PaneId::alloc();
        assert!(fresh.0 > 50_000, "alloc() reused a restored id: {fresh:?}");
    }

    #[test]
    fn a_pane_in_two_windows_is_refused_rather_than_spawned_twice() {
        // Restore spawns a shell per leaf; the duplicate would overwrite the
        // first in the pane map and orphan its ConPTY.
        let json = r#"{"list":[{"root":{"Leaf":4},"focus":4},
                               {"root":{"Leaf":4},"focus":4}],"current":0}"#;
        let w: Windows = serde_json::from_str(json).unwrap();
        assert!(w.repaired().is_none(), "a duplicated pane id must start the session fresh");
    }

    #[test]
    fn windows_survive_a_serde_round_trip() {
        let mut w = Windows::new(one(1));
        w.current_mut().split(Dir::Vertical, area(), PaneId(2)).unwrap();
        w.push(one(3));
        w.select_previous();
        let json = serde_json::to_string(&w).unwrap();
        let back: Windows = serde_json::from_str(&json).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.index(), 0);
        assert_eq!(back.panes().len(), 3);
    }
}
