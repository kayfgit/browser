//! tmux-style pane tiling: the split tree (which tab shows in which rect of the
//! content band), its maintenance operations, and the [`App`] methods that drive
//! pane focus, splitting, and hit-testing.

use crate::{App, ModeKind, Tab};

/// A rectangle in physical pixels within the content band, for pane tiling.
#[derive(Clone, Copy)]
pub(crate) struct PaneRect {
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) w: i32,
    pub(crate) h: i32,
}

/// Which way a split divides its region.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum SplitDir {
    /// Children side by side, left | right (a vertical divider) — `:vsplit`.
    Row,
    /// Children stacked, top / bottom (a horizontal divider) — `:split`.
    Col,
}

/// A node in the pane layout tree. A `Leaf` shows one tab (by index into `tabs`);
/// a `Split` divides its region between two children, `ratio` being the fraction of
/// the space given to the first child `a` (left in a Row split, top in a Col split).
/// New splits start at `0.5` (an even divide); Ctrl+W H/J/K/L nudge it.
pub(crate) enum PaneNode {
    Leaf(usize),
    Split { dir: SplitDir, ratio: f32, a: Box<PaneNode>, b: Box<PaneNode> },
}

/// Clamp on a split's [`ratio`](PaneNode::Split) so a pane can't be resized to nothing
/// (or swallow its sibling) — each side keeps at least this fraction.
const RATIO_MIN: f32 = 0.1;
const RATIO_MAX: f32 = 0.9;
/// How much one Ctrl+W H/J/K/L step moves a divider (fraction of the split's extent).
const RESIZE_STEP: f32 = 0.05;

/// Width/height in px of the line drawn between two panes.
pub(crate) const DIVIDER: i32 = 1;
/// Thickness in px of the accent border around the focused pane.
pub(crate) const FOCUS_BORDER: i32 = 2;

impl PaneNode {
    /// The tab index of the first (top-left-most) leaf — used to pick a new focus
    /// after the focused pane is closed.
    pub(crate) fn first_leaf(&self) -> usize {
        match self {
            PaneNode::Leaf(t) => *t,
            PaneNode::Split { a, .. } => a.first_leaf(),
        }
    }

    /// Whether any leaf shows `tab` — a cheap, allocation-free membership test (used
    /// to find which window holds the active tab, on every layout/draw).
    pub(crate) fn contains_leaf(&self, tab: usize) -> bool {
        match self {
            PaneNode::Leaf(t) => *t == tab,
            PaneNode::Split { a, b, .. } => a.contains_leaf(tab) || b.contains_leaf(tab),
        }
    }

    /// Collect every leaf's tab index.
    pub(crate) fn leaves(&self, out: &mut Vec<usize>) {
        match self {
            PaneNode::Leaf(t) => out.push(*t),
            PaneNode::Split { a, b, .. } => {
                a.leaves(out);
                b.leaves(out);
            }
        }
    }

    /// After a tab at index `removed` is deleted from `tabs`, decrement every leaf
    /// index above it so leaves keep pointing at the right tabs.
    pub(crate) fn shift_after_remove(&mut self, removed: usize) {
        match self {
            PaneNode::Leaf(t) => {
                if *t > removed {
                    *t -= 1;
                }
            }
            PaneNode::Split { a, b, .. } => {
                a.shift_after_remove(removed);
                b.shift_after_remove(removed);
            }
        }
    }

    /// Replace the leaf showing `target` with a `dir` split of `target` and `new`.
    pub(crate) fn insert_split(self, target: usize, dir: SplitDir, new: usize) -> PaneNode {
        match self {
            PaneNode::Leaf(t) if t == target => PaneNode::Split {
                dir,
                ratio: 0.5,
                a: Box::new(PaneNode::Leaf(t)),
                b: Box::new(PaneNode::Leaf(new)),
            },
            PaneNode::Leaf(t) => PaneNode::Leaf(t),
            PaneNode::Split { dir: d, ratio, a, b } => PaneNode::Split {
                dir: d,
                ratio,
                a: Box::new(a.insert_split(target, dir, new)),
                b: Box::new(b.insert_split(target, dir, new)),
            },
        }
    }

