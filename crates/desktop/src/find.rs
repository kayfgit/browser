//! Find-in-page: the `/` prompt state, native match scanning over read/pager
//! lines, and the web-tab handoff to the injected `__find` script.

use crate::{js_string, App, ModeKind};

/// A find-in-page match on a native (read/vim) tab: a line index plus the char
/// range `[start, end)` within that line's plain text.
pub(crate) struct NativeMatch {
    pub(crate) line: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

/// Find-in-page state, shared across tab types. For web tabs the matches live in
/// the page (injected JS); for native read/vim tabs they live in `matches`.
#[derive(Default)]
pub(crate) struct FindState {
    /// The confirmed query (`n`/`N` navigate it while this is non-empty + `active`).
    pub(crate) query: String,
    /// Whether a search is live (highlights shown, `n`/`N` active).
    pub(crate) active: bool,
    /// Matches on a native tab; empty for web tabs (JS owns those).
    pub(crate) matches: Vec<NativeMatch>,
    /// Index of the current match within `matches`.
    pub(crate) current: usize,
}

impl App {
    /// Open the `/` find prompt: searches live as you type; Enter keeps the
    /// highlights (then `n`/`N` step through matches), Esc cancels.
    pub(crate) fn enter_find(&mut self) {
        self.find_clear();
        self.mode = ModeKind::Find;
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        self.cursor_on = true;
        // Reveal the bar over a fullscreen page (it hides again on leaving Find).
        self.relayout_active();
    }

    /// Live-update the search from the current `/` input.
    pub(crate) fn find_update(&mut self) {
        let q = self.command.clone();
        self.find.query = q.clone();
        self.find_search(&q, true);
        self.window.request_redraw();
    }

    /// Confirm the search: keep the highlights and enable `n`/`N`, or clear it if
    /// the query is empty (or matched nothing on a native tab).
    pub(crate) fn find_confirm(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        self.mode = ModeKind::Normal;
        let native_empty = self.active_webview().is_none() && self.find.matches.is_empty();
        if self.find.query.is_empty() || native_empty {
            self.find_clear();
        } else {
            self.find.active = true;
            if self.active_webview().is_none() {
                self.set_status(self.find_count_label());
            }
            // On a read tab, drop a caret on the matched word so hjkl move relative to
            // it (j below, k above) instead of scrolling the page from the top.
            if self.active_is_read_native() {
                if let Some(&NativeMatch { line, start, .. }) =
                    self.find.matches.get(self.find.current)
                {
                    self.place_read_caret_at(line, start);
                }
            }
        }
        // Leaving Find re-hides the bar over a fullscreen page; refit the page.
        self.relayout_active();
    }

    /// Drop the active search and clear any highlights (page-side and native).
    pub(crate) fn find_clear(&mut self) {
        self.find.active = false;
        self.find.query.clear();
        self.find.matches.clear();
        self.find.current = 0;
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script("window.__findClear&&window.__findClear()");
        }
        self.window.request_redraw();
    }

    /// Reset find state without touching page highlights — used when the active tab
    /// changes (the old page's highlights fade on its own; `n`/`N` shouldn't carry).
    pub(crate) fn find_reset(&mut self) {
        self.find.active = false;
        self.find.query.clear();
        self.find.matches.clear();
        self.find.current = 0;
    }

    /// Run a search for `q` on the active tab (web → injected JS; read/vim → a
    /// native match list). `reveal` scrolls/moves to the first match.
    pub(crate) fn find_search(&mut self, q: &str, reveal: bool) {
        self.find.matches.clear();
        self.find.current = 0;
        // Web tab: hand off to the page's injected search.
        if let Some(wv) = self.active_webview() {
            let js = if q.is_empty() {
                "window.__findClear&&window.__findClear()".to_string()
            } else {
                format!("window.__find&&window.__find({})", js_string(q))
            };
            let _ = wv.evaluate_script(&js);
            return;
        }
        if q.is_empty() {
            return;
        }
        // Native tab: match against this tab's lines (the borrow ends before the
        // match list is stored).
        let matches = self.find_native_lines().map(|lines| find_in_lines(&lines, q));
        if let Some(matches) = matches {
            self.find.matches = matches;
            if reveal && !self.find.matches.is_empty() {
                self.find_reveal_current();
            }
        }
    }

