//! Glance (after Zen browser's): a link opened in a floating card over the page you
//! were on, which stays in view, dimmed, behind it. A quick look without leaving the
//! page: close it (`x`, `Esc`, or a click on the dimmed page) to go back, or `t` to
//! keep it as a real tab.
//!
//! Ways in: `q` hints label the links to glance at (as `f` does to follow them), the
//! right-click menu has "Open in glance", `:glance <url>` opens one, and
//! `:glance clicks on` makes plain link clicks open in a glance, as Zen does.
//!
//! The card is a real shell tab (keys, hints, find, caret all work in it) that
//! belongs to no tab-strip window — like the `:ai` tab — and is drawn over its
//! opener's window ([`glance_rect`]). It lives only while it has focus: the
//! moment focus moves elsewhere (another tab, a click on the page behind, an
//! `:open -t`), [`App::settle_glance`] closes it.

use browser_engine::Source;

use crate::layout::TabId;
use crate::panes::{PaneNode, PaneRect};
use crate::tabs::{TabContent, TabNav};
use crate::{App, ModeKind, Tab};

/// Marks the glance view's own pages: a link clicked inside a glance opens there,
/// not in another glance (see `bridge.js`).
const GLANCE_INIT: &str = "window.__glanceInGlance = true;";

/// The open glance: its tab and the tab whose page it floats over.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Glance {
    pub(crate) tab: TabId,
    pub(crate) opener: TabId,
}

/// The card's place in the content band: most of it, centred, so a rim of the page
/// behind stays in view.
pub(crate) fn glance_rect(band: PaneRect) -> PaneRect {
    let w = (band.w * 82 / 100).max(band.w.min(400));
    let h = band.h * 92 / 100;
    PaneRect {
        x: band.x + (band.w - w) / 2,
        y: band.y + (band.h - h) / 2,
        w,
        h,
    }
}

impl App {
    /// The glance's tab index, if one is open.
    pub(crate) fn glance_index(&self) -> Option<usize> {
        let g = self.glance?;
        self.tabs.iter().position(|t| t.id == g.tab)
    }

    /// Whether the focused tab is the open glance.
    pub(crate) fn active_is_glance(&self) -> bool {
        self.active.is_some() && self.active == self.glance_index()
    }

    /// Open `target` (an address or a search, as for `:open`) in a glance over the
    /// focused page. A glance asked for from inside a glance loads in the same card.
    /// With no page to float over (the welcome screen, the `:ai` tab) it opens as a
    /// new tab instead.
    pub(crate) fn open_glance(&mut self, target: &str) {
        if self.frozen {
            self.set_status("browser is frozen — :unfreeze first");
            return;
        }
        let url = self.resolve_target(target);
        if let Some(i) = self.glance_index() {
            if let Some(view) = self.tabs[i].webview() {
                if let Err(e) = view.load_url(&url) {
                    self.set_error(format!("glance: {e}"));
                }
            }
            self.active = Some(i);
            self.refresh_visibility();
            return;
        }
        let Some(opener) = self
            .active
            .filter(|&a| self.windows.iter().any(|w| w.contains_leaf(a)))
        else {
            self.open_tab(&url, self.nojs, true);
            return;
        };
        let private = self.tabs[opener].private;
        let provider = self.default_engine().to_string();
        match self.build_provider_view(
            &provider,
            Source::Url(url.clone()),
            self.nojs,
            GLANCE_INIT,
            private,
        ) {
            Ok((view, page)) => {
                if !private {
                    self.record_history(&url);
                }
                let tab = Tab {
                    id: TabId::new(),
                    content: TabContent::Web(view, page),
                    url,
                    nojs: self.nojs,
                    research: false,
                    read: false,
                    private,
                    nav: TabNav {
                        settling: true,
                        ..TabNav::default()
                    },
                };
                self.glance = Some(Glance {
                    tab: tab.id,
                    opener: self.tabs[opener].id,
                });
                self.tabs.push(tab);
                self.active = Some(self.tabs.len() - 1);
                self.mode = ModeKind::Normal;
                self.dim_opener(true);
                self.find_reset();
                self.refresh_visibility();
                self.window.set_focus();
                self.set_status("glance — t: keep as a tab · x / Esc: close");
            }
            Err(e) => self.set_error(format!("glance: {e:#}")),
        }
    }

