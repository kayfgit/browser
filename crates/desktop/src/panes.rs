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
/// a `Split` divides its region 50/50 between two children.
pub(crate) enum PaneNode {
    Leaf(usize),
    Split { dir: SplitDir, a: Box<PaneNode>, b: Box<PaneNode> },
}

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

    /// Point the leaf currently showing `from` at `to` instead (used to load a
    /// different tab into the focused pane).
    pub(crate) fn retarget(&mut self, from: usize, to: usize) {
        match self {
            PaneNode::Leaf(t) => {
                if *t == from {
                    *t = to;
                }
            }
            PaneNode::Split { a, b, .. } => {
                a.retarget(from, to);
                b.retarget(from, to);
            }
        }
    }

    /// Swap which tabs two leaves show (when bringing a tab that's already in another
    /// pane into the focused pane, the two panes trade contents).
    pub(crate) fn swap_leaves(&mut self, x: usize, y: usize) {
        match self {
            PaneNode::Leaf(t) => {
                if *t == x {
                    *t = y;
                } else if *t == y {
                    *t = x;
                }
            }
            PaneNode::Split { a, b, .. } => {
                a.swap_leaves(x, y);
                b.swap_leaves(x, y);
            }
        }
    }

    /// Replace the leaf showing `target` with a `dir` split of `target` and `new`.
    pub(crate) fn insert_split(self, target: usize, dir: SplitDir, new: usize) -> PaneNode {
        match self {
            PaneNode::Leaf(t) if t == target => PaneNode::Split {
                dir,
                a: Box::new(PaneNode::Leaf(t)),
                b: Box::new(PaneNode::Leaf(new)),
            },
            PaneNode::Leaf(t) => PaneNode::Leaf(t),
            PaneNode::Split { dir: d, a, b } => PaneNode::Split {
                dir: d,
                a: Box::new(a.insert_split(target, dir, new)),
                b: Box::new(b.insert_split(target, dir, new)),
            },
        }
    }

    /// Remove the leaf showing `tab`, collapsing its parent split into the sibling.
    /// `None` if this whole subtree was just that leaf.
    pub(crate) fn prune(self, tab: usize) -> Option<PaneNode> {
        match self {
            PaneNode::Leaf(t) => (t != tab).then_some(PaneNode::Leaf(t)),
            PaneNode::Split { dir, a, b } => match (a.prune(tab), b.prune(tab)) {
                (Some(a), Some(b)) => Some(PaneNode::Split {
                    dir,
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
            PaneNode::Split { dir, a, b } => match dir {
                SplitDir::Row => {
                    let half = ((r.w - DIVIDER) / 2).max(1);
                    a.layout(PaneRect { w: half, ..r }, panes, divs);
                    divs.push(PaneRect { x: r.x + half, w: DIVIDER, ..r });
                    let bx = r.x + half + DIVIDER;
                    b.layout(PaneRect { x: bx, w: (r.x + r.w - bx).max(1), ..r }, panes, divs);
                }
                SplitDir::Col => {
                    let half = ((r.h - DIVIDER) / 2).max(1);
                    a.layout(PaneRect { h: half, ..r }, panes, divs);
                    divs.push(PaneRect { y: r.y + half, h: DIVIDER, ..r });
                    let by = r.y + half + DIVIDER;
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

    /// The current pane tiling: each `(tab, rect)` to paint/position, plus the
    /// divider rects between them. With no split it's just the active tab filling
    /// the whole content band.
    pub(crate) fn pane_layout(&self) -> (Vec<(usize, PaneRect)>, Vec<PaneRect>) {
        let band = self.content_band();
        let mut panes = Vec::new();
        let mut divs = Vec::new();
        match &self.split {
            Some(tree) => tree.layout(band, &mut panes, &mut divs),
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
    /// returning the shell to Normal so its keys work in the newly focused pane.
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

    /// Split the focused pane, opening a new blank pane beside it (`:vsplit` = Row,
    /// side by side; `:split` = Col, stacked) and focusing the new pane.
    pub(crate) fn split_pane(&mut self, dir: SplitDir) {
        let Some(a) = self.active else {
            self.set_status("no pane to split — open something first");
            return;
        };
        let new_idx = self.tabs.len();
        self.tabs.push(Tab::blank());
        self.split = Some(match self.split.take() {
            None => PaneNode::Split {
                dir,
                a: Box::new(PaneNode::Leaf(a)),
                b: Box::new(PaneNode::Leaf(new_idx)),
            },
            Some(tree) => tree.insert_split(a, dir, new_idx),
        });
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

    /// Build `Split(dir, Leaf(a), Leaf(b))`.
    fn split(dir: SplitDir, a: usize, b: usize) -> PaneNode {
        PaneNode::Split { dir, a: Box::new(PaneNode::Leaf(a)), b: Box::new(PaneNode::Leaf(b)) }
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
    fn swap_and_retarget_change_leaf_tabs() {
        let mut t = split(SplitDir::Row, 0, 1);
        t.swap_leaves(0, 1);
        assert_eq!(leaves(&t), vec![1, 0]);
        t.retarget(0, 5); // the leaf showing 0 now shows 5.
        assert_eq!(leaves(&t), vec![1, 5]);
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
