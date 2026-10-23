//! Undo and redo for layout changes (`U` / `R`, `:undo` / `:redo`): closing a tab or
//! pane, splitting, moving, swapping or breaking out a pane, flipping a split,
//! resizing panes, reordering the tab strip. Each change keeps the layout before and
//! after it, in [`TabId`]s rather than tab indices (which shift as tabs close), plus
//! what it opened and closed, so undoing a close reopens the page where it was. `u`
//! puts a closed tab back the same way, from the layout it closed in ([`ClosedTab`]).
//!
//! Opening pages isn't a layout change here, and a tab that disappears some other way
//! (a terminal whose shell exits) simply drops out of the layouts that mention it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::panes::PaneNode;
use crate::session::SavedTab;
use crate::tabs::Tab;
use crate::{App, ModeKind};

/// How many changes `U` can go back.
const LIMIT: usize = 50;
/// Resizes (or strip moves) closer together than this are one change.
const MERGE_WINDOW: Duration = Duration::from_millis(1500);

/// A tab's identity for its whole life, unlike its index. Replacing a pane's content
/// (opening another page in it) keeps the id: it's the same slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TabId(u64);

impl TabId {
    pub(crate) fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        TabId(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// The tab strip's pane trees and the focused tab, with [`TabId`]s at the leaves.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Layout {
    windows: Vec<PaneNode>,
    active: Option<TabId>,
}

/// What a change was: for the status message, and to merge bursts of one kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ChangeKind {
    Close,
    Split,
    Move,
    BreakOut,
    Flip,
    Resize,
    Reorder,
}

impl ChangeKind {
    fn label(self) -> &'static str {
        match self {
            ChangeKind::Close => "the close",
            ChangeKind::Split => "the split",
            ChangeKind::Move => "the pane move",
            ChangeKind::BreakOut => "the break-out",
            ChangeKind::Flip => "the flip",
            ChangeKind::Resize => "the resize",
            ChangeKind::Reorder => "the tab move",
        }
    }
}

/// A closed tab, as undo will reopen it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Reopen {
    /// An empty pane.
    Blank,
    /// A page or terminal, as `u` would reopen it.
    Saved(SavedTab),
}

/// A tab on the `u` stack: how to reopen it, and where it was.
#[derive(Clone, Debug)]
pub(crate) struct ClosedTab {
    pub(crate) saved: SavedTab,
    pub(crate) id: TabId,
    /// The layout just before it closed.
    before: Layout,
}

struct Change {
    kind: ChangeKind,
    before: Layout,
    after: Layout,
    /// Empty panes the change opened (a split's new pane).
    created: Vec<TabId>,
    /// Tabs the change closed.
    closed: Vec<(TabId, Reopen)>,
    at: Instant,
}

#[derive(Default)]
pub(crate) struct LayoutHistory {
    undo: Vec<Change>,
    redo: Vec<Change>,
}

impl LayoutHistory {
    fn push(&mut self, change: Change) {
        self.redo.clear();
        if let Some(last) = self.undo.last_mut() {
            let burst = matches!(change.kind, ChangeKind::Resize | ChangeKind::Reorder)
                && change.kind == last.kind
                && change.at.duration_since(last.at) < MERGE_WINDOW;
            if burst {
                last.after = change.after;
                last.at = change.at;
                return;
            }
        }
        self.undo.push(change);
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
    }
}

/// Rewrite a tree's leaves with `f`, dropping those it maps to `None` (a split left
/// with one side collapses into it).
fn map_leaves(node: &PaneNode, f: &mut impl FnMut(usize) -> Option<usize>) -> Option<PaneNode> {
    match node {
        PaneNode::Leaf(t) => f(*t).map(PaneNode::Leaf),
        PaneNode::Split { dir, ratio, a, b } => match (map_leaves(a, f), map_leaves(b, f)) {
            (Some(a), Some(b)) => Some(PaneNode::Split {
                dir: *dir,
                ratio: *ratio,
                a: Box::new(a),
                b: Box::new(b),
            }),
            (Some(n), None) | (None, Some(n)) => Some(n),
            (None, None) => None,
        },
    }
}

/// `windows` (tab indices) as a [`Layout`] (tab ids), given each tab's id.
fn to_layout(windows: &[PaneNode], active: Option<usize>, ids: &[TabId]) -> Layout {
    let mut id_of = |i: usize| ids.get(i).map(|id| id.0 as usize);
    Layout {
        windows: windows
            .iter()
            .filter_map(|tree| map_leaves(tree, &mut id_of))
            .collect(),
        active: active.and_then(|i| ids.get(i).copied()),
    }
}

