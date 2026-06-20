//! Hint mode (`f`/`F`): label clickable things and follow the picked one.
//! Web tabs inject HINT_JS and filter in-page; native read tabs place labels
//! over visible links and match them shell-side.

use tao::event::KeyEvent;
use tao::keyboard::Key;

use crate::{read_view, App, ModeKind, HINT_JS};

/// A placed hint label over a native read link: the typed label and target URL.
pub(crate) struct NativeHint {
    pub(crate) label: String,
    pub(crate) url: String,
    pub(crate) x: i32,
    pub(crate) y: i32,
}

/// Generate `n` fixed-width, prefix-free hint labels from the home-row charset
/// (matches the web HINT_JS scheme, so the muscle memory is the same).
pub(crate) fn hint_labels(n: usize) -> Vec<String> {
    const CH: &[u8] = b"asdfghjkl";
    if n == 0 {
        return Vec::new();
    }
    let (mut width, mut cap) = (1usize, CH.len());
    while cap < n {
        width += 1;
        cap *= CH.len();
    }
    (0..n)
        .map(|i| {
            let mut buf = vec![0u8; width];
            let mut x = i;
            for w in 0..width {
                buf[width - 1 - w] = CH[x % CH.len()];
                x /= CH.len();
            }
            String::from_utf8(buf).unwrap()
        })
        .collect()
}

impl App {
    /// `new_tab`: enter with `F` to follow the picked link in a NEW tab (badges
    /// render uppercase). `f` (false) follows in the current tab.
    pub(crate) fn enter_hint(&mut self, new_tab: bool) {
        let Some(idx) = self.active else {
            self.set_status("no page — open one first");
            return;
        };
        self.hint_new_tab = new_tab;
        // Engine-free read tab: hints are computed and drawn natively.
        if self.tabs[idx].native().is_some() {
            self.hint_input.clear();
            self.mode = ModeKind::Hint;
            self.build_native_hints();
            if self.native_hints.is_empty() {
                self.mode = ModeKind::Normal;
                self.set_status("no links on screen");
            }
            self.window.request_redraw();
            return;
        }
        if self.tabs[idx].nojs {
            self.set_status("hint mode needs JavaScript (this tab is no-js)");
            return;
        }
        self.hint_input.clear();
        self.mode = ModeKind::Hint;
        if let Some(wv) = self.tabs[idx].webview() {
            let _ = wv.evaluate_script(&format!("window.__hintUpper={new_tab};"));
            let _ = wv.evaluate_script(HINT_JS);
        }
    }

    /// Place hint labels over the links currently visible in the native read tab.
    pub(crate) fn build_native_hints(&mut self) {
        self.native_hints.clear();
        let Some(i) = self.active else { return };
        // Place hints within the focused pane's rect (offset by its left edge).
        let pane = self.focused_pane_rect();
        let (top, bottom) = (pane.y, pane.y + pane.h);
        let painter = &self.painter;
        let Some(nr) = self.tabs[i].native() else { return };
        let links = read_view::visible_links(&nr.layout, nr.scroll, top, bottom, painter);
        let labels = hint_labels(links.len());
        let mut hints = Vec::with_capacity(links.len());
        for ((id, x, y), label) in links.into_iter().zip(labels) {
            if let Some(url) = nr.doc.link_url(id) {
                // +8 to match the content's left draw margin; +pane.x for the column.
                hints.push(NativeHint { label, url: url.to_string(), x: pane.x + x + 8, y });
            }
        }
        self.native_hints = hints;
    }

    pub(crate) fn key_hint(&mut self, key: &KeyEvent) {
        let native = !self.native_hints.is_empty();
        match &key.logical_key {
            Key::Escape => self.exit_hint(),
            Key::Backspace => {
                self.hint_input.pop();
                if native {
                    self.window.request_redraw();
                } else {
                    self.hint_send();
                }
            }
            Key::Character(s) => {
                let c = *s;
                if !c.is_empty() && c.chars().all(|ch| ch.is_ascii_alphabetic()) {
                    // Typing a label uppercase switches this pick to new-tab mode,
                    // even if hint mode was entered with plain `f`.
                    if c.chars().any(|ch| ch.is_ascii_uppercase()) {
                        self.hint_new_tab = true;
                    }
                    self.hint_input.push_str(&c.to_lowercase());
                    if native {
                        self.hint_match_native();
                    } else {
                        self.hint_send();
                    }
                }
            }
            _ => {}
        }
    }

    /// Live feedback while picking a hint: holding Shift flips every badge to
    /// UPPERCASE (and arms new-tab mode); releasing it returns to lowercase. The
    /// actual open then follows whatever Shift state is held when a label completes.
    pub(crate) fn on_modifiers_changed(&mut self) {
        if self.mode != ModeKind::Hint {
            return;
        }
        let shift = self.modifiers.shift_key();
        if shift == self.hint_new_tab {
            return;
        }
        self.hint_new_tab = shift;
        if self.native_hints.is_empty() {
            self.hint_send(); // repaint the page's badges in the new case
        } else {
            self.window.request_redraw();
        }
    }

    /// Forward the current label string to the page to filter/activate hints.
    pub(crate) fn hint_send(&self) {
        if let Some(wv) = self.active_webview() {
            // Following a hint may navigate this tab cross-site via a synthetic click the
            // native guard would otherwise read as a forced redirect — stamp intent so it
            // passes. (Harmless when the hint resolves to a button/in-page action.)
            crate::navguard::mark(&self.nav_intent);
            let _ = wv.evaluate_script(&format!(
                "window.__hintInput&&window.__hintInput({:?},{})",
                self.hint_input, self.hint_new_tab
            ));
        }
    }

    /// Native hint input: on an exact label match, follow the link (re-extract it
    /// into the current read tab); reset if the typed prefix matches nothing.
    pub(crate) fn hint_match_native(&mut self) {
        if let Some(h) = self.native_hints.iter().find(|h| h.label == self.hint_input) {
            let url = h.url.clone();
            let new_tab = self.hint_new_tab;
            self.exit_hint();
            // New-tab mode opens a fresh read tab; otherwise follow in place.
            self.start_read(&url, !new_tab, true);
            return;
        }
        if !self.native_hints.iter().any(|h| h.label.starts_with(&self.hint_input)) {
            self.hint_input.clear();
        }
        self.window.request_redraw();
    }

    pub(crate) fn exit_hint(&mut self) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script("window.__hintClear&&window.__hintClear()");
        }
        self.native_hints.clear();
        self.hint_input.clear();
        self.hint_new_tab = false;
        self.mode = ModeKind::Normal;
        self.window.request_redraw();
    }
}
