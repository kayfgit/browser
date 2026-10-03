//! Only live view incarnations can deliver page events to the shell.
use crate::tabs::Tab;
use crate::{App, UserEvent};
use browser_engine::ViewId;
use tao::event_loop::EventLoopProxy;

#[derive(Clone)]
pub(crate) struct PageEventProxy {
    proxy: EventLoopProxy<UserEvent>,
    view: ViewId,
}

impl PageEventProxy {
    pub(crate) fn new(proxy: EventLoopProxy<UserEvent>, view: ViewId) -> Self {
        Self { proxy, view }
    }
    pub(crate) fn send_event(&self, event: UserEvent) -> Result<(), ()> {
        self.proxy
            .send_event(UserEvent::Engine {
                view: self.view,
                event: Box::new(event),
            })
            .map_err(|_| ())
    }
}

/// Decode a message the shared page scripts sent with `window.__post`. Every engine
/// uses this one table, so a message added to the scripts reaches all of them. Messages
/// only one engine can act on (WebView2's `nav-intent`) are handled by that adapter first.
pub(crate) fn decode_page_message(body: &str) -> Option<UserEvent> {
    Some(match body {
        // Insert (light field typing): Esc / focus left the field → back to Normal.
        "leave-passthrough" | "insert-escape" | "insert-blur" => UserEvent::ExitToNormal,
        "page-ready" => UserEvent::FocusShell,
        // SPA URL changes (pushState/popstate/hashchange) — no document load fires, so
        // this is the only signal the shell gets. 'url-changed' records a back/forward
        // step; 'url-replaced' (replaceState) only syncs the shown URL.
        "url-changed" => UserEvent::UrlChanged { record: true },
        "url-replaced" => UserEvent::UrlChanged { record: false },
        "grab-focus" => UserEvent::GrabFocus,
        "page-hold" => UserEvent::PageHold,
        "page-edit" => UserEvent::PageEdit,
        // Esc reached the page in Normal mode: give the keyboard back to the shell.
        "reclaim" => UserEvent::ReclaimNormal,
        "pane-click" => UserEvent::PaneClick,
        "hint-exit" => UserEvent::ExitHint,
        "hint-edit" => UserEvent::HintEdit,
        "scroll-selected" => UserEvent::ScrollSelected,
        "scroll-exit" => UserEvent::ScrollExit,
        "caret-exit" => UserEvent::CaretExit,
        // Right-click menu: "Inspect" and "View page source".
        "inspect" => UserEvent::Inspect,
        "view-source" => UserEvent::ViewSource,
        "fs-enter" => UserEvent::PageFullscreen(true),
        "fs-exit" => UserEvent::PageFullscreen(false),
        body => {
            let (kind, arg) = body.split_once(':')?;
            match kind {
                // Web caret-mode yanked a selection.
                "caret-yank" => UserEvent::CaretYank(arg.into()),
                // A shell key reached the page in Normal mode (see `shellKey` in
                // bridge.js): `shell-key:<keyCode>,<shift>,<ctrl>`.
                "shell-key" => {
                    let mut parts = arg.split(',');
                    let vk = parts.next()?.parse::<u16>().ok().filter(|&v| v != 0)?;
                    let shift = parts.next() == Some("1");
                    let ctrl = parts.next() == Some("1");
                    UserEvent::ReplayToShell(crate::shellkeys::KeyReplay::from_vk(vk, shift, ctrl))
                }
                // A right-click menu item copied something (the selection, a link
                // address, an image address).
                "clip" => UserEvent::ClipCopy(arg.into()),
                // A hint in new-tab mode resolved to a link.
                "hint-open" => UserEvent::HintOpen(arg.into()),
                // A hint picked a control: `hint-click:<x>,<y>` asks for a trusted click.
                "hint-click" => {
                    let (x, y) = arg.split_once(',')?;
                    let (x, y) = (x.parse::<f64>().ok()?, y.parse::<f64>().ok()?);
                    if !x.is_finite() || !y.is_finite() {
                        return None;
                    }
                    UserEvent::HintClick(x, y)
                }
                // A hint in copy mode (`yf`) resolved to a link.
                "hint-copy" => UserEvent::HintCopy(arg.into()),
                // The page blocker neutered a scripted pop-up.
                "popup-blocked" => UserEvent::PopupBlocked(arg.into()),
                // The pointer moved onto/off a link (empty = off).
                "link-hover" => UserEvent::LinkHover(arg.into()),
                _ => return None,
            }
        }
    })
}

