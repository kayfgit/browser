//! `:freeze` / `:unfreeze`: shut the web engine down while keeping every tab.
//!
//! Suspending pages isn't enough: WebView2's browser, GPU and utility processes keep
//! running and hold most of its memory. So freezing drops every web tab's view, which
//! lets the engine exit, and leaves a [`TabContent::Frozen`] placeholder with the tab's
//! URL and flags; unfreezing builds the views again. Pages reload on unfreeze (scroll
//! position and form contents are lost); the tab's own back/forward history is kept.
//! Terminals keep running: freezing them would stop whatever they're doing.

use crate::tabs::TabContent;
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
            let tab = &self.tabs[i];
            let extra = if tab.research { RESEARCH_JS } else { "" };
            let candidate = self.build_provider_view(
                &provider.clone(),
                crate::Source::Url(tab.url.clone()),
                tab.nojs,
                extra,
                tab.private,
            );
            if let Err(error) = self.tabs[i].replace_engine(candidate) {
                failed += 1;
                let tab = &self.tabs[i];
                let provider = tab.provider().unwrap_or("webview2").to_string();
                self.tabs[i].content =
                    crate::engines::unavailable_content(&provider, &tab.url, &format!("{error:#}"));
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
