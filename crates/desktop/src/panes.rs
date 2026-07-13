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
#[derive(Clone)]
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

    /// Exchange the two leaves showing `x` and `y` (swapping which pane each tab
    /// occupies) — the primitive behind move-pane's h/j/k/l. Leaves the tree shape and
    /// every split ratio untouched, so only the contents slide. A no-op for any leaf
    /// that isn't one of the two.
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

    /// Flip the orientation (Row ↔ Col) of the split that is the *immediate parent* of the
    /// leaf showing `tab` — turning that pane's divider from side-by-side to stacked or
    /// vice versa. Ratios and every other split stay put. Returns whether such a parent
    /// was found (false if `tab` is a lone leaf with no split above it here).
    pub(crate) fn flip_parent_split(&mut self, tab: usize) -> bool {
        let PaneNode::Split { dir, a, b, .. } = self else {
            return false;
        };
        let child_is_target = |n: &PaneNode| matches!(n, PaneNode::Leaf(t) if *t == tab);
        if child_is_target(a) || child_is_target(b) {
            *dir = match *dir {
                SplitDir::Row => SplitDir::Col,
                SplitDir::Col => SplitDir::Row,
            };
            return true;
        }
        (a.contains_leaf(tab) && a.flip_parent_split(tab))
            || (b.contains_leaf(tab) && b.flip_parent_split(tab))
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
        // Landing on a terminal in Normal mode = copy/vi mode; seed its cursor now so
        // the block is live immediately (not frozen until the first key heals it).
        self.ensure_term_vi();
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

    /// The spatially-nearest pane whose center lies `dir` (`h`/`j`/`k`/`l`) from the
    /// focused pane's center, preferring panes aligned on the cross axis. `None` when not
    /// split or nothing lies that way. Shared by pane-focus movement and move-pane swaps.
    pub(crate) fn pane_neighbor(&self, dir: char) -> Option<usize> {
        let (panes, _) = self.pane_layout();
        let (_, fr) = panes.iter().find(|(t, _)| Some(*t) == self.active)?;
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
        best
    }

    /// Move pane focus in a direction (`h`/`j`/`k`/`l`): focus the spatially-nearest pane
    /// that way. No-op when not split or nothing lies that way.
    pub(crate) fn move_pane_focus(&mut self, dir: char) {
        if let Some(t) = self.pane_neighbor(dir) {
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

    /// `Ctrl+W <n>` in Normal: start moving a pane. `win` is a tab-bar entry index. If it
    /// is the active window, grab its focused pane to rearrange in place; otherwise pull
    /// that tab into the active window as a new pane beside the focused one. Either way we
    /// enter [`PaneMove`](ModeKind::PaneMove) with the grabbed pane highlighted yellow.
    pub(crate) fn grab_pane_move(&mut self, win: usize) {
        let Some(aw) = self.active_window() else {
            self.set_status("can't move panes here — open a page first");
            return;
        };
        if win >= self.windows.len() {
            self.set_status("no such tab to grab");
            return;
        }
        if win == aw {
            return self.grab_focused_pane_move();
        }
        let Some(anchor) = self.active else { return };
        // Snapshot first so Esc can restore the exact prior layout (including the pulled
        // tab's own window).
        self.pane_move_orig = Some((self.windows.clone(), self.active));
        let tab = self.windows[win].first_leaf();
        // Add the pulled tab beside the focused pane in the active window…
        let tree = std::mem::replace(&mut self.windows[aw], PaneNode::Leaf(tab));
        self.windows[aw] = tree.insert_split(anchor, SplitDir::Row, tab);
        // …then remove it from its source window, dropping that window if it's now empty.
        let src = std::mem::replace(&mut self.windows[win], PaneNode::Leaf(tab));
        match src.prune(tab) {
            Some(t) => self.windows[win] = t,
            None => {
                self.windows.remove(win);
            }
        }
        self.active = Some(tab);
        self.enter_pane_move();
    }

    /// Grab the currently focused pane to rearrange it within its split (`Ctrl+W m`). A
    /// no-op (with a hint) unless the active window is actually split.
    pub(crate) fn grab_focused_pane_move(&mut self) {
        let Some(aw) = self.active_window() else {
            self.set_status("nothing to move — open a page first");
            return;
        };
        if !matches!(self.windows[aw], PaneNode::Split { .. }) {
            self.set_status("this tab isn't split — split it first (Ctrl+W v)");
            return;
        }
        self.pane_move_orig = Some((self.windows.clone(), self.active));
        self.enter_pane_move();
    }

    /// Common tail of both grabs: switch to move mode with the shell holding the keyboard
    /// (so h/j/k/l reach us, not the page) and the grabbed pane focused/highlighted.
    fn enter_pane_move(&mut self) {
        self.mode = ModeKind::PaneMove;
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
        self.set_status("move pane: hjkl swap · Enter set · Esc cancel");
    }

    /// Move-pane h/j/k/l: swap the grabbed (focused) pane with its neighbour that way,
    /// so the pane slides through the arrangement. Focus stays on the grabbed tab, so the
    /// yellow highlight follows it. A no-op when nothing lies that direction.
    pub(crate) fn pane_move_swap(&mut self, dir: char) {
        let Some(a) = self.active else { return };
        let Some(t) = self.pane_neighbor(dir) else { return };
        let Some(aw) = self.active_window() else { return };
        self.windows[aw].swap_leaves(a, t);
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// Commit the move: keep the current arrangement and return to Normal.
    pub(crate) fn commit_pane_move(&mut self) {
        self.pane_move_orig = None;
        self.mode = ModeKind::Normal;
        self.clear_status();
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// Cancel the move: restore the arrangement snapshotted when the pane was grabbed
    /// (undoing any swaps and, for a pulled-in tab, returning it to its own window).
    pub(crate) fn revert_pane_move(&mut self) {
        if let Some((windows, active)) = self.pane_move_orig.take() {
            self.windows = windows;
            self.active = active;
        }
        self.mode = ModeKind::Normal;
        self.clear_status();
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// `Ctrl+W b`: break the focused pane out of its split into its own standalone tab
    /// (a new tab-bar entry) — the reverse of pulling a tab in. A no-op (with a hint) when
    /// the pane is already standalone.
    pub(crate) fn break_pane(&mut self) {
        let Some(a) = self.active else { return };
        let Some(aw) = self.active_window() else { return };
        if !matches!(self.windows[aw], PaneNode::Split { .. }) {
            self.set_status("pane isn't split — nothing to break out");
            return;
        }
        let src = std::mem::replace(&mut self.windows[aw], PaneNode::Leaf(a));
        if let Some(t) = src.prune(a) {
            self.windows[aw] = t;
        }
        self.windows.push(PaneNode::Leaf(a));
        self.mode = ModeKind::Normal;
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
        self.set_status("pane broken out into its own tab");
    }

    /// Flip the focused pane's split between side-by-side and stacked (`Ctrl+W r`): turns
    /// its divider from vertical to horizontal or back, re-tiling the two panes. Only the
    /// split directly dividing the focused pane from its neighbour is flipped; ratios and
    /// any other splits are left alone. A no-op (with a hint) unless the pane is split.
    pub(crate) fn toggle_pane_orientation(&mut self) {
        let Some(a) = self.active else { return };
        let Some(aw) = self.active_window() else {
            self.set_status("nothing to flip — open a page first");
            return;
        };
        if self.windows[aw].flip_parent_split(a) {
            self.refresh_visibility();
            self.window.set_focus();
            self.window.request_redraw();
        } else {
            self.set_status("this tab isn't split — split it first (Ctrl+W v)");
        }
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

// --- Session persistence of the pane layout -------------------------------------------
// A window's tree is stored as a compact string so it drops into the TOML session as a
// plain `Vec<String>` (no fragile nested-enum tables). Grammar: a leaf is its tab index;
// a split is `R`/`C` (Row/Col) + ratio + `(a|b)`, e.g. `R0.5000(0|C0.6000(1|2))`.

/// Encode a window's pane tree to its string form, remapping each leaf's LIVE tab index
/// to a SAVED-tab index via `live_to_saved` and pruning leaves that aren't being saved
/// (collapsing the split into its surviving side, exactly like [`PaneNode::prune`]).
/// `None` when the whole tree maps away (every leaf was unsaved).
pub(crate) fn encode_window(tree: &PaneNode, live_to_saved: &[Option<usize>]) -> Option<String> {
    match tree {
        PaneNode::Leaf(t) => live_to_saved.get(*t).copied().flatten().map(|s| s.to_string()),
        PaneNode::Split { dir, ratio, a, b } => {
            let dc = if *dir == SplitDir::Row { 'R' } else { 'C' };
            match (encode_window(a, live_to_saved), encode_window(b, live_to_saved)) {
                (Some(a), Some(b)) => Some(format!("{dc}{ratio:.4}({a}|{b})")),
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
    }
}

/// Decode a window string back into a tree of LIVE tab indices, translating each stored
/// SAVED index through `saved_to_live` and pruning any tab that didn't come back (e.g. an
/// async read tab not yet restored). `None` if the string is malformed or fully pruned.
pub(crate) fn decode_window(s: &str, saved_to_live: &[Option<usize>]) -> Option<PaneNode> {
    let (tree, rest) = parse_node(s)?;
    if !rest.is_empty() {
        return None;
    }
    remap_window(tree, saved_to_live)
}

/// Recursive-descent parse of one node, returning it and the unconsumed tail. Leaves are
/// left as their stored (saved) indices; [`decode_window`] remaps them afterwards.
fn parse_node(s: &str) -> Option<(PaneNode, &str)> {
    let first = s.as_bytes().first()?;
    match first {
        b'R' | b'C' => {
            let dir = if *first == b'R' { SplitDir::Row } else { SplitDir::Col };
            let rest = &s[1..];
            let open = rest.find('(')?;
            let ratio: f32 = rest[..open].parse().ok()?;
            let (a, rest) = parse_node(&rest[open + 1..])?;
            let (b, rest) = parse_node(rest.strip_prefix('|')?)?;
            let rest = rest.strip_prefix(')')?;
            Some((PaneNode::Split { dir, ratio, a: Box::new(a), b: Box::new(b) }, rest))
        }
        _ => {
            let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
            let n: usize = s[..end].parse().ok()?;
            Some((PaneNode::Leaf(n), &s[end..]))
        }
    }
}

/// Translate a parsed tree's saved indices to live ones, pruning unmapped leaves.
fn remap_window(node: PaneNode, map: &[Option<usize>]) -> Option<PaneNode> {
    match node {
        PaneNode::Leaf(s) => map.get(s).copied().flatten().map(PaneNode::Leaf),
        PaneNode::Split { dir, ratio, a, b } => {
            match (remap_window(*a, map), remap_window(*b, map)) {
                (Some(a), Some(b)) => {
                    Some(PaneNode::Split { dir, ratio, a: Box::new(a), b: Box::new(b) })
                }
                (Some(x), None) | (None, Some(x)) => Some(x),
                (None, None) => None,
            }
        }
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
    fn swap_leaves_exchanges_positions_and_keeps_shape() {
        // Row(Leaf0, Row(Leaf1, Leaf2)): swapping 0 and 2 moves each into the other's
        // slot while the tree structure and ratios are untouched.
        let inner = split(SplitDir::Row, 1, 2);
        let mut t = PaneNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            a: Box::new(PaneNode::Leaf(0)),
            b: Box::new(inner),
        };
        t.swap_leaves(0, 2);
        assert_eq!(leaves(&t), vec![2, 1, 0]);
        // Swapping two tabs that aren't present leaves the tree unchanged.
        t.swap_leaves(7, 8);
        assert_eq!(leaves(&t), vec![2, 1, 0]);
    }

    #[test]
    fn flip_parent_split_toggles_only_the_focused_divider() {
        // Row(Leaf0, Row(Leaf1, Leaf2)): flipping from leaf 1 hits the INNER split (its
        // immediate parent), turning it Col; the outer Row is untouched.
        let inner = split(SplitDir::Row, 1, 2);
        let mut t = PaneNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            a: Box::new(PaneNode::Leaf(0)),
            b: Box::new(inner),
        };
        assert!(t.flip_parent_split(1));
        match &t {
            PaneNode::Split { dir: outer, b, .. } => {
                assert!(*outer == SplitDir::Row, "outer divider unchanged");
                match b.as_ref() {
                    PaneNode::Split { dir: innr, .. } => assert!(*innr == SplitDir::Col),
                    _ => panic!("expected inner split"),
                }
            }
            _ => panic!("expected a split"),
        }
        // A lone leaf has no parent split here → no-op, reports false.
        assert!(!PaneNode::Leaf(0).flip_parent_split(0));
    }

    #[test]
    fn encode_decode_roundtrips_and_remaps_indices() {
        // Row(Leaf0, Col(Leaf1, Leaf2)) with live indices 0,1,2.
        let inner = PaneNode::Split {
            dir: SplitDir::Col,
            ratio: 0.6,
            a: Box::new(PaneNode::Leaf(1)),
            b: Box::new(PaneNode::Leaf(2)),
        };
        let tree = PaneNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            a: Box::new(PaneNode::Leaf(0)),
            b: Box::new(inner),
        };
        // Identity save map → the canonical string.
        let id = [Some(0), Some(1), Some(2)];
        let enc = encode_window(&tree, &id).expect("encodes");
        assert_eq!(enc, "R0.5000(0|C0.6000(1|2))");
        // Restore with a shifted map (saved i → live i+10): shape/ratios preserved.
        let shift = [Some(10), Some(11), Some(12)];
        let back = decode_window(&enc, &shift).expect("decodes");
        assert_eq!(leaves(&back), vec![10, 11, 12]);
        match &back {
            PaneNode::Split { ratio, .. } => assert!((*ratio - 0.5).abs() < 1e-4),
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn decode_prunes_leaves_that_didnt_restore() {
        // A split of tabs 0 and 1 where tab 1 didn't come back (None) collapses to a lone
        // leaf 0; a leaf that's entirely gone yields None.
        let none_for_1 = [Some(5), None];
        let one = decode_window("R0.5000(0|1)", &none_for_1).expect("collapses to survivor");
        assert!(matches!(one, PaneNode::Leaf(5)));
        assert!(decode_window("3", &[None, None, None, None]).is_none());
        // Malformed strings are rejected, not panicked on.
        assert!(decode_window("R0.5(0|", &[Some(0), Some(1)]).is_none());
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