pub(super) fn event_target(tabs: &[Tab], view: ViewId) -> Option<usize> {
    tabs.iter()
        .position(|tab| tab.webview().is_some_and(|v| v.identity().id == view))
}

fn active_ui_event(event: UserEvent, source: usize, active: Option<usize>) -> Option<UserEvent> {
    if active != Some(source) {
        return None;
    }
    match event {
        event @ (UserEvent::ExitToNormal
        | UserEvent::FocusShell
        | UserEvent::GrabFocus
        | UserEvent::PageHold
        | UserEvent::PageEdit
        | UserEvent::ReclaimNormal
        | UserEvent::ReplayToShell(_)
        | UserEvent::RestoreDefaults
        | UserEvent::LinkHover(_)
        | UserEvent::ExitHint
        | UserEvent::HintEdit
        | UserEvent::ScrollSelected
        | UserEvent::ScrollExit
        | UserEvent::HintCopy(_)
        | UserEvent::CaretYank(_)
        | UserEvent::ClipCopy(_)
        | UserEvent::CaretExit
        | UserEvent::PageFullscreen(_)) => Some(event),
        _ => None,
    }
}

impl App {
    pub(crate) fn view_by_id(&self, view: ViewId) -> Option<&dyn browser_engine::EngineView> {
        self.tabs
            .get(event_target(&self.tabs, view)?)
            .and_then(Tab::webview)
    }

