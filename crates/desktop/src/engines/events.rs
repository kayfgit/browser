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
}