    /// Nudge the divider of the deepest split on the path to `tab` whose axis matches
    /// `axis`, by `delta` (a signed fraction added to the first child's `ratio`). That
    /// deepest match is the divider closest to the focused pane, so resizing feels
    /// local. `delta` is applied in screen terms — positive grows the first child
    /// (left/top), so a rightward/downward key passes `+`, a leftward/upward one `-`,
    /// regardless of which side the focused pane sits on. Returns whether one was found.
    pub(crate) fn resize_split(&mut self, tab: usize, axis: SplitDir, delta: f32) -> bool {
        let PaneNode::Split { dir, ratio, a, b } = self else {
            return false;
        };
        let (in_a, in_b) = (a.contains_leaf(tab), b.contains_leaf(tab));
        if !in_a && !in_b {
            return false;
        }
        // Try a deeper matching divider first (closest to the pane wins).
        if in_a && a.resize_split(tab, axis, delta) {
            return true;
        }
        if in_b && b.resize_split(tab, axis, delta) {
            return true;
        }
        if *dir == axis {
            *ratio = (*ratio + delta).clamp(RATIO_MIN, RATIO_MAX);
            return true;
        }
        false
    }

    /// Remove the leaf showing `tab`, collapsing its parent split into the sibling.
    /// `None` if this whole subtree was just that leaf.
    pub(crate) fn prune(self, tab: usize) -> Option<PaneNode> {
        match self {
            PaneNode::Leaf(t) => (t != tab).then_some(PaneNode::Leaf(t)),
            PaneNode::Split { dir, ratio, a, b } => match (a.prune(tab), b.prune(tab)) {
                (Some(a), Some(b)) => Some(PaneNode::Split {
                    dir,
                    ratio,
                    a: Box::new(a),
                    b: Box::new(b),
                }),
                (Some(n), None) | (None, Some(n)) => Some(n),
                (None, None) => None,
            },
        }
    }

    /// Tile `r` across this tree, pushing each leaf's `(tab, rect)` into `panes` and
    /// each split's divider rect into `divs`.
    pub(crate) fn layout(&self, r: PaneRect, panes: &mut Vec<(usize, PaneRect)>, divs: &mut Vec<PaneRect>) {
        match self {
            PaneNode::Leaf(t) => panes.push((*t, r)),
            PaneNode::Split { dir, ratio, a, b } => match dir {
                SplitDir::Row => {
                    // First child gets `ratio` of the space left after the divider.
                    let avail = (r.w - DIVIDER).max(2);
                    let first = ((avail as f32 * ratio).round() as i32).clamp(1, avail - 1);
                    a.layout(PaneRect { w: first, ..r }, panes, divs);
                    divs.push(PaneRect { x: r.x + first, w: DIVIDER, ..r });
                    let bx = r.x + first + DIVIDER;
                    b.layout(PaneRect { x: bx, w: (r.x + r.w - bx).max(1), ..r }, panes, divs);
                }
                SplitDir::Col => {
                    let avail = (r.h - DIVIDER).max(2);
                    let first = ((avail as f32 * ratio).round() as i32).clamp(1, avail - 1);
                    a.layout(PaneRect { h: first, ..r }, panes, divs);
                    divs.push(PaneRect { y: r.y + first, h: DIVIDER, ..r });
                    let by = r.y + first + DIVIDER;
                    b.layout(PaneRect { y: by, h: (r.y + r.h - by).max(1), ..r }, panes, divs);
                }
            },
        }
    }
}

impl App {
    /// The whole content band (between the tab bar and command bar) as a [`PaneRect`].
    pub(crate) fn content_band(&self) -> PaneRect {
        let (w, h) = self.inner();
        let top = self.tab_bar_h() as i32;
        let bot = h as i32 - self.bar_h() as i32;
        PaneRect { x: 0, y: top, w: w as i32, h: (bot - top).max(1) }
    }

    /// Index into [`windows`](Self::windows) of the active window — the one whose pane
    /// tree holds the active tab. `None` when nothing is open, or when the active tab is
    /// the windowless `:ai` singleton (which overlays the whole band).
    pub(crate) fn active_window(&self) -> Option<usize> {
        let a = self.active?;
        self.windows.iter().position(|tree| tree.contains_leaf(a))
    }

