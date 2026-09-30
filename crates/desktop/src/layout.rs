//! Undo and redo for layout changes (`U` / `R`, `:undo` / `:redo`): closing a tab or
//! pane, splitting, moving, swapping or breaking out a pane, flipping a split,
//! resizing panes, reordering the tab strip. Each change keeps the layout before and
//! after it, in [`TabId`]s rather than tab indices (which shift as tabs close), plus
//! what it opened and closed, so undoing a close reopens the page where it was.
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
    /// ([`set_layout`](Self::set_layout) then puts it in place). A `:read` page loads
    /// asynchronously, so it comes back as a new tab instead, like `u`.
    fn reopen_tabs(&mut self, tabs: &[(TabId, Reopen)]) {
        for (id, how) in tabs {
            if self.tabs.iter().any(|t| t.id == *id) {
                continue;
            }
            let count = self.tabs.len();
            // Nothing active: every placement path opens a new window instead of
            // filling the focused pane.
            self.active = None;
            match how {
                Reopen::Blank => {
                    self.push_tab_window(Tab::blank());
                }
                Reopen::Saved(saved) => {
                    // It's back in place, so `u` mustn't reopen it a second time.
                    if let Some(p) = self.closed_tabs.iter().rposition(|c| c == saved) {
                        self.closed_tabs.remove(p);
                    }
                    match saved.kind.as_str() {
                        "term" => {
                            self.open_terminal_at((!saved.cwd.is_empty()).then_some(&saved.cwd))
                        }
                        "read" => self.start_read(&saved.url, false, true),
                        _ => self.restore_web_tab(saved),
                    }
                }
            }
            if self.tabs.len() > count {
                self.tabs[count].id = *id;
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