/// Push the splits on the way down to leaf `id`, nearest first: each one with whether
/// the leaf is on its first side. False when `node` doesn't hold the leaf.
fn path_to<'a>(node: &'a PaneNode, id: usize, path: &mut Vec<(&'a PaneNode, bool)>) -> bool {
    match node {
        PaneNode::Leaf(t) => *t == id,
        PaneNode::Split { a, b, .. } => {
            let first = path_to(a, id, path);
            if first || path_to(b, id, path) {
                path.push((node, first));
                return true;
            }
            false
        }
    }
}

fn leaves_of(node: &PaneNode) -> Vec<usize> {
    let mut out = Vec::new();
    node.leaves(&mut out);
    out
}

/// `node` with its first subtree that `pick` matches (top-down) replaced by `with` of it.
fn replace_node(
    node: &PaneNode,
    pick: &impl Fn(&PaneNode) -> bool,
    with: &impl Fn(&PaneNode) -> PaneNode,
) -> Option<PaneNode> {
    if pick(node) {
        return Some(with(node));
    }
    let PaneNode::Split { dir, ratio, a, b } = node else {
        return None;
    };
    let split = |a: PaneNode, b: PaneNode| PaneNode::Split {
        dir: *dir,
        ratio: *ratio,
        a: Box::new(a),
        b: Box::new(b),
    };
    match replace_node(a, pick, with) {
        Some(a) => Some(split(a, (**b).clone())),
        None => replace_node(b, pick, with).map(|b| split((**a).clone(), b)),
    }
}

/// `now` with tab `new` put back where tab `old` sat in `before` (the layout just
/// before it closed), focused. It goes back beside the panes it shared a split with
/// (same side, same direction and size) if any of them are still open — the nearest
/// split first, so a pane whose neighbour closed too lands next to the next one out.
/// Otherwise it's its own window again, at its old place in the strip (after the
/// nearest window before it that's still open). `old` still being open — the page was
/// replaced, not closed — puts it in a window right after that one.
fn put_back(now: &Layout, before: &Layout, old: TabId, new: TabId) -> Layout {
    let (old, new) = (old.0 as usize, new.0 as usize);
    // `new` already sits in a window of its own; take it out to place it.
    let mut windows: Vec<PaneNode> = now
        .windows
        .iter()
        .filter_map(|tree| map_leaves(tree, &mut |t| (t != new).then_some(t)))
        .collect();
    let leaf = PaneNode::Leaf(new);
    let window_of =
        |windows: &[PaneNode], tab: usize| windows.iter().position(|t| t.contains_leaf(tab));
    let placed = |windows| Layout {
        windows,
        active: Some(TabId(new as u64)),
    };
    if let Some(w) = window_of(&windows, old) {
        windows.insert(w + 1, leaf);
        return placed(windows);
    }
    let Some(w) = window_of(&before.windows, old) else {
        windows.push(leaf);
        return placed(windows);
    };
    let open = |tab: &usize| window_of(&windows, *tab).is_some();
    let mut path = Vec::new();
    path_to(&before.windows[w], old, &mut path);
    for (split, first) in path {
        let PaneNode::Split { dir, ratio, a, b } = split else {
            continue;
        };
        let other = if first { b } else { a };
        let mut survivors: Vec<usize> = leaves_of(other).into_iter().filter(open).collect();
        let Some(&anchor) = survivors.first() else {
            continue;
        };
        let wi = window_of(&windows, anchor).expect("an open tab has a window");
        // The whole neighbouring subtree if it's still together, else just one of its panes.
        survivors.sort_unstable();
        let whole = |n: &PaneNode| {
            let mut l = leaves_of(n);
            l.sort_unstable();
            l == survivors
        };
        let wrap = |n: &PaneNode| {
            let (a, b) = if first {
                (leaf.clone(), n.clone())
            } else {
                (n.clone(), leaf.clone())
            };
            PaneNode::Split {
                dir: *dir,
                ratio: *ratio,
                a: Box::new(a),
                b: Box::new(b),
            }
        };
        let tree = &windows[wi];
        windows[wi] = replace_node(tree, &whole, &wrap)
            .or_else(|| replace_node(tree, &|n| *n == PaneNode::Leaf(anchor), &wrap))
            .expect("the anchor pane is in its window");
        return placed(windows);
    }
    // Its own window: after the nearest earlier window still open, else before the
    // nearest later one.
    let first_open = |tree: &PaneNode| {
        leaves_of(tree)
            .into_iter()
            .find_map(|t| window_of(&windows, t))
    };
    let at = before.windows[..w]
        .iter()
        .rev()
        .find_map(|t| first_open(t).map(|i| i + 1))
        .or_else(|| before.windows[w + 1..].iter().find_map(first_open))
        .unwrap_or(windows.len());
    windows.insert(at, leaf);
    placed(windows)
}

