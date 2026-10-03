//! A small lab host for the production bridge/hints, not a registered browser provider.
use super::{Lab, Message, bridge, native};
use browser_engine::EngineView;
use browser_servo_lab::key_ownership::{KeyOwnership, Route};
use std::{
    collections::HashSet,
    rc::Rc,
    time::{Duration, Instant},
};
use tao::{
    event::{ElementState, WindowEvent},
    event_loop::EventLoopProxy,
    keyboard::Key,
    window::WindowId,
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum Probe {
    Start,
    SharedCallbacks,
    InsertKey,
    InsertCheck,
    EscapeCallback,
    HintReady,
    HintPick,
    HintFocused,
    HintEmpty,
    HintTyping,
    HintTyped,
    HintVerified,
    CspNavigation,
    CspCheck,
    BackgroundReply,
    Done,
}
pub struct Shell {
    navigation: Rc<bridge::Navigation>,
    mode: &'static str,
    hint: String,
    count: usize,
    smoke: bool,
    seen: HashSet<String>,
    probe: Probe,
    due: Instant,
    background_focus: isize,
    proof: bool,
    navigation_before: u64,
    epoch: u64,
    consumed_text: Option<String>,
    owned_keys: KeyOwnership<tao::keyboard::KeyCode>,
    hint_key: Option<tao::event::KeyEvent>,
    zoom: f64,
}
impl Shell {
    pub fn new(navigation: Rc<bridge::Navigation>, smoke: bool) -> Self {
        Self {
            navigation,
            mode: "passthrough",
            hint: String::new(),
            count: 0,
            smoke,
            seen: HashSet::new(),
            probe: Probe::Start,
            due: Instant::now(),
            background_focus: 0,
            proof: false,
            navigation_before: 0,
            epoch: 0,
            consumed_text: None,
            owned_keys: KeyOwnership::default(),
            hint_key: None,
            zoom: 1.0,
        }
    }
    fn mode(&mut self, lab: &Lab, mode: &'static str) {
        self.mode = mode;
        self.hint.clear();
        dispatch(lab, &format!("window.__mode = '{}'", mode));
        let owns_focus = native::focused(&lab.servo_window) || native::focused(&lab.parent);
        if !owns_focus {
            // A delayed blur/escape message from Servo must not steal WebView2 focus.
        } else {
            let page: &dyn EngineView = &lab.page;
            let result = if mode == "normal" || mode == "hint" {
                page.focus_parent()
            } else {
                page.focus()
            };
            if let Err(error) = result {
                eprintln!("{error}");
            }
        }
        self.title(lab);
    }
    fn title(&self, lab: &Lab) {
        lab.parent.set_title(&format!("Servo [{}; {:.0}%; {} messages] | WebView2 — F6: shell/page; f: hints; i: insert; +/-/0: zoom; r: reload; H/L: history", self.mode, self.zoom * 100.0, self.count));
    }
    pub fn message(&mut self, lab: &Lab, message: &str) {
        self.sync_navigation(lab);
        // Only these explicit messages have host behavior. Page strings never become
        // native commands, scripts, filesystem operations or arbitrary log paths.
        let known = matches!(
            message,
            "page-ready"
                | "insert-escape"
                | "insert-blur"
                | "leave-passthrough"
                | "page-edit"
                | "hint-edit"
                | "hint-exit"
                | "pane-click"
                | "page-hold"
                | "grab-focus"
                | "url-changed"
                | "url-replaced"
                | "nav-intent"
                | "bridge-csp-proof"
        ) || message.starts_with("link-hover:");
        if !known {
            return;
        }
        self.count += 1;
        if self.smoke && !message.starts_with("link-hover:") {
            self.seen.insert(message.into());
        }
        match message {
            "insert-escape" | "insert-blur" | "leave-passthrough" | "hint-exit" => {
                self.mode(lab, "normal")
            }
            "hint-edit" => {
                self.mode(lab, "insert");
                dispatch(
                    lab,
                    "window.__hintTarget&&(window.__hintTarget.focus(),window.__hintTarget=null)",
                );
            }
            "page-edit" if self.mode == "normal" => self.mode(lab, "insert"),
            "grab-focus" if self.mode == "normal" && native::focused(&lab.servo_window) => {
                if let Err(error) = lab.page.focus_parent() {
                    eprintln!("{error}");
                }
            }
            _ => {}
        }
        self.title(lab);
    }
    pub fn key(&mut self, lab: &Lab, id: WindowId, event: &WindowEvent<'_>) -> bool {
        self.sync_navigation(lab);
        if (self.smoke && self.probe == Probe::Start)
            || (id != lab.servo_window.id() && id != lab.parent.id())
        {
            return false;
        }
        if matches!(event, WindowEvent::Focused(false))
            && !native::focused(&lab.parent)
            && !native::focused(&lab.servo_window)
        {
            // Releases over WebView2 or another app won't arrive at our Tao windows.
            self.owned_keys.clear();
            self.consumed_text = None;
        }
        if let WindowEvent::ReceivedImeText(text) = event {
            return self.consumed_text.take().as_deref() == Some(text.as_str())
                || self.mode == "normal"
                || self.mode == "hint";
        }
        let WindowEvent::KeyboardInput {
            event,
            is_synthetic,
            ..
        } = event
        else {
            return false;
        };
        self.keyboard(lab, event, *is_synthetic)
    }
    pub(super) fn keyboard(
        &mut self,
        lab: &Lab,
        event: &tao::event::KeyEvent,
        is_synthetic: bool,
    ) -> bool {
        let pressed = event.state == ElementState::Pressed;
        if self.smoke
            && self.mode == "hint"
            && pressed
            && !is_synthetic
            && event.logical_key == Key::Character("a")
        {
            self.hint_key = Some(event.clone());
        }
        let route = self.owned_keys.route(
            event.physical_key,
            pressed,
            is_synthetic,
            self.mode == "normal" || self.mode == "hint" || event.logical_key == Key::F6,
        );
        match route {
            Route::Page => return false,
            Route::Suppress => {
                if pressed && !is_synthetic {
                    self.consumed_text = match event.logical_key {
                        Key::Character(text) => Some(text.into()),
                        _ => None,
                    };
                }
                return true;
            }
            Route::Shell => {}
        }
        if event.logical_key == Key::F6 {
            if event.state == ElementState::Pressed && !event.repeat {
                self.mode(
                    lab,
                    if self.mode == "passthrough" {
                        "normal"
                    } else {
                        "passthrough"
                    },
                );
            }
            return true;
        }
        if self.mode != "normal" && self.mode != "hint" {
            return false;
        }
        if event.state != ElementState::Pressed {
            return true;
        }
        self.consumed_text = match event.logical_key {
            Key::Character(text) => Some(text.into()),
            _ => None,
        };
        if event.logical_key == Key::Escape {
            dispatch(lab, "window.__hintClear && window.__hintClear()");
            self.mode(lab, "normal");
        } else if let Key::Character(text) = event.logical_key {
            if self.mode == "hint" {
                if text.chars().all(|c| "asdfghjkl".contains(c)) && self.hint.len() < 16 {
                    self.hint.push_str(text);
                    dispatch(
                        lab,
                        &format!(
                            "window.__hintInput({}, 'follow')",
                            serde_json::to_string(&self.hint).unwrap()
                        ),
                    );
                }
            } else {
                match text {
                    "f" => {
                        self.mode(lab, "hint");
                        dispatch(lab, bridge::HINTS);
                    }
                    "i" => {
                        self.mode(lab, "insert");
                        dispatch(
                            lab,
                            "document.querySelector('input,textarea,[contenteditable]')?.focus()",
                        );
                    }
                    "v" => self.mode(lab, "passthrough"),
                    "+" | "=" | "-" | "0" | "r" | "H" | "L" => {
                        let page: &dyn EngineView = &lab.page;
                        let result = match text {
                            "r" => page.reload(),
                            "H" | "L" => page
                                .history()
                                .ok_or_else(|| "History unavailable".to_string())
                                .and_then(|history| history.go(text == "L")),
                            _ => {
                                let factor = match text {
                                    "+" | "=" => (self.zoom * 1.25).min(10.0),
                                    "-" => (self.zoom / 1.25).max(0.1),
                                    _ => 1.0,
                                };
                                page.zoom(factor).map(|()| self.zoom = factor)
                            }
                        };
                        if let Err(error) = result {
                            lab.parent.set_title(&error);
                        } else {
                            self.title(lab);
                        }
                    }
                    _ => {}
                }
            }
        }
        true
    }
    pub fn probe_done(&self) -> bool {
        self.probe == Probe::Done
    }
    pub fn probe_status(&self) -> String {
        format!("{:?}, mode={}, seen={:?}", self.probe, self.mode, self.seen)
    }
    fn sync_navigation(&mut self, lab: &Lab) {
        let epoch = self.navigation.epoch.get();
        if epoch != self.epoch {
            self.epoch = epoch;
            self.mode = "passthrough";
            self.hint.clear();
            self.consumed_text = None;
            self.title(lab);
        }
    }
    pub fn probe_reply(&mut self, result: Result<String, String>) -> Result<(), String> {
        let value = result?;
        if value != "true" {
            return Err(format!(
                "Shared shell bridge probe {:?} failed: {value}",
                self.probe
            ));
        }
        self.proof = true;
        Ok(())
    }
    pub fn probe(&mut self, lab: &Lab, proxy: &EventLoopProxy<Message>) -> Result<(), String> {
        match self.probe {
            Probe::Start => {
                self.seen.clear();
                self.proof = false;
                self.probe = Probe::SharedCallbacks;
                let script = format!(
                    r#"
                    window.__mode = 'insert';
                    const escaped = !document.dispatchEvent(new KeyboardEvent('keydown', {{ key:'Escape', bubbles:true, cancelable:true }}));
                    window.__mode = 'normal';
                    {}
                    const label = Object.keys(window.__hintMap).find(k => window.__hintMap[k].el.id === 'counter');
                    window.__hintInput(label, 'follow');
                    location.hash = 'bridge-route';
                    const clicks = document.querySelector('#counter').textContent;
                    escaped && clicks === 'Clicks: 1' ? 'true' : JSON.stringify({{ escaped, clicks, label }})
                "#,
                    bridge::HINTS
                );
                evaluate(lab, proxy, script);
            }
            Probe::SharedCallbacks
                if self.proof
                    && ["insert-escape", "hint-exit", "url-changed"]
                        .iter()
                        .all(|key| self.seen.contains(*key)) =>
            {
                self.proof = false;
                self.mode(lab, "normal");
                super::smoke::key(&lab.parent, 0x49, 0x17, false)?;
                self.due = Instant::now() + Duration::from_millis(400);
                self.probe = Probe::InsertKey;
            }
            Probe::InsertKey if Instant::now() >= self.due => {
                // Let Windows translate the keydown to WM_CHAR before releasing;
                // PostMessage does not update the physical keyboard-state array.
                super::smoke::key(&lab.parent, 0x49, 0x17, true)?;
                self.probe = Probe::InsertCheck;
                evaluate(lab, proxy, "window.__mode === 'insert' && document.activeElement === document.querySelector('input') && document.querySelector('input').value === '' ? 'true' : JSON.stringify([window.__mode, document.activeElement.tagName, document.querySelector('input').value])".into());
            }
            Probe::InsertCheck if self.proof => {
                self.seen.remove("insert-escape");
                super::smoke::key(&lab.servo_window, 0x1b, 0x01, false)?;
                super::smoke::key(&lab.servo_window, 0x1b, 0x01, true)?;
                self.probe = Probe::EscapeCallback;
            }
            Probe::EscapeCallback
                if self.seen.contains("insert-escape") && self.mode == "normal" =>
            {
                self.proof = false;
                super::smoke::key(&lab.parent, 0x46, 0x21, false)?;
                self.due = Instant::now() + Duration::from_millis(400);
                self.probe = Probe::HintReady;
            }
            Probe::HintReady if Instant::now() >= self.due => {
                super::smoke::key(&lab.parent, 0x46, 0x21, true)?;
                self.probe = Probe::HintPick;
                evaluate(
                    lab,
                    proxy,
                    "String(window.__hintMap?.a?.el === document.querySelector('input'))".into(),
                );
            }
            Probe::HintPick if self.proof => {
                self.proof = false;
                self.seen.remove("hint-edit");
                super::smoke::key(&lab.parent, 0x41, 0x1e, false)?;
                self.probe = Probe::HintFocused;
            }
            Probe::HintFocused if self.seen.contains("hint-edit") && self.mode == "insert" => {
                let event = self.hint_key.take().ok_or("Missing native hint key")?;
                proxy
                    .send_event(Message::ProbeFocusKey(event))
                    .map_err(|_| "Smoke loop closed")?;
                // Repeat the still-held hint key over the newly focused input.
                super::smoke::key(&lab.servo_window, 0x41, 0x1e, false)?;
                self.due = Instant::now() + Duration::from_millis(400);
                self.probe = Probe::HintEmpty;
            }
            Probe::HintEmpty if Instant::now() >= self.due => {
                super::smoke::key(&lab.servo_window, 0x41, 0x1e, true)?;
                self.probe = Probe::HintTyping;
                evaluate(lab, proxy, "document.activeElement === document.querySelector('input') && document.querySelector('input').value === '' ? 'true' : JSON.stringify([document.activeElement.tagName, document.querySelector('input').value])".into());
            }
            Probe::HintTyping if self.proof => {
                self.proof = false;
                super::smoke::key(&lab.servo_window, 0x41, 0x1e, false)?;
                self.due = Instant::now() + Duration::from_millis(400);
                self.probe = Probe::HintTyped;
            }
            Probe::HintTyped if Instant::now() >= self.due => {
                super::smoke::key(&lab.servo_window, 0x41, 0x1e, true)?;
                self.probe = Probe::HintVerified;
                evaluate(
                    lab,
                    proxy,
                    "String(document.querySelector('input').value === 'a')".into(),
                );
            }
            Probe::HintVerified if self.proof => {
                eprintln!(
                    "Hint input probe: focus replay and held-key repeat inserted nothing; a fresh press inserted one character"
                );
                self.navigation_before = self.navigation.epoch.get();
                self.seen.clear();
                self.proof = false;
                self.probe = Probe::CspNavigation;
                let html = "<!doctype html><meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; script-src 'none'\"><title>CSP bridge qualification</title><script>window.cspInlineRan = true</script><p>Shared shell bridge under strict CSP.</p>";
                let url = url::Url::parse(&format!(
                    "data:text/html,{}",
                    url::form_urlencoded::byte_serialize(html.as_bytes())
                        .collect::<String>()
                        .replace('+', "%20")
                ))
                .map_err(|e| e.to_string())?;
                lab.page.load_url(url.as_str())?;
            }
            Probe::CspNavigation
                if self.navigation.epoch.get() > self.navigation_before
                    && self.seen.contains("page-ready") =>
            {
                self.probe = Probe::CspCheck;
                evaluate(lab, proxy, "window.__post('bridge-csp-proof'); String(window.__shellBridge === document && window.__mode === 'passthrough' && window.cspInlineRan === undefined && document.title === 'CSP bridge qualification')".into());
            }
            Probe::CspCheck if self.proof && self.seen.contains("bridge-csp-proof") => {
                lab.webview2.focus().map_err(|e| e.to_string())?;
                self.background_focus = super::native::focused_handle();
                self.proof = false;
                self.seen.remove("insert-escape");
                self.probe = Probe::BackgroundReply;
                evaluate(lab, proxy, "window.__post('insert-escape'); 'true'".into());
            }
            Probe::BackgroundReply if self.proof && self.seen.contains("insert-escape") => {
                if self.background_focus == 0
                    || super::native::focused_handle() != self.background_focus
                {
                    return Err("Background Servo bridge message stole WebView2 focus".into());
                }
                eprintln!(
                    "Bridge probe: Escape, hints, native Insert/Escape, SPA URL event, full navigation, strict CSP and background focus isolation passed"
                );
                self.probe = Probe::Done;
            }
            _ => {}
        }
        Ok(())
    }
}
fn dispatch(lab: &Lab, script: &str) {
    let page: &dyn EngineView = &lab.page;
    if let Err(error) = page.evaluate_script(script) {
        eprintln!("{error}");
    }
}
fn evaluate(lab: &Lab, proxy: &EventLoopProxy<Message>, script: String) {
    let proxy = proxy.clone();
    lab.page.raw().evaluate_javascript(script, move |result| {
        let result = match result {
            Ok(servo::JSValue::String(value)) => Ok(value),
            other => Err(format!("Bridge probe evaluation: {other:?}")),
        };
        let _ = proxy.send_event(Message::BridgeProbe(result));
    });
}
