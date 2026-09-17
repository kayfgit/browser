//! Provider selection and recovery of saved locations.
use crate::tabs::{Tab, TabContent, TabNav};
use crate::{session, vim};
use crate::{App, ModeKind, Source, RESEARCH_JS};

impl App {
    pub(crate) fn default_engine(&self) -> &str {
        self.config.engine.as_deref().unwrap_or("webview2")
    }

    pub(crate) fn provider_for_open(&self, new_tab: bool) -> String {
        if !new_tab {
            if let Some(provider) =
                self.active.and_then(|i| self.tabs.get(i)).and_then(Tab::provider)
            {
                return provider.into();
            }
        }
        self.default_engine().into()
    }

    pub(crate) fn engine_command(&mut self, args: &str) {
        let words: Vec<_> = args.split_whitespace().collect();
        match words.as_slice() {
            [] => {
                let active = self
                    .active
                    .and_then(|i| self.tabs.get(i))
                    .and_then(Tab::provider)
                    .unwrap_or("none");
                self.set_status(format!("engine: {active}; default: {}", self.default_engine()));
            }
            ["default", name] => match super::resolve(name, self.default_engine()) {
                Ok(provider) => {
                    self.config.engine = Some(provider.id.into());
                    crate::config::save(&self.config);
                    self.set_status(format!(
                        "new web tabs use {} ({})",
                        provider.id, provider.family
                    ));
                }
                Err(error) => self.set_error(error),
            },
            [name] => self.switch_engine(name),
            _ => self.set_error("usage: :engine [<provider> | default <provider>]"),
        }
    }

    pub(crate) fn list_engines(&mut self) {
        let mut lines = vec!["Installed engines".into(), String::new()];
        for provider in super::PROVIDERS {
            let default = if provider.id == self.default_engine() { " (default)" } else { "" };
            lines.push(format!(
                "{} — {} — {}{default}",
                provider.id, provider.family, provider.display_name
            ));
            lines.push(format!(
                "  private: {}; no-JS pages: {}; shell bridge: {}",
                provider.capabilities.private,
                provider.capabilities.disable_javascript,
                provider.capabilities.document_scripts && provider.capabilities.page_messages
            ));
            if provider.id == "servo" {
                lines.push("  Experimental: top-level shell bridge; separate storage; no extensions/uBlock, downloads or full IME; limited site compatibility.".into());
            }
        }
        lines.extend([
            String::new(),
            "Only providers listed above can be selected. Gecko is not installed.".into(),
            ":engine blink selects the installed Blink provider (currently webview2).".into(),
            ":engine default <provider> changes new tabs; :engine <provider> reopens this pane."
                .into(),
            "Engine changes reopen the URL; live page state and sign-ins do not transfer.".into(),
        ]);
        let mut tab = Tab::blank();
        tab.url = "browser://engines".into();
        tab.content = TabContent::Pager(vim::TextBuffer::new(lines));
        self.place_tab(tab, true);
        self.window.set_focus();
    }

    pub(crate) fn open_web_provider(
        &mut self,
        target: &str,
        nojs: bool,
        research: bool,
        new_tab: bool,
        private: bool,
        provider: &str,
    ) {
        if self.frozen {
            self.set_status("browser is frozen — :unfreeze first");
            return;
        }
        let url = self.resolve_target(target);
        let extra = if research { RESEARCH_JS } else { "" };
        match self.build_provider_view(provider, Source::Url(url.clone()), nojs, extra, private) {
            Ok((view, page)) => {
                if !private {
                    self.record_history(&url);
                }
                self.place_tab_escaping_split(
                    Tab {
                        content: TabContent::Web(view, page),
                        url,
                        nojs,
                        research,
                        private,
                        read: false,
                        nav: TabNav { settling: true, ..TabNav::default() },
                    },
                    new_tab,
                );
                self.window.set_focus();
                let mode = if research {
                    "research — media stripped"
                } else if nojs {
                    "no-js"
                } else {
                    ""
                };
                self.set_status(if private {
                    format!("(private{sep}{mode})", sep = if mode.is_empty() { "" } else { ", " })
                } else if mode.is_empty() {
                    String::new()
                } else {
                    format!("({mode})")
                });
            }
            Err(error) => self.set_error(format!("failed to open: {error:#}")),
        }
    }