/// A [`Layout`] as tab-index trees for the tabs that exist now (`ids`, in tab order),
/// dropping panes whose tab is gone; `unplaced` lists the tabs it doesn't mention.
fn from_layout(layout: &Layout, ids: &[TabId]) -> (Vec<PaneNode>, Option<usize>) {
    let index_of = |id: u64| ids.iter().position(|t| t.0 == id);
    let windows = layout
        .windows
        .iter()
        .filter_map(|tree| map_leaves(tree, &mut |id| index_of(id as u64)))
        .collect();
    (windows, layout.active.and_then(|id| index_of(id.0)))
}

impl App {
    fn tab_ids(&self) -> Vec<TabId> {
        self.tabs.iter().map(|t| t.id).collect()
    }

    /// The current layout, for recording a change.
    pub(crate) fn layout_now(&self) -> Layout {
        to_layout(&self.windows, self.active, &self.tab_ids())
    }

    /// `windows` and `active` (tab indices, in the current tabs) as a [`Layout`].
    pub(crate) fn layout_of(&self, windows: &[PaneNode], active: Option<usize>) -> Layout {
        to_layout(windows, active, &self.tab_ids())
    }

    /// Record a layout change that took the layout from `before` to what it is now.
    pub(crate) fn record_layout(
        &mut self,
        kind: ChangeKind,
        before: Layout,
        created: Vec<TabId>,
        closed: Vec<(TabId, Reopen)>,
    ) {
        let after = self.layout_now();
        if before == after && created.is_empty() && closed.is_empty() {
            return;
        }
        self.layout_history.push(Change {
            kind,
            before,
            after,
            created,
            closed,
            at: Instant::now(),
        });
    }

    /// How tab `i` would be reopened after closing it: an empty pane, or the page or
    /// terminal as `u` reopens it. `None` for tabs that can't come back (internal
    /// pages, private tabs).
    pub(crate) fn reopen_info(&self, i: usize) -> Option<Reopen> {
        let tab = self.tabs.get(i)?;
        if tab.is_blank() {
            Some(Reopen::Blank)
        } else {
            self.saved_tab(i).map(Reopen::Saved)
        }
    }

    /// Tab `i` as `u` reopens it, and where it is now; `None` for tabs that can't come
    /// back (see [`saved_tab`](Self::saved_tab)).
    pub(crate) fn closed_tab(&self, i: usize) -> Option<ClosedTab> {
        Some(ClosedTab {
            saved: self.saved_tab(i)?,
            id: self.tabs[i].id,
            before: self.layout_now(),
        })
    }

    /// Open `saved` again as tab `id`, in a window of its own for now. False when
    /// nothing opened.
    pub(crate) fn reopen_saved(&mut self, saved: &SavedTab, id: TabId) -> bool {
        let count = self.tabs.len();
        // Nothing active: every placement path opens a new window instead of filling
        // the focused pane.
        self.active = None;
        match saved.kind.as_str() {
            "term" => self.open_terminal_at((!saved.cwd.is_empty()).then_some(&saved.cwd)),
            "read" => self.reopen_read(&saved.url, id),
            _ => self.restore_web_tab(saved),
        }
        let opened = self.tabs.len() > count;
        if opened {
            self.tabs[count].id = id;
        }
        opened
    }

    /// Put tab `id`, just reopened from `closed`, back where `closed` was.
    pub(crate) fn put_back_closed(&mut self, closed: &ClosedTab, id: TabId) {
        let target = put_back(&self.layout_now(), &closed.before, closed.id, id);
        self.set_layout(&target);
    }

    /// `U` / `:undo` — undo the last layout change.
    pub(crate) fn undo_layout(&mut self) {
        let Some(change) = self.layout_history.undo.pop() else {
            self.set_status("no layout change to undo");
            return;
        };
        self.reopen_tabs(&change.closed);
        // A pane the change opened goes again, unless something was opened in it
        // since: that keeps its own tab.
        for id in &change.created {
            if let Some(i) = self.tabs.iter().position(|t| t.id == *id && t.is_blank()) {
                self.drop_tab(i);
            }
        }
        self.set_layout(&change.before);
        self.set_status(format!("undid {} — R redoes it", change.kind.label()));
        self.layout_history.redo.push(change);
    }