    /// Handle source-scoped events here; only active-view UI events reach the
    /// existing modal handler. An outgoing view's ID is never reused by its replacement.
    pub(crate) fn route_engine_event(
        &mut self,
        view: ViewId,
        event: UserEvent,
    ) -> Option<UserEvent> {
        let index = event_target(&self.tabs, view)?;
        match event {
            UserEvent::PaneClick => {
                let visible = self.pane_layout().0.iter().any(|(tab, _)| *tab == index);
                if visible && !self.frozen {
                    self.page_gesture_at = Some(std::time::Instant::now());
                    self.focus_pane_click(index);
                }
                None
            }
            UserEvent::UrlChanged { record } => {
                self.refresh_tab_url_record(index, record);
                None
            }
            UserEvent::SyncAdblock => {
                if let Some(view) = self.tabs[index].webview() {
                    let _ = view.evaluate_script(&format!(
                        "window.__setAdblock&&window.__setAdblock({})",
                        self.adblock.blocking()
                    ));
                }
                None
            }
            UserEvent::FocusShell if self.active != Some(index) => {
                self.refresh_tab_url_record(index, true);
                if let Some(view) = self.tabs[index].webview() {
                    let _ = view.zoom(self.content_zoom);
                }
                None
            }
            UserEvent::Navigate(url) => {
                if let Some(view) = self.tabs[index].webview() {
                    match view.load_url(&url) {
                        Ok(()) => self.tabs[index].url = url,
                        Err(error) => self.set_error(error),
                    }
                }
                None
            }
            UserEvent::OpenPopupTab(url) => {
                let tab = &self.tabs[index];
                let provider = tab.provider().unwrap().to_string();
                let private = tab.private;
                self.open_web_provider(&url, self.nojs, false, true, private, &provider);
                None
            }
            UserEvent::HintOpen(url) if self.active == Some(index) => {
                let provider = self.tabs[index].provider().unwrap().to_string();
                let private = self.tabs[index].private;
                self.hint_input.clear();
                self.hint_act = crate::HintAct::Follow;
                self.mode = crate::ModeKind::Normal;
                self.window.set_focus();
                self.open_web_provider(&url, self.nojs, false, true, private, &provider);
                None
            }
            UserEvent::Inspect => {
                self.inspect_tab(index);
                None
            }
            UserEvent::ViewSource => {
                self.view_source_of(index);
                None
            }
            UserEvent::HintClick(x, y) => {
                if self.active == Some(index) && self.mode == crate::ModeKind::Hint {
                    if let Some(view) = self.tabs[index].webview() {
                        if view.trusted_click(x, y).is_err() {
                            let _ = view
                                .evaluate_script("window.__hintFallback&&window.__hintFallback()");
                        }
                    }
                }
                None
            }
            // These notify the user, but cannot change another tab's document or mode.
            event @ (UserEvent::RedirectBlocked(_)
            | UserEvent::PopupBlocked(_)
            | UserEvent::DownloadBlocked(_)
            | UserEvent::Redraw) => Some(event),
            event => active_ui_event(event, index, self.active),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn background_pages_cannot_change_focus_modes_clipboard_or_fullscreen() {
        for event in [
            UserEvent::FocusShell,
            UserEvent::ExitToNormal,
            UserEvent::HintEdit,
            UserEvent::ScrollSelected,
            UserEvent::ScrollExit,
            UserEvent::PageFullscreen(true),
            UserEvent::ClipCopy("background".into()),
        ] {
            assert!(active_ui_event(event, 1, Some(0)).is_none());
        }
        assert!(matches!(
            active_ui_event(UserEvent::HintEdit, 1, Some(1)),
            Some(UserEvent::HintEdit)
        ));
        assert!(active_ui_event(UserEvent::Quit, 1, Some(1)).is_none());
    }

    #[test]
    fn only_the_active_page_can_hand_keys_back_to_the_shell() {
        let key = crate::shellkeys::KeyReplay::from_vk(0xBA, true, false);
        assert!(matches!(
            active_ui_event(UserEvent::ReplayToShell(key), 1, Some(1)),
            Some(UserEvent::ReplayToShell(k)) if k == key
        ));
        assert!(active_ui_event(UserEvent::ReplayToShell(key), 1, Some(0)).is_none());
        assert!(matches!(
            active_ui_event(UserEvent::ReclaimNormal, 1, Some(1)),
            Some(UserEvent::ReclaimNormal)
        ));
    }

    #[test]
    fn page_messages_decode_for_every_engine() {
        assert!(matches!(
            decode_page_message("hint-click:12.5,40"),
            Some(UserEvent::HintClick(x, y)) if x == 12.5 && y == 40.0
        ));
        let key = crate::shellkeys::KeyReplay::from_vk(0xBA, true, false);
        assert!(matches!(
            decode_page_message("shell-key:186,1,0"),
            Some(UserEvent::ReplayToShell(k)) if k == key
        ));
        assert!(matches!(
            decode_page_message("reclaim"),
            Some(UserEvent::ReclaimNormal)
        ));
        // Text after the first colon is kept whole: URLs contain colons.
        assert!(matches!(
            decode_page_message("hint-open:https://example.com:8080/a"),
            Some(UserEvent::HintOpen(u)) if u == "https://example.com:8080/a"
        ));
        assert!(matches!(
            decode_page_message("link-hover:"),
            Some(UserEvent::LinkHover(u)) if u.is_empty()
        ));
        for bad in [
            "hint-click:NaN,1",
            "hint-click:1",
            "shell-key:0,0,0",
            "shell-key:x",
            "nav-intent",
            "unknown",
        ] {
            assert!(decode_page_message(bad).is_none(), "{bad}");
        }
    }
}