    /// Restore/reopen must preserve a missing provider and its URL without loading
    /// the location in an unrelated engine. The placeholder itself uses no runtime.
    pub(crate) fn restore_web_tab(&mut self, saved: &session::SavedTab) {
        let nojs = saved.kind == "nojs";
        let research = saved.kind == "research";
        let extra = if research { RESEARCH_JS } else { "" };
        let candidate = if self.frozen {
            Err(anyhow::anyhow!("browser is frozen — :unfreeze first"))
        } else {
            self.build_provider_view(
                &saved.provider,
                Source::Url(saved.url.clone()),
                nojs,
                extra,
                false,
            )
        };
        let content = match candidate {
            Ok((view, page)) => TabContent::Web(view, page),
            Err(error) => unavailable_content(&saved.provider, &saved.url, &format!("{error:#}")),
        };
        self.place_tab_escaping_split(
            Tab {
                content,
                url: saved.url.clone(),
                nojs,
                research,
                private: false,
                read: false,
                nav: TabNav { settling: true, ..TabNav::default() },
            },
            true,
        );
    }

    pub(crate) fn switch_engine(&mut self, requested: &str) {
        let provider = match super::resolve(requested, self.default_engine()) {
            Ok(provider) => provider,
            Err(error) => {
                self.set_error(error);
                return;
            }
        };
        if self.frozen {
            self.set_status("browser is frozen — :unfreeze first");
            return;
        }
        let Some(index) = self.active else {
            self.set_error("open a web page first");
            return;
        };
        let tab = &self.tabs[index];
        if tab.provider().is_none() || tab.url.starts_with("browser://") {
            self.set_error("engine selection requires a web page or an unavailable-engine tab");
            return;
        }
        if tab.webview().is_some() && tab.provider() == Some(provider.id) {
            self.set_status(format!("already using {} ({})", provider.id, provider.family));
            return;
        }
        // Only reopen ordinary GET-style locations. Other schemes need provider-specific handling.
        let url = self.current_url().unwrap_or_else(|| tab.url.clone());
        if !url.starts_with("https://") && !url.starts_with("http://") {
            self.set_error("engine switching currently requires an HTTP(S) location");
            return;
        }
        let extra = if tab.research { RESEARCH_JS } else { "" };
        let candidate = self.build_provider_view(
            provider.id,
            Source::Url(url.clone()),
            tab.nojs,
            extra,
            tab.private,
        );
        match self.tabs[index].replace_engine(candidate) {
            Ok(()) => {
                self.tabs[index].url = url;
                self.find_reset();
                self.hint_input.clear();
                self.hint_act = crate::HintAct::Follow;
                self.mode = ModeKind::Normal;
                self.page_focus_yielded = false;
                self.hover_link = None;
                self.refresh_visibility();
                self.apply_active_zoom();
                self.reclaim_shell_focus();
                self.set_status(format!("using {} ({})", provider.id, provider.family));
            }
            Err(error) => self.set_error(format!("engine unchanged: {error:#}")),
        }
    }
}

pub(super) fn unavailable_content(provider: &str, url: &str, error: &str) -> TabContent {
    TabContent::Unavailable {
        provider: provider.into(),
        error: error.into(),
        buffer: vim::TextBuffer::new(vec![
            format!("Engine unavailable: {provider}"),
            String::new(),
            url.into(),
            String::new(),
            error.into(),
            String::new(),
            "The saved provider and URL have been kept.".into(),
            "Install its provider and :reload to retry, or :engine webview2 to reopen here.".into(),
        ]),
    }
}