    /// `R` / `:redo` — redo the last undone layout change.
    pub(crate) fn redo_layout(&mut self) {
        let Some(change) = self.layout_history.redo.pop() else {
            self.set_status("nothing to redo");
            return;
        };
        let created: Vec<(TabId, Reopen)> = change
            .created
            .iter()
            .map(|id| (*id, Reopen::Blank))
            .collect();
        self.reopen_tabs(&created);
        for (id, _) in &change.closed {
            if let Some(i) = self.tabs.iter().position(|t| t.id == *id) {
                self.record_closed(i);
                if let Some(session) = self.tabs[i].take_term() {
                    session.shutdown();
                }
                self.drop_tab(i);
            }
        }
        self.set_layout(&change.after);
        self.set_status(format!("redid {}", change.kind.label()));
        self.layout_history.undo.push(change);
    }

    /// Bring back closed tabs under their old ids, each as its own window for now
    /// ([`set_layout`](Self::set_layout) then puts it in place).
    fn reopen_tabs(&mut self, tabs: &[(TabId, Reopen)]) {
        for (id, how) in tabs {
            if self.tabs.iter().any(|t| t.id == *id) {
                continue;
            }
            match how {
                Reopen::Blank => {
                    self.active = None;
                    let i = self.push_tab_window(Tab::blank());
                    self.tabs[i].id = *id;
                }
                Reopen::Saved(saved) => {
                    // It's back in place, so `u` mustn't reopen it a second time.
                    if let Some(p) = self
                        .closed_tabs
                        .iter()
                        .rposition(|c| c.id == *id && c.saved == *saved)
                    {
                        self.closed_tabs.remove(p);
                    }
                    self.reopen_saved(saved, *id);
                }
            }
        }
    }