    /// Close the glance and return to the page behind it.
    pub(crate) fn close_glance(&mut self) {
        let Some(i) = self.glance_index() else {
            self.glance = None;
            return;
        };
        let opener = self
            .glance
            .and_then(|g| self.tabs.iter().position(|t| t.id == g.opener));
        self.dim_opener(false);
        self.glance = None;
        // Where focus goes: the tab it already moved to, else the page behind.
        let keep = self.active.filter(|&a| a != i).or(opener);
        let fallback = self.drop_tab(i);
        let shift = |a: usize| if a > i { a - 1 } else { a };
        self.active = keep.map(shift).or(fallback);
        if self.mode != ModeKind::Command {
            self.mode = ModeKind::Normal;
        }
        self.refresh_visibility();
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// `t` in a glance: keep it as a real tab of its own, right after the page it
    /// floated over.
    pub(crate) fn keep_glance(&mut self) {
        let Some(i) = self.glance_index() else { return };
        let opener = self
            .glance
            .and_then(|g| self.tabs.iter().position(|t| t.id == g.opener));
        self.dim_opener(false);
        self.glance = None;
        let after = opener
            .and_then(|o| self.windows.iter().position(|w| w.contains_leaf(o)))
            .map_or(self.windows.len(), |w| w + 1);
        self.windows.insert(after, PaneNode::Leaf(i));
        self.active = Some(i);
        self.refresh_visibility();
        self.set_status("kept the glance as a tab");
    }

    /// Called once per event: a glance only lives while it has focus. Anything that
    /// moved focus off it — another tab, a click on the dimmed page, an `:open -t` —
    /// closes it here, after the event is done with its tab indices.
    pub(crate) fn settle_glance(&mut self) {
        if self.glance.is_some() && !self.active_is_glance() {
            self.close_glance();
        }
    }

    /// Dim (or un-dim) the page the glance floats over, so the card stands out; a
    /// click on the dimmed page closes the glance (`bridge.js`).
    fn dim_opener(&self, on: bool) {
        let Some(g) = self.glance else { return };
        if let Some(view) = self
            .tabs
            .iter()
            .find(|t| t.id == g.opener)
            .and_then(Tab::webview)
        {
            let _ = view.evaluate_script(&format!("window.__glanceDim&&window.__glanceDim({on})"));
        }
    }

    /// The panes to show while the glance has focus: its opener's window, as usual,
    /// with the card on top. `None` when no glance is focused.
    pub(crate) fn glance_layout(
        &self,
        band: PaneRect,
        panes: &mut Vec<(usize, PaneRect)>,
        divs: &mut Vec<PaneRect>,
    ) -> bool {
        let Some(i) = self.glance_index().filter(|_| self.active_is_glance()) else {
            return false;
        };
        let opener = self
            .glance
            .and_then(|g| self.tabs.iter().position(|t| t.id == g.opener));
        if let Some(w) = opener.and_then(|o| self.windows.iter().position(|w| w.contains_leaf(o))) {
            self.windows[w].layout(band, panes, divs);
        }
        panes.push((i, glance_rect(band)));
        true
    }

    /// `:glance clicks on|off` — whether a plain link click opens a glance (Zen's
    /// default) instead of following the link. Saved; pages pick it up at once.
    pub(crate) fn set_glance_clicks(&mut self, on: bool) {
        self.config.glance_clicks = on;
        crate::config::save(&self.config);
        let js = format!("window.__glanceClicks={on};");
        for tab in &self.tabs {
            if let Some(view) = tab.webview() {
                let _ = view.evaluate_script(&js);
            }
        }
        self.set_status(if on {
            "link clicks now open in a glance — :glance clicks off to follow them again"
        } else {
            "link clicks follow the link again — q hints still open a glance"
        });
    }

    /// `:glance …`: `clicks [on|off]` sets (or toggles) glance-on-click; anything else
    /// is an address or search to open in a glance.
    pub(crate) fn glance_command(&mut self, rest: &str) {
        match rest.split_whitespace().collect::<Vec<_>>().as_slice() {
            [] => self.set_status(if self.config.glance_clicks {
                "glance: link clicks open in a glance (q hints too) — :glance clicks off"
            } else {
                "glance: q hints or the right-click menu open one — :glance clicks on for every link click"
            }),
            ["clicks"] => self.set_glance_clicks(!self.config.glance_clicks),
            ["clicks", "on"] => self.set_glance_clicks(true),
            ["clicks", "off"] => self.set_glance_clicks(false),
            _ => self.open_glance(rest),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_is_centred_and_leaves_a_rim_of_the_page() {
        let band = PaneRect {
            x: 0,
            y: 30,
            w: 1000,
            h: 600,
        };
        let r = glance_rect(band);
        assert_eq!((r.w, r.h), (820, 552));
        assert_eq!((r.x, r.y), (90, 54));
        // A narrow band still gives the card a usable width.
        let narrow = glance_rect(PaneRect { w: 450, ..band });
        assert_eq!(narrow.w, 400);
    }
}