    /// Whether the active window is split into more than one pane (a `Split` tree).
    /// False for a standalone tab, the `:ai` overlay, or the welcome screen.
    pub(crate) fn is_split(&self) -> bool {
        self.active_window()
            .is_some_and(|w| matches!(self.windows[w], PaneNode::Split { .. }))
    }

    /// The current pane tiling: each `(tab, rect)` to paint/position, plus the divider
    /// rects between them. Only the ACTIVE window is tiled (the rest are hidden); a
    /// standalone tab or the `:ai` overlay fills the whole content band.
    pub(crate) fn pane_layout(&self) -> (Vec<(usize, PaneRect)>, Vec<PaneRect>) {
        let band = self.content_band();
        let mut panes = Vec::new();
        let mut divs = Vec::new();
        match self.active_window() {
            Some(w) => self.windows[w].layout(band, &mut panes, &mut divs),
            None => {
                if let Some(a) = self.active {
                    panes.push((a, band));
                }
            }
        }
        (panes, divs)
    }

    /// The rect of the focused (active) pane — the whole band when not split.
    pub(crate) fn focused_pane_rect(&self) -> PaneRect {
        let (panes, _) = self.pane_layout();
        panes
            .iter()
            .find(|(t, _)| Some(*t) == self.active)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| self.content_band())
    }

    /// Make `tab` the focused pane (the active tab), repositioning webviews and
    /// returning the shell to Normal so its keys work in the newly focused pane. Used
    /// by the Ctrl+W keyboard move, which deliberately pulls the keyboard to the shell.
    pub(crate) fn set_active_pane(&mut self, tab: usize) {
        if Some(tab) == self.active || tab >= self.tabs.len() {
            return;
        }
        self.active = Some(tab);
        self.mode = ModeKind::Normal;
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// Mark `tab` the active pane because the user CLICKED inside it. Unlike
    /// [`set_active_pane`](Self::set_active_pane) (the Ctrl+W keyboard move), this does
    /// NOT wrestle keyboard focus back to the shell or reset the mode: the click is
    /// already being handled by that pane's own content — the page bridge focuses a
    /// field (→ passthrough), follows a link, or keeps a control's menu open — so the
    /// focus border is a purely visual cue. Stealing focus here is what made a click on
    /// a non-focused web pane merely "select" it, needing a second click to interact.
    pub(crate) fn focus_pane_click(&mut self, tab: usize) {
        if Some(tab) == self.active || tab >= self.tabs.len() {
            return;
        }
        self.active = Some(tab);
        self.find_reset();
        self.refresh_visibility();
        self.window.request_redraw();
    }

    /// Move pane focus in a direction (`h`/`j`/`k`/`l`): the spatially-nearest pane
    /// whose center lies that way from the focused pane's center, preferring panes
    /// aligned on the cross axis. No-op when not split or nothing lies that way.
    pub(crate) fn move_pane_focus(&mut self, dir: char) {
        let (panes, _) = self.pane_layout();
        let Some((_, fr)) = panes.iter().find(|(t, _)| Some(*t) == self.active) else {
            return;
        };
        let (fcx, fcy) = (fr.x + fr.w / 2, fr.y + fr.h / 2);
        let mut best: Option<usize> = None;
        let mut best_score = i32::MAX;
        for (t, r) in &panes {
            if Some(*t) == self.active {
                continue;
            }
            let (cx, cy) = (r.x + r.w / 2, r.y + r.h / 2);
            let ahead = match dir {
                'h' => cx < fcx,
                'l' => cx > fcx,
                'k' => cy < fcy,
                'j' => cy > fcy,
                _ => false,
            };
            if !ahead {
                continue;
            }
            // Distance along the move axis, plus a penalty for cross-axis offset so a
            // pane directly in line is preferred over a diagonal one.
            let (primary, cross) = match dir {
                'h' | 'l' => ((fcx - cx).abs(), (fcy - cy).abs()),
                _ => ((fcy - cy).abs(), (fcx - cx).abs()),
            };
            let score = primary + cross * 2;
            if score < best_score {
                best_score = score;
                best = Some(*t);
            }
        }
        if let Some(t) = best {
            self.set_active_pane(t);
        }
    }

    /// Resize the focused pane in a direction (`h`/`j`/`k`/`l`, bound to Ctrl+W then
    /// Shift+H/J/K/L): `h`/`l` slide the nearest vertical divider left/right, `j`/`k`
    /// the nearest horizontal one down/up. Repositions the panes and repaints; a no-op
    /// (with a hint) when nothing lies along that axis to resize.
    pub(crate) fn resize_pane(&mut self, dir: char) {
        let Some(a) = self.active else { return };
        let Some(w) = self.active_window() else { return };
        // `h`/`l` move a Row split's divider (delta on the left child's width); `j`/`k`
        // a Col split's (delta on the top child's height). Right/down grow the first
        // child (+), left/up shrink it (−).
        let (axis, delta) = match dir {
            'h' => (SplitDir::Row, -RESIZE_STEP),
            'l' => (SplitDir::Row, RESIZE_STEP),
            'k' => (SplitDir::Col, -RESIZE_STEP),
            'j' => (SplitDir::Col, RESIZE_STEP),
            _ => return,
        };
        if self.windows[w].resize_split(a, axis, delta) {
            self.refresh_visibility();
            self.window.request_redraw();
        } else {
            self.set_status("no divider to resize that way");
        }
    }

    /// Enter the repeatable pane-resize mode ([`ModeKind::PaneResize`]) with a first
    /// nudge, so `Ctrl+W` then `Shift+H/J/K/L` starts resizing and subsequent h/j/k/l
    /// keep going without re-arming. No-op (with a hint) when there's no split to size.
    pub(crate) fn enter_pane_resize(&mut self, dir: char) {
        if !self.is_split() {
            self.set_status("nothing to resize — split first (Ctrl+W v, or :vsplit)");
            return;
        }
        self.resize_pane(dir);
        self.mode = ModeKind::PaneResize;
        self.pane_resize_at = std::time::Instant::now();
        self.window.request_redraw();
    }

    /// Split the focused pane, opening a new blank pane beside it (`:vsplit` = Row,
    /// side by side; `:split` = Col, stacked) and focusing the new pane. The new pane
    /// grows the ACTIVE window's tree, so it stays "inside" the same tab-strip entry
    /// (tmux-style) rather than spawning another tab.
    pub(crate) fn split_pane(&mut self, dir: SplitDir) {
        let Some(a) = self.active else {
            self.set_status("no pane to split — open something first");
            return;
        };
        // The `:ai` singleton has no window of its own (it overlays the band); there's
        // nothing to tile it against, so don't fold it into a split.
        let Some(w) = self.active_window() else {
            self.set_status("can't split this — open a page first");
            return;
        };
        let new_idx = self.tabs.len();
        self.tabs.push(Tab::blank());
        let tree = std::mem::replace(&mut self.windows[w], PaneNode::Leaf(new_idx));
        self.windows[w] = tree.insert_split(a, dir, new_idx);
        self.active = Some(new_idx);
        self.mode = ModeKind::Normal;
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// The tab + rect of the pane under a pixel (for wheel/click routing). With no
    /// split this is the active tab filling the band.
    pub(crate) fn pane_at_pixel(&self, x: f64, y: f64) -> Option<(usize, PaneRect)> {
        let (px, py) = (x as i32, y as i32);
        self.pane_layout()
            .0
            .into_iter()
            .find(|(_, r)| px >= r.x && px < r.x + r.w && py >= r.y && py < r.y + r.h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an even `Split(dir, Leaf(a), Leaf(b))`.
    fn split(dir: SplitDir, a: usize, b: usize) -> PaneNode {
        PaneNode::Split {
            dir,
            ratio: 0.5,
            a: Box::new(PaneNode::Leaf(a)),
            b: Box::new(PaneNode::Leaf(b)),
        }
    }

    fn leaves(n: &PaneNode) -> Vec<usize> {
        let mut v = Vec::new();
        n.leaves(&mut v);
        v
    }

    #[test]
    fn insert_split_replaces_the_target_leaf() {
        // Single leaf 0 → vsplit → two leaves [0, 1].
        let t = PaneNode::Leaf(0).insert_split(0, SplitDir::Row, 1);
        assert_eq!(leaves(&t), vec![0, 1]);
        // Splitting leaf 1 again adds leaf 2 beside it: [0, 1, 2].
        let t = t.insert_split(1, SplitDir::Col, 2);
        assert_eq!(leaves(&t), vec![0, 1, 2]);
    }

    #[test]
    fn prune_collapses_the_split_into_the_sibling() {
        let t = split(SplitDir::Row, 0, 1);
        // Removing leaf 1 leaves a lone Leaf(0).
        let pruned = t.prune(1).unwrap();
        assert!(matches!(pruned, PaneNode::Leaf(0)));
        // Removing the only leaf yields nothing.
        assert!(PaneNode::Leaf(0).prune(0).is_none());
    }

    #[test]
    fn shift_after_remove_decrements_higher_leaves() {
        let mut t = split(SplitDir::Row, 0, 2);
        t.shift_after_remove(1); // tab 1 was deleted: 2 → 1, 0 unchanged.
        assert_eq!(leaves(&t), vec![0, 1]);
    }

    #[test]
    fn contains_leaf_finds_membership() {
        let t = split(SplitDir::Row, 0, 2);
        assert!(t.contains_leaf(0) && t.contains_leaf(2));
        assert!(!t.contains_leaf(1));
    }

    #[test]
    fn resize_split_moves_the_matching_divider_and_clamps() {
        // A Row split: `l` grows the left child, shrinking on repeated `h`.
        let mut t = split(SplitDir::Row, 0, 1);
        assert!(t.resize_split(0, SplitDir::Row, RESIZE_STEP));
        match &t {
            PaneNode::Split { ratio, .. } => assert!((*ratio - 0.55).abs() < 1e-5),
            _ => panic!("expected a split"),
        }
        // A Col motion finds no horizontal divider here → no change, reports false.
        assert!(!t.resize_split(0, SplitDir::Col, RESIZE_STEP));
        // Ratio can't be driven past the clamp even with many steps.
        for _ in 0..100 {
            t.resize_split(1, SplitDir::Row, RESIZE_STEP);
        }
        match &t {
            PaneNode::Split { ratio, .. } => assert!(*ratio <= RATIO_MAX + 1e-5),
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn resize_targets_the_deepest_matching_divider() {
        // Row(Leaf0, Row(Leaf1, Leaf2)): resizing from leaf 1 hits the INNER Row split
        // (the divider next to it), leaving the outer 0.5 untouched.
        let inner = split(SplitDir::Row, 1, 2);
        let mut t = PaneNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            a: Box::new(PaneNode::Leaf(0)),
            b: Box::new(inner),
        };
        assert!(t.resize_split(1, SplitDir::Row, RESIZE_STEP));
        match &t {
            PaneNode::Split { ratio: outer, b, .. } => {
                assert!((*outer - 0.5).abs() < 1e-5, "outer divider untouched");
                match b.as_ref() {
                    PaneNode::Split { ratio: innr, .. } => {
                        assert!((*innr - 0.55).abs() < 1e-5, "inner divider moved")
                    }
                    _ => panic!("expected inner split"),
                }
            }
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn layout_tiles_without_overlap_and_inside_the_band() {
        // A row split of a 100×40 band: two side-by-side panes plus a 1px divider.
        let band = PaneRect { x: 0, y: 0, w: 100, h: 40 };
        let mut panes = Vec::new();
        let mut divs = Vec::new();
        split(SplitDir::Row, 0, 1).layout(band, &mut panes, &mut divs);
        assert_eq!(panes.len(), 2);
        assert_eq!(divs.len(), 1);
        // Left pane starts at the band's left; right pane ends at the band's right.
        assert_eq!(panes[0].1.x, 0);
        let right = &panes[1].1;
        assert_eq!(right.x + right.w, 100);
        // The two panes don't overlap (left ends at or before right begins).
        assert!(panes[0].1.x + panes[0].1.w <= right.x);
    }
}