    /// Arrange the tabs as `target` says. Panes whose tab is gone are dropped, and a
    /// tab it doesn't mention keeps a window of its own.
    fn set_layout(&mut self, target: &Layout) {
        let (mut windows, active) = from_layout(target, &self.tab_ids());
        let mut placed = Vec::new();
        for tree in &windows {
            tree.leaves(&mut placed);
        }
        for (i, tab) in self.tabs.iter().enumerate() {
            if tab.ai().is_none() && !placed.contains(&i) {
                windows.push(PaneNode::Leaf(i));
            }
        }
        self.windows = windows;
        self.active = active
            .or(self.active.filter(|&a| a < self.tabs.len()))
            .or_else(|| self.windows.first().map(|w| self.pane_focus.target(w)));
        self.mode = ModeKind::Normal;
        self.find_reset();
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::panes::SplitDir;

    fn split(a: usize, b: usize) -> PaneNode {
        PaneNode::Split {
            dir: SplitDir::Row,
            ratio: 0.5,
            a: Box::new(PaneNode::Leaf(a)),
            b: Box::new(PaneNode::Leaf(b)),
        }
    }

    #[test]
    fn layouts_survive_tab_indices_shifting() {
        let ids = [TabId::new(), TabId::new(), TabId::new()];
        // Tab 0 alone, tabs 1 and 2 split; tab 2 focused.
        let before = to_layout(&[PaneNode::Leaf(0), split(1, 2)], Some(2), &ids);
        // Tab 0 closes: the others are now at 0 and 1.
        let (windows, active) = from_layout(&before, &ids[1..]);
        assert_eq!(windows, vec![split(0, 1)]);
        assert_eq!(active, Some(1));
    }

    #[test]
    fn a_pane_whose_tab_is_gone_collapses_its_split() {
        let ids = [TabId::new(), TabId::new()];
        let layout = to_layout(&[split(0, 1)], Some(1), &ids);
        let (windows, active) = from_layout(&layout, &ids[..1]);
        assert_eq!(windows, vec![PaneNode::Leaf(0)]);
        assert_eq!(active, None);
    }

    fn tree(dir: SplitDir, ratio: f32, a: PaneNode, b: PaneNode) -> PaneNode {
        PaneNode::Split {
            dir,
            ratio,
            a: Box::new(a),
            b: Box::new(b),
        }
    }

    /// A layout over tab ids 1, 2, … written straight into the leaves.
    fn lay(windows: Vec<PaneNode>) -> Layout {
        Layout {
            windows,
            active: None,
        }
    }

    const X: usize = 9;
    const N: usize = 10;
    use PaneNode::Leaf;

    /// Put tab 9 (`X`) back from `before`; it reopened as itself, its own window at
    /// the end of `now` like a new tab.
    fn back(now: Vec<PaneNode>, before: Vec<PaneNode>) -> Vec<PaneNode> {
        let mut now = now;
        now.push(Leaf(X));
        let placed = put_back(&lay(now), &lay(before), TabId(X as u64), TabId(X as u64));
        assert_eq!(placed.active, Some(TabId(X as u64)));
        placed.windows
    }

    #[test]
    fn a_closed_tab_comes_back_at_its_place_in_the_strip() {
        let before = vec![Leaf(1), Leaf(X), Leaf(2)];
        assert_eq!(back(vec![Leaf(1), Leaf(2)], before.clone()), before);
        // Its left neighbour closed too: it goes before the right one.
        assert_eq!(back(vec![Leaf(2)], before.clone()), vec![Leaf(X), Leaf(2)]);
        // The strip was reordered since: it follows its old left neighbour.
        assert_eq!(
            back(vec![Leaf(2), Leaf(1)], before),
            vec![Leaf(2), Leaf(1), Leaf(X)]
        );
        // Nothing it knew is left: the end.
        assert_eq!(back(vec![Leaf(3)], vec![Leaf(X)]), vec![Leaf(3), Leaf(X)]);
    }

    #[test]
    fn a_closed_pane_goes_back_into_its_split() {
        use SplitDir::{Col, Row};
        // The right side of a 30/70 split.
        let before = vec![tree(Row, 0.3, Leaf(1), Leaf(X))];
        assert_eq!(back(vec![Leaf(1)], before.clone()), before);
        // Beside a neighbour that is itself split, and on the left.
        let before = vec![tree(Row, 0.5, Leaf(X), tree(Col, 0.5, Leaf(1), Leaf(2)))];
        let now = vec![Leaf(3), tree(Col, 0.5, Leaf(1), Leaf(2))];
        assert_eq!(back(now, before.clone()), [vec![Leaf(3)], before].concat());
        // Its neighbour closed too: next to the pane beyond it.
        let before = vec![tree(Row, 0.4, Leaf(1), tree(Col, 0.5, Leaf(X), Leaf(2)))];
        assert_eq!(
            back(vec![Leaf(1)], before),
            vec![tree(Row, 0.4, Leaf(1), Leaf(X))]
        );
        // Its neighbour moved into another split: it follows that pane.
        let before = vec![tree(Col, 0.5, Leaf(1), Leaf(X))];
        let now = vec![tree(Row, 0.5, Leaf(2), Leaf(1))];
        assert_eq!(
            back(now, before),
            vec![tree(Row, 0.5, Leaf(2), tree(Col, 0.5, Leaf(1), Leaf(X)))]
        );
    }

    #[test]
    fn a_replaced_page_comes_back_next_to_its_tab() {
        let now = lay(vec![Leaf(1), Leaf(2), Leaf(N)]);
        let placed = put_back(
            &now,
            &lay(vec![Leaf(1), Leaf(2)]),
            TabId(1),
            TabId(N as u64),
        );
        assert_eq!(placed.windows, vec![Leaf(1), Leaf(N), Leaf(2)]);
    }

    fn change(kind: ChangeKind, n: usize) -> Change {
        let layout = Layout {
            windows: vec![PaneNode::Leaf(n)],
            active: None,
        };
        Change {
            kind,
            before: layout.clone(),
            after: layout,
            created: Vec::new(),
            closed: Vec::new(),
            at: Instant::now(),
        }
    }

    #[test]
    fn a_burst_of_resizes_is_one_change_and_new_changes_clear_redo() {
        let mut h = LayoutHistory::default();
        h.push(change(ChangeKind::Resize, 1));
        h.push(change(ChangeKind::Resize, 2));
        assert_eq!(h.undo.len(), 1);
        assert_eq!(h.undo[0].after.windows, vec![PaneNode::Leaf(2)]);
        h.push(change(ChangeKind::Close, 3));
        assert_eq!(h.undo.len(), 2);
        h.redo.push(change(ChangeKind::Split, 4));
        h.push(change(ChangeKind::Flip, 5));
        assert!(h.redo.is_empty());
    }

    #[test]
    fn history_keeps_the_last_changes_only() {
        let mut h = LayoutHistory::default();
        for n in 0..LIMIT + 5 {
            h.push(change(ChangeKind::Close, n));
        }
        assert_eq!(h.undo.len(), LIMIT);
        assert_eq!(h.undo[0].before.windows, vec![PaneNode::Leaf(5)]);
    }
}
