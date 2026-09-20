//! BSP tiling tree. Design follows herdr's `src/layout.rs` (Apache-2.0);
//! written fresh so the bugs catalogued in DESIGN.md §2.2 are fixed at the
//! source rather than inherited:
//!
//! - `PaneId::from_raw` advances the allocator, so restoring panes 1,2,3 and
//!   then allocating cannot hand back 1 again.
//! - The tree derives serde, because persistence needs it (Phase 4).
//! - Restoring validates that focus actually exists.
//! - Panes have a MINIMUM size; a small terminal cannot produce zero-width
//!   panes that then divide by zero.

use std::sync::atomic::{AtomicU32, Ordering};

use serde::{Deserialize, Serialize};

/// Smallest usable pane interior. Splits that cannot honour this are refused.
pub const MIN_COLS: u16 = 8;
pub const MIN_ROWS: u16 = 2;

static NEXT_PANE_ID: AtomicU32 = AtomicU32::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PaneId(pub u32);

impl PaneId {
    pub fn alloc() -> Self {
        Self(NEXT_PANE_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Rebuild a saved id AND keep the allocator ahead of it.
    ///
    /// herdr's version does not do this (`layout.rs:19-31`), so restoring a
    /// session and then splitting collides on ids.
    pub fn from_raw(id: u32) -> Self {
        // `id + 1` on a saved id of u32::MAX panics in debug and wraps in
        // release, after which `alloc` hands back ids already in use. A state
        // file can say any number; `Windows::repaired` refuses ids with no
        // room left above them, and this keeps the arithmetic itself safe.
        NEXT_PANE_ID.fetch_max(id.saturating_add(1), Ordering::Relaxed);
        Self(id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dir {
    /// Children sit side by side; the divider is vertical.
    Horizontal,
    /// Children sit stacked; the divider is horizontal.
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }
    fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    fn centre(&self) -> (u16, u16) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Node {
    Leaf(PaneId),
    Split { dir: Dir, ratio: f32, a: Box<Node>, b: Box<Node> },
}

impl Node {
    fn leaves(&self, out: &mut Vec<PaneId>) {
        match self {
            Node::Leaf(id) => out.push(*id),
            Node::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }
}

/// A divider between two panes, for drawing and for mouse-drag resizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divider {
    pub rect: Rect,
    pub dir: Dir,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    root: Node,
    focus: PaneId,
}

impl Layout {
    pub fn new(first: PaneId) -> Self {
        Self { root: Node::Leaf(first), focus: first }
    }

    pub fn focus(&self) -> PaneId {
        self.focus
    }

    pub fn panes(&self) -> Vec<PaneId> {
        let mut v = Vec::new();
        self.root.leaves(&mut v);
        v
    }

    pub fn len(&self) -> usize {
        self.panes().len()
    }

    pub fn is_empty(&self) -> bool {
        false // a layout always holds at least one leaf
    }

    pub fn set_focus(&mut self, id: PaneId) -> bool {
        if self.panes().contains(&id) {
            self.focus = id;
            true
        } else {
            false
        }
    }

    /// Re-validate a layout that came from `serde`, which skips `from_saved`.
    ///
    /// Consumes and moves the tree rather than cloning it, which is what a
    /// `root_snapshot()` round trip used to cost on every restored window.
    pub fn repaired(self) -> Self {
        Self::from_saved(self.root, self.focus)
    }

    /// Restore from a saved tree, repairing focus if it names a missing pane.
    pub fn from_saved(root: Node, focus: PaneId) -> Self {
        let mut l = Self { root, focus };
        let panes = l.panes();
        for id in &panes {
            PaneId::from_raw(id.0);
        }
        if !panes.contains(&focus) {
            l.focus = panes[0];
        }
        l
    }

    /// Split the focused pane, giving the new pane the second half.
    ///
    /// Returns `None` when the area cannot accommodate two panes; the caller
    /// must not create a pty for a pane that will not fit.
    pub fn split(&mut self, dir: Dir, area: Rect, new_id: PaneId) -> Option<PaneId> {
        let current = self.rect_of(self.focus, area)?;
        let fits = match dir {
            Dir::Horizontal => current.w > MIN_COLS * 2,
            Dir::Vertical => current.h > MIN_ROWS * 2,
        };
        if !fits {
            return None;
        }
        let focus = self.focus;
        replace_leaf(&mut self.root, focus, |leaf| Node::Split {
            dir,
            ratio: 0.5,
            a: Box::new(leaf),
            b: Box::new(Node::Leaf(new_id)),
        });
        self.focus = new_id;
        Some(new_id)
    }

    /// Remove a specific pane. Returns false when it is absent or is the last
    /// one (a window always keeps at least one pane).
    ///
    /// Focus only moves if the pane being removed held it. Callers used to
    /// emulate this by focusing the victim, closing, then restoring focus -
    /// which silently closed the WRONG pane whenever the victim had already
    /// gone. Removing by id has no such failure mode.
    pub fn close(&mut self, id: PaneId) -> bool {
        if self.len() <= 1 || !self.panes().contains(&id) {
            return false;
        }
        let Some(surviving) = remove_leaf(&mut self.root, id) else { return false };
        if self.focus == id {
            self.focus = surviving;
        }
        true
    }

    /// Geometry for every pane, plus the dividers between them.
    pub fn compute(&self, area: Rect) -> (Vec<(PaneId, Rect)>, Vec<Divider>) {
        let mut panes = Vec::new();
        let mut dividers = Vec::new();
        layout_node(&self.root, area, &mut panes, &mut dividers);
        (panes, dividers)
    }

    pub fn rect_of(&self, id: PaneId, area: Rect) -> Option<Rect> {
        self.compute(area).0.into_iter().find(|(p, _)| *p == id).map(|(_, r)| r)
    }

    /// Move focus geometrically: pick the nearest pane whose centre lies in the
    /// requested direction. More predictable than tree-order traversal when the
    /// tree shape does not match what the user sees.
    pub fn focus_dir(&mut self, nav: Nav, area: Rect) -> bool {
        let (panes, _) = self.compute(area);
        let Some(cur) = panes.iter().find(|(p, _)| *p == self.focus).map(|(_, r)| *r) else {
            return false;
        };
        let (cx, cy) = cur.centre();
        let mut best: Option<(u32, PaneId)> = None;
        for (id, r) in &panes {
            if *id == self.focus {
                continue;
            }
            let (ox, oy) = r.centre();
            let ok = match nav {
                Nav::Left => r.x + r.w <= cur.x,
                Nav::Right => r.x >= cur.x + cur.w,
                Nav::Up => r.y + r.h <= cur.y,
                Nav::Down => r.y >= cur.y + cur.h,
            };
            if !ok {
                continue;
            }
            let d = (ox as i32 - cx as i32).unsigned_abs() + (oy as i32 - cy as i32).unsigned_abs();
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, *id));
            }
        }
        if let Some((_, id)) = best {
            self.focus = id;
            true
        } else {
            false
        }
    }

    /// Move the divider bordering the focused pane one step along `nav`.
    ///
    /// The divider moves the way you point, regardless of which side holds
    /// focus. An earlier version flipped the sign based on the focused side,
    /// which made the same keystroke grow or shrink depending on where you
    /// happened to be - unpredictable, and caught by a test.
    pub fn resize_focused(&mut self, nav: Nav, area: Rect, step: u16) -> bool {
        let want = match nav {
            Nav::Left | Nav::Right => Dir::Horizontal,
            Nav::Up | Nav::Down => Dir::Vertical,
        };
        let forward = matches!(nav, Nav::Right | Nav::Down);
        let focus = self.focus;
        adjust_ratio(&mut self.root, focus, want, area, forward, step)
    }

    /// The pane at a screen position, for click-to-focus.
    pub fn pane_at(&self, area: Rect, x: u16, y: u16) -> Option<PaneId> {
        self.compute(area).0.into_iter().find(|(_, r)| r.contains(x, y)).map(|(p, _)| p)
    }
}

fn replace_leaf(node: &mut Node, target: PaneId, f: impl FnOnce(Node) -> Node) -> bool {
    match node {
        Node::Leaf(id) if *id == target => {
            let old = std::mem::replace(node, Node::Leaf(target));
            *node = f(old);
            true
        }
        Node::Leaf(_) => false,
        Node::Split { a, b, .. } => {
            // FnOnce cannot be passed twice, so try each side explicitly.
            if contains_leaf(a, target) {
                replace_leaf(a, target, f)
            } else if contains_leaf(b, target) {
                replace_leaf(b, target, f)
            } else {
                false
            }
        }
    }
}

fn contains_leaf(node: &Node, target: PaneId) -> bool {
    match node {
        Node::Leaf(id) => *id == target,
        Node::Split { a, b, .. } => contains_leaf(a, target) || contains_leaf(b, target),
    }
}

/// Remove `target`, collapsing its parent split. Returns a surviving pane id to
/// take focus.
fn remove_leaf(node: &mut Node, target: PaneId) -> Option<PaneId> {
    let (collapse_to, focus) = match node {
        Node::Leaf(_) => return None,
        Node::Split { a, b, .. } => {
            if matches!(**a, Node::Leaf(id) if id == target) {
                let mut leaves = Vec::new();
                b.leaves(&mut leaves);
                ((**b).clone(), leaves[0])
            } else if matches!(**b, Node::Leaf(id) if id == target) {
                let mut leaves = Vec::new();
                a.leaves(&mut leaves);
                ((**a).clone(), leaves[0])
            } else {
                return remove_leaf(a, target).or_else(|| remove_leaf(b, target));
            }
        }
    };
    *node = collapse_to;
    Some(focus)
}

fn adjust_ratio(
    node: &mut Node,
    target: PaneId,
    want: Dir,
    area: Rect,
    forward: bool,
    step: u16,
) -> bool {
    let Node::Split { dir, ratio, a, b } = node else { return false };
    let dir = *dir;
    let (ra, rb) = split_rects(dir, *ratio, area);

    // DESCEND FIRST, then consider this node.
    //
    // Testing this node before descending moved the first matching ANCESTOR,
    // which for nested same-axis splits is the wrong divider: with three panes
    // side by side (split horizontally twice, so the tree is 1 | (2 | 3)),
    // resizing pane 3 leftwards moved the outer 1|(2 3) divider instead of the
    // 2|3 divider that actually borders it. The doc comment on
    // `resize_focused` has always said "the divider BORDERING the focused
    // pane"; this makes it true.
    let descended = if contains_leaf(a, target) {
        adjust_ratio(a, target, want, ra, forward, step)
    } else if contains_leaf(b, target) {
        adjust_ratio(b, target, want, rb, forward, step)
    } else {
        return false; // the target is not below us at all
    };
    if descended {
        return true;
    }

    // The DEEPEST split on this axis wins, which is what descending first
    // achieves. Taking the shallowest left the inner divider unreachable from
    // any pane: in 1 | (2 | 3) both pane 2 and pane 3 drove the outer divider
    // and nothing could move the one between them.
    //
    // Deliberately NOT restricted to the side the target sits on. That would
    // be more literal - "move my left edge left" - but it breaks the symmetry
    // this module documents and tests: an edge pane could then point outward
    // and get nothing, so the same keystroke would work or not depending on
    // where you happened to be.
    if dir != want {
        return false;
    }
    let span = match dir {
        Dir::Horizontal => area.w,
        Dir::Vertical => area.h,
    };
    if span == 0 {
        return false;
    }
    let delta = step as f32 / span as f32;
    // Right/Down move the divider forward (ratio up); Left/Up move it back.
    let sign = if forward { 1.0 } else { -1.0 };
    let min = match dir {
        Dir::Horizontal => MIN_COLS as f32 / span as f32,
        Dir::Vertical => MIN_ROWS as f32 / span as f32,
    };
    // f32::clamp panics when lo > hi or either bound is NaN. Both happen in a
    // viewport too small for two minimum-size panes, which is exactly when a
    // user is most likely to be dragging a divider around.
    if !min.is_finite() || min >= 0.5 {
        return false;
    }
    let next = (*ratio + sign * delta).clamp(min, 1.0 - min);
    if (next - *ratio).abs() < f32::EPSILON {
        return false;
    }
    *ratio = next;
    true
}

/// Divide `area` in two, reserving one cell for the divider between them.
fn split_rects(dir: Dir, ratio: f32, area: Rect) -> (Rect, Rect) {
    match dir {
        Dir::Horizontal => {
            let usable = area.w.saturating_sub(1);
            // `clamp(lo, hi)` PANICS when lo > hi, which happens as soon as the
            // area is too small to hold two minimum-width panes - reachable by
            // shrinking an existing split, not just by creating one.
            let lo = MIN_COLS.min(usable);
            let hi = usable.saturating_sub(lo);
            let aw = if lo > hi {
                usable / 2 // too small for the minimum; split evenly and let it be ugly
            } else {
                ((usable as f32 * ratio).round() as u16).clamp(lo, hi)
            };
            (
                Rect::new(area.x, area.y, aw, area.h),
                Rect::new(area.x + aw + 1, area.y, usable.saturating_sub(aw), area.h),
            )
        }
        Dir::Vertical => {
            let usable = area.h.saturating_sub(1);
            let lo = MIN_ROWS.min(usable);
            let hi = usable.saturating_sub(lo);
            let ah = if lo > hi {
                usable / 2
            } else {
                ((usable as f32 * ratio).round() as u16).clamp(lo, hi)
            };
            (
                Rect::new(area.x, area.y, area.w, ah),
                Rect::new(area.x, area.y + ah + 1, area.w, usable.saturating_sub(ah)),
            )
        }
    }
}

fn layout_node(
    node: &Node,
    area: Rect,
    panes: &mut Vec<(PaneId, Rect)>,
    dividers: &mut Vec<Divider>,
) {
    match node {
        Node::Leaf(id) => panes.push((*id, area)),
        Node::Split { dir, ratio, a, b } => {
            let (ra, rb) = split_rects(*dir, *ratio, area);
            dividers.push(Divider {
                dir: *dir,
                rect: match dir {
                    Dir::Horizontal => Rect::new(ra.x + ra.w, area.y, 1, area.h),
                    Dir::Vertical => Rect::new(area.x, ra.y + ra.h, area.w, 1),
                },
            });
            layout_node(a, ra, panes, dividers);
            layout_node(b, rb, panes, dividers);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 80, 24)
    }

    #[test]
    fn single_pane_fills_the_area() {
        let l = Layout::new(PaneId(1));
        let (panes, dividers) = l.compute(area());
        assert_eq!(panes, vec![(PaneId(1), area())]);
        assert!(dividers.is_empty());
    }

    #[test]
    fn horizontal_split_reserves_one_column_for_the_divider() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        let (panes, dividers) = l.compute(area());
        assert_eq!(panes.len(), 2);
        let total: u16 = panes.iter().map(|(_, r)| r.w).sum();
        assert_eq!(total + 1, 80, "panes plus divider must tile the area exactly");
        assert_eq!(dividers.len(), 1);
        assert_eq!(dividers[0].rect.w, 1);
    }

    #[test]
    fn panes_never_overlap() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        l.split(Dir::Vertical, area(), PaneId(3)).unwrap();
        let (panes, _) = l.compute(area());
        for (i, (_, a)) in panes.iter().enumerate() {
            for (_, b) in panes.iter().skip(i + 1) {
                let disjoint = a.x + a.w <= b.x || b.x + b.w <= a.x
                    || a.y + a.h <= b.y || b.y + b.h <= a.y;
                assert!(disjoint, "{a:?} overlaps {b:?}");
            }
        }
    }