    /// Step to the next (`forward`) or previous match.
    pub(crate) fn find_step(&mut self, forward: bool) {
        if !self.find.active {
            return;
        }
        if let Some(wv) = self.active_webview() {
            let js = if forward {
                "window.__findNext&&window.__findNext()"
            } else {
                "window.__findPrev&&window.__findPrev()"
            };
            let _ = wv.evaluate_script(js);
            return;
        }
        let n = self.find.matches.len();
        if n == 0 {
            return;
        }
        self.find.current =
            if forward { (self.find.current + 1) % n } else { (self.find.current + n - 1) % n };
        self.find_reveal_current();
        // Keep the read-mode caret on the current match as you step with n/N.
        if self.active_is_read_native() && self.read_caret_active() {
            if let Some(&NativeMatch { line, start, .. }) = self.find.matches.get(self.find.current) {
                self.place_read_caret_at(line, start);
            }
        }
        self.set_status(self.find_count_label());
        self.window.request_redraw();
    }

    /// The active tab's searchable lines (read = laid-out lines, vim = buffer lines).
    pub(crate) fn find_native_lines(&mut self) -> Option<std::borrow::Cow<'_, [String]>> {
        let i = self.active?;
        if self.tabs.get(i).is_some_and(|t| t.native().is_some()) {
            self.refresh_read_layout();
            let nr = self.tabs[i].native()?;
            // Borrowed straight from the layout's cached text: typing in `/` no
            // longer rebuilds every line string of the document per keystroke.
            return Some(std::borrow::Cow::Borrowed(nr.layout.text_lines()));
        }
        // AI tabs are vim buffers too (their conversation), so they search the same way.
        if let Some(ai) = self.tabs.get(i).and_then(|t| t.ai()) {
            return Some(std::borrow::Cow::Owned(
                ai.buf.lines.iter().map(|l| l.iter().collect::<String>()).collect(),
            ));
        }
        let vb = self.tabs.get(i)?.vim()?;
        Some(std::borrow::Cow::Owned(
            vb.lines.iter().map(|l| l.iter().collect::<String>()).collect(),
        ))
    }

    /// Scroll a read tab (or move a vim tab's cursor) so the current match shows.
    pub(crate) fn find_reveal_current(&mut self) {
        let Some(&NativeMatch { line, start, .. }) = self.find.matches.get(self.find.current) else {
            return;
        };
        let view = self.content_view_h();
        let Some(i) = self.active else { return };
        if let Some(nr) = self.tabs.get_mut(i).and_then(|t| t.native_mut()) {
            let line_h = nr.layout.line_h.max(1);
            let max = (nr.layout.height - view).max(0);
            nr.scroll = (line as i32 * line_h - view / 2).clamp(0, max);
            return;
        }
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (w as usize).saturating_sub(16) / cw;
        let rows = (view as usize) / line_h;
        if let Some(vb) = self.tabs.get_mut(i).and_then(|t| t.vim_mut()) {
            vb.place_cursor(line, start, rows, cols);
        } else if let Some(ai) = self.tabs.get_mut(i).and_then(|t| t.ai_mut()) {
            ai.buf.place_cursor(line, start, rows, cols);
            ai.follow = false; // stay on the match, don't snap back to the bottom
        }
    }

    /// `"/query  cur/total"` (or `no matches`) for the status bar on native tabs.
    pub(crate) fn find_count_label(&self) -> String {
        if self.find.matches.is_empty() {
            format!("/{}  no matches", self.find.query)
        } else {
            format!("/{}  {}/{}", self.find.query, self.find.current + 1, self.find.matches.len())
        }
    }
}

/// Case-insensitive (ASCII) find-in-page over `lines`, returning every match as a
/// `(line, char-range)`. Matches don't span lines (each visual/buffer line is
/// searched independently), which is fine for the short queries this is for.
pub(crate) fn find_in_lines(lines: &[String], q: &str) -> Vec<NativeMatch> {
    let needle: Vec<char> = q.chars().map(|c| c.to_ascii_lowercase()).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let hay: Vec<char> = line.chars().map(|c| c.to_ascii_lowercase()).collect();
        let mut i = 0;
        while i + needle.len() <= hay.len() {
            if hay[i..i + needle.len()] == needle[..] {
                out.push(NativeMatch { line: li, start: i, end: i + needle.len() });
                i += needle.len();
            } else {
                i += 1;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn find_in_lines_is_case_insensitive_and_per_line() {
        let lines = vec!["The Rust language".to_string(), "rust rust".to_string()];
        let m = super::find_in_lines(&lines, "rust");
        // One on line 0 (case-insensitive), two on line 1.
        assert_eq!(m.len(), 3);
        assert_eq!((m[0].line, m[0].start, m[0].end), (0, 4, 8));
        assert_eq!((m[1].line, m[1].start), (1, 0));
        assert_eq!((m[2].line, m[2].start), (1, 5));
    }
}
