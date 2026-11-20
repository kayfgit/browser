//! `:freeze` / `:unfreeze`: shut the web engine down while keeping every tab.
//!
//! Suspending pages isn't enough: WebView2's browser, GPU and utility processes keep
//! running and hold most of its memory. So freezing drops every web tab's view, which
//! lets the engine exit, and leaves a [`TabContent::Frozen`] placeholder with the tab's
//! URL and flags; unfreezing builds the views again. Pages reload on unfreeze (scroll
//! position and form contents are lost); the tab's own back/forward history is kept.
//! Terminals keep running: freezing them would stop whatever they're doing.
//!
//! A restored session uses the same idea from the other end: its web tabs come back
//! as [`TabContent::Unloaded`] placeholders, and each is built the first time it's on
//! screen ([`App::load_shown_tabs`]), so opening the browser loads only what you see.

use crate::panes::PaneRect;
use crate::tabs::{Tab, TabContent};
use crate::{App, RESEARCH_JS};

impl App {
    /// `:freeze` — drop every web tab's view so the engine exits. Idempotent.
    pub(crate) fn freeze(&mut self) {
        if self.frozen {
            self.set_status("already frozen — :unfreeze to resume");
            return;
        }
        let mut n = 0usize;
        for tab in &mut self.tabs {
            let Some(view) = tab.webview() else { continue };
            // The live address: a page that navigated in place is frozen where it is.
            let url = view.url().ok().filter(|u| !u.is_empty());
            let provider = view.identity().provider.clone();
            if let Some(url) = url {
                tab.url = url;
            }
            tab.content = TabContent::Frozen { provider };
            n += 1;
        }
        // A profile switch may still hold the engine open with a hidden view.
        if self.engine_keepalive.is_some() {
            self.release_engine();
        }
        self.frozen = true;
        self.hover_link = None;
        self.refresh_visibility();
        self.set_status(if n == 0 {
            "frozen — no web tabs were open".to_string()
        } else {
            let plural = if n == 1 { "tab" } else { "tabs" };
            format!("frozen {n} web {plural}, engine stopped — :unfreeze to reload them")
        });
        self.window.request_redraw();
    }

    /// `:unfreeze` — rebuild every frozen tab's view, reloading its page.
    pub(crate) fn unfreeze(&mut self) {
        if !self.frozen {
            self.set_status("not frozen");
            return;
        }
        self.frozen = false;
        let mut failed = 0usize;
        for i in 0..self.tabs.len() {
            let TabContent::Frozen { provider } = &self.tabs[i].content else {
                continue;
            };
            if !self.build_tab_view(i, provider.clone()) {
                failed += 1;
            }
        }
        self.refresh_visibility();
        self.apply_active_zoom();
        if failed == 0 {
            self.set_status("unfrozen");
        } else {
            self.set_error(format!(
                "unfrozen, but {failed} tab(s) couldn't be reopened"
            ));
        }
        self.window.request_redraw();
    }
}

impl App {
    /// Build a view for tab `i` (a frozen or unloaded placeholder) from its URL and
    /// flags with `provider`. A tab whose engine can't be created becomes an
    /// "unavailable" page that keeps the URL; returns false then.
    fn build_tab_view(&mut self, i: usize, provider: String) -> bool {
        let tab = &self.tabs[i];
        let extra = if tab.research { RESEARCH_JS } else { "" };
        let candidate = self.build_provider_view(
            &provider,
            crate::Source::Url(tab.url.clone()),
            tab.nojs,
            extra,
            tab.private,
        );
        match self.tabs[i].replace_engine(candidate) {
            Ok(()) => true,
            Err(error) => {
                let url = self.tabs[i].url.clone();
                self.tabs[i].content =
                    crate::engines::unavailable_content(&provider, &url, &format!("{error:#}"));
                false
            }
        }
    }

    /// Build every [`TabContent::Unloaded`] tab that's on screen now. Called from
    /// `refresh_visibility`, which every tab switch and layout change goes through,
    /// so a restored tab loads the moment you first look at it. Does nothing while
    /// frozen (`:unfreeze` leaves unloaded tabs for this to build as they're shown)
    /// or while a session restore is still placing its tabs.
    pub(crate) fn load_shown_tabs(&mut self) {
        if self.frozen || self.defer_tab_loads {
            return;
        }
        let (panes, _) = self.pane_layout();
        let shown = shown_unloaded(&panes, &self.tabs);
        if shown.is_empty() {
            return;
        }
        for i in shown {
            let TabContent::Unloaded { provider } = &self.tabs[i].content else {
                continue;
            };
            let provider = provider.clone();
            crate::journal::log("tabs", format!("loading restored tab {i} on first show"));
            self.build_tab_view(i, provider);
        }
        self.apply_active_zoom();
    }
}

/// The tabs in `panes` (the ones on screen) that are still unloaded placeholders.
fn shown_unloaded(panes: &[(usize, PaneRect)], tabs: &[Tab]) -> Vec<usize> {
    panes
        .iter()
        .map(|(i, _)| *i)
        .filter(|&i| {
            tabs.get(i)
                .is_some_and(|t| matches!(t.content, TabContent::Unloaded { .. }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unloaded(url: &str) -> Tab {
        let mut tab = Tab::blank();
        tab.url = url.into();
        tab.content = TabContent::Unloaded {
            provider: "webview2".into(),
        };
        tab
    }

    fn rect() -> PaneRect {
        PaneRect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        }
    }

    #[test]
    fn only_unloaded_tabs_on_screen_get_loaded() {
        // 0: unloaded and shown, 1: unloaded but in a background tab, 2: blank pane
        // shown beside 0, 3: unloaded and shown in the other half of a split.
        let tabs = vec![
            unloaded("a.com"),
            unloaded("b.com"),
            Tab::blank(),
            unloaded("c.com"),
        ];
        let panes = vec![(0, rect()), (2, rect()), (3, rect())];
        assert_eq!(shown_unloaded(&panes, &tabs), vec![0, 3]);
    }

    #[test]
    fn nothing_on_screen_loads_nothing() {
        let tabs = vec![unloaded("a.com"), unloaded("b.com")];
        assert!(shown_unloaded(&[], &tabs).is_empty());
        // A stale pane index past the end is ignored, not a panic.
        assert!(shown_unloaded(&[(5, rect())], &tabs).is_empty());
    }
}