    #[test]
    fn split_is_refused_when_the_area_is_too_small() {
        let mut l = Layout::new(PaneId(1));
        let tiny = Rect::new(0, 0, 10, 24); // < MIN_COLS*2+1
        assert!(l.split(Dir::Horizontal, tiny, PaneId(2)).is_none());
        assert_eq!(l.len(), 1, "a refused split must not mutate the tree");
    }

    #[test]
    fn closing_collapses_the_parent_split() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        assert_eq!(l.focus(), PaneId(2));
        assert!(l.close(PaneId(2)));
        assert_eq!(l.panes(), vec![PaneId(1)]);
        assert_eq!(l.focus(), PaneId(1));
        assert_eq!(l.compute(area()).0[0].1, area(), "survivor reclaims the space");
    }

    #[test]
    fn closing_by_id_leaves_focus_alone_unless_it_was_the_victim() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap(); // focus 2
        l.split(Dir::Vertical, area(), PaneId(3)).unwrap();   // focus 3
        assert!(l.close(PaneId(1)), "pane 1 should close");
        assert_eq!(l.focus(), PaneId(3), "closing another pane must not steal focus");
        assert!(l.close(PaneId(3)), "the focused pane should close");
        assert_eq!(l.focus(), PaneId(2), "focus moves to the survivor");
    }

    #[test]
    fn closing_a_pane_that_is_not_here_does_nothing() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        assert!(!l.close(PaneId(99)));
        assert_eq!(l.len(), 2, "a no-op close must not disturb the tree");
    }

    #[test]
    fn the_last_pane_cannot_be_closed() {
        let mut l = Layout::new(PaneId(1));
        assert!(!l.close(PaneId(1)));
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn focus_moves_geometrically() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap(); // 1 | 2, focus 2
        assert!(l.focus_dir(Nav::Left, area()));
        assert_eq!(l.focus(), PaneId(1));
        assert!(l.focus_dir(Nav::Right, area()));
        assert_eq!(l.focus(), PaneId(2));
        assert!(!l.focus_dir(Nav::Right, area()), "nothing to the right of the last pane");
    }

    #[test]
    fn resize_moves_the_divider_and_respects_minimums() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        // The divider moves the way you point, whichever pane is focused.
        let before = l.rect_of(PaneId(1), area()).unwrap().w;
        assert!(l.resize_focused(Nav::Left, area(), 5));
        let after = l.rect_of(PaneId(1), area()).unwrap().w;
        assert!(after < before, "Nav::Left should move the divider left: {before} -> {after}");

        // And it is symmetric: pointing back returns it.
        assert!(l.resize_focused(Nav::Right, area(), 5));
        assert_eq!(l.rect_of(PaneId(1), area()).unwrap().w, before, "resize is not symmetric");

        for _ in 0..100 {
            l.resize_focused(Nav::Left, area(), 5);
        }
        let (panes, _) = l.compute(area());
        for (id, r) in panes {
            assert!(r.w >= MIN_COLS, "pane {id:?} collapsed below MIN_COLS: {r:?}");
        }
    }

    /// Three panes side by side is `1 | (2 | 3)`. Resizing pane 3 must move
    /// the divider next to it, not the outer one - which is what taking the
    /// first matching ancestor did, leaving the inner divider unreachable
    /// from any pane at all.
    #[test]
    fn nested_same_axis_splits_resize_the_nearest_divider() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap(); // focus 2
        l.split(Dir::Horizontal, area(), PaneId(3)).unwrap(); // focus 3
        let w = |l: &Layout, id| l.rect_of(id, area()).unwrap().w;
        let (w1, w2, w3) = (w(&l, PaneId(1)), w(&l, PaneId(2)), w(&l, PaneId(3)));

        assert!(l.resize_focused(Nav::Left, area(), 4));
        assert_eq!(w(&l, PaneId(1)), w1, "the outer divider must not have moved");
        assert!(w(&l, PaneId(2)) < w2, "pane 2 should have given up the space");
        assert!(w(&l, PaneId(3)) > w3, "pane 3 should have taken it");
    }

    /// And it is still symmetric there: pointing back returns it.
    #[test]
    fn resizing_a_nested_split_is_symmetric() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        l.split(Dir::Horizontal, area(), PaneId(3)).unwrap();
        let before = l.rect_of(PaneId(3), area()).unwrap().w;
        assert!(l.resize_focused(Nav::Left, area(), 4));
        assert!(l.resize_focused(Nav::Right, area(), 4));
        assert_eq!(l.rect_of(PaneId(3), area()).unwrap().w, before);
    }

    #[test]
    fn pane_at_finds_the_pane_under_a_point() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        assert_eq!(l.pane_at(area(), 2, 2), Some(PaneId(1)));
        assert_eq!(l.pane_at(area(), 78, 2), Some(PaneId(2)));
    }

    /// `clamp(lo, hi)` panics when lo > hi. Creating a split in a tiny area is
    /// refused, but SHRINKING an existing one walks straight into it.
    #[test]
    fn shrinking_a_split_below_the_minimum_does_not_panic() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Horizontal, area(), PaneId(2)).unwrap();
        // Squeeze the viewport well past what two panes can occupy.
        for w in (1..=20).rev() {
            let tiny = Rect::new(0, 0, w, 24);
            let _ = l.compute(tiny);
            let _ = l.resize_focused(Nav::Left, tiny, 1);
            let _ = l.focus_dir(Nav::Right, tiny);
        }
        for h in (1..=8).rev() {
            let tiny = Rect::new(0, 0, 80, h);
            let _ = l.compute(tiny);
        }
    }

    #[test]
    fn a_split_layout_in_a_one_cell_area_still_computes() {
        let mut l = Layout::new(PaneId(1));
        l.split(Dir::Vertical, area(), PaneId(2)).unwrap();
        let (panes, _) = l.compute(Rect::new(0, 0, 1, 1));
        assert_eq!(panes.len(), 2, "every leaf must still get a rect");
    }

    /// The bug from DESIGN.md §2.2 that this module exists to avoid.
    #[test]
    fn restoring_ids_advances_the_allocator() {
        let restored = PaneId::from_raw(9_000);
        let fresh = PaneId::alloc();
        assert!(fresh.0 > restored.0, "alloc() reused a restored id: {fresh:?} vs {restored:?}");
    }

    #[test]
    fn restoring_repairs_focus_that_names_a_missing_pane() {
        let root = Node::Split {
            dir: Dir::Horizontal,
            ratio: 0.5,
            a: Box::new(Node::Leaf(PaneId(1))),
            b: Box::new(Node::Leaf(PaneId(2))),
        };
        let l = Layout::from_saved(root, PaneId(42));
        assert!(l.panes().contains(&l.focus()), "focus must name a real pane");
    }

    #[test]
    fn layout_survives_a_serde_round_trip() {
        let mut l = Layout::new(PaneId::alloc());
        l.split(Dir::Vertical, area(), PaneId::alloc()).unwrap();
        let json = serde_json::to_string(&l).unwrap();
        let back: Layout = serde_json::from_str(&json).unwrap();
        assert_eq!(back.panes(), l.panes());
        assert_eq!(back.focus(), l.focus());
    }
}
