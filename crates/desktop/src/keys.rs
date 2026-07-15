//! Input handling: per-mode key dispatch, the command-line editor (motions,
//! selection, word ops, autocomplete), the read/web caret modes, and the
//! Insert/Passthrough mode transitions.

use tao::event::KeyEvent;
use tao::keyboard::{Key, KeyCode};

use crate::chrome::history_display;
use crate::panes::SplitDir;
use crate::{clipboard_get, clipboard_set, vim, App, ModeKind, COMMANDS};

impl App {
    pub(crate) fn handle_key(&mut self, key: &KeyEvent) {
        // Alt+Tab straggler: switching INTO the browser can (re)deliver the Tab that
        // completed the switch — sometimes with Alt still reported held. Nothing in
        // the shell wants a Tab that soon after an app switch (or one with Alt down),
        // so swallow it in EVERY mode. Pages that hold OS focus don't come through
        // here; those are guarded in `khook`.
        if matches!(key.logical_key, Key::Tab)
            && (self.modifiers.alt_key()
                || self.last_focus_gain.elapsed() < std::time::Duration::from_millis(300))
        {
            return;
        }
        // Receiving a key here means the shell holds the keyboard, so any prior
        // page-focus yield is over (the hook only forwards Esc as ReclaimNormal).
        self.page_focus_yielded = false;
        match self.mode {
            ModeKind::Command | ModeKind::Find => self.key_command(key),
            ModeKind::Resize => self.key_resize(key),
            ModeKind::Move => self.key_move(key),
            ModeKind::PaneResize => self.key_pane_resize(key),
            ModeKind::PaneMove => self.key_pane_move(key),
            ModeKind::Hint => self.key_hint(key),
            ModeKind::Caret => self.key_caret(key),
            // Light field typing (web only): the page has focus and the bridge + hook own
            // the leave key; this is the shell-side fallback for the beat right after
            // entering, before focus lands on the page. Esc leaves. Ctrl+V is left to the
            // page as a normal paste — to enter passthrough, leave Insert first, then Ctrl+V.
            ModeKind::Insert => {
                if matches!(key.logical_key, Key::Escape) && !self.modifiers.shift_key() {
                    self.exit_to_normal();
                }
            }
            // Sticky typing mode. The content normally has focus (web page) or the shell
            // forwards to it (terminal / AI), and the injected bridge + keyboard hook own
            // the leave chords; these arms are the shell-side fallbacks.
            ModeKind::Passthrough => {
                if self.active_is_ai() {
                    // AI tabs type straight into their native field (no webview); key_ai
                    // owns Esc (leave), Enter (send), Ctrl+U (clear).
                    self.key_ai(key);
                } else if self.active_is_term() {
                    // Native terminal: Ctrl+S leaves; Esc and everything else go to the
                    // PTY (vim needs Esc). A few chords stay shell-owned: Ctrl+V pastes
                    // the clipboard into the PTY, Ctrl +/-/0 zoom.
                    if self.modifiers.control_key() && key.physical_key == KeyCode::KeyS {
                        return self.exit_to_normal();
                    }
                    if self.modifiers.control_key() {
                        match key.physical_key {
                            KeyCode::KeyV => return self.term_paste(),
                            KeyCode::Equal => return self.zoom_by(1),
                            KeyCode::Minus => return self.zoom_by(-1),
                            KeyCode::Digit0 => return self.zoom_reset(),
                            _ => {}
                        }
                    }
                    self.key_term(key);
                } else {
                    // Web page: sticky — leaves only on Ctrl+S or Shift+Esc. Plain Esc is
                    // left for the page (a web SSH/vim needs it), so passthrough is never
                    // exited by a bare Esc, a click, or navigation.
                    let leave = (self.modifiers.control_key()
                        && key.physical_key == KeyCode::KeyS)
                        || (matches!(key.logical_key, Key::Escape) && self.modifiers.shift_key());
                    if leave {
                        self.exit_to_normal();
                    }
                }
            }
            ModeKind::Normal => self.key_normal(key),
        }
        self.window.request_redraw();
    }

    pub(crate) fn key_normal(&mut self, key: &KeyEvent) {
        // Ctrl+W window prefix: the next key picks a pane action (vim-window style).
        // h/j/k/l move focus, Shift+H/J/K/L enter repeatable resize, s/v split (stacked
        // / side-by-side), c/q close the pane. Matched on the physical key so it works
        // whether or not Ctrl is still held. A prefix left dangling past the timeout is
        // dropped so a forgotten Ctrl+W can't hijack a later key (that key falls through
        // to its normal binding).
        if self.pending_window_key {
            self.pending_window_key = false;
            if self.pending_window_at.elapsed() <= crate::app::WINDOW_PREFIX_TIMEOUT {
                let resize = self.modifiers.shift_key();
                match key.physical_key {
                    KeyCode::KeyH if resize => self.enter_pane_resize('h'),
                    KeyCode::KeyJ if resize => self.enter_pane_resize('j'),
                    KeyCode::KeyK if resize => self.enter_pane_resize('k'),
                    KeyCode::KeyL if resize => self.enter_pane_resize('l'),
                    KeyCode::KeyH => self.move_pane_focus('h'),
                    KeyCode::KeyJ => self.move_pane_focus('j'),
                    KeyCode::KeyK => self.move_pane_focus('k'),
                    KeyCode::KeyL => self.move_pane_focus('l'),
                    KeyCode::KeyS => self.split_pane(SplitDir::Col),
                    KeyCode::KeyV => self.split_pane(SplitDir::Row),
                    KeyCode::KeyC | KeyCode::KeyQ => self.close_active(),
                    // Grab a pane to move it: `m` grabs the focused pane, a digit pulls
                    // that tab-bar entry into the split. `b` breaks the focused pane out.
                    KeyCode::KeyM => self.grab_focused_pane_move(),
                    KeyCode::KeyB => self.break_pane(),
                    // Flip the focused pane's split between side-by-side and stacked.
                    KeyCode::KeyR => self.toggle_pane_orientation(),
                    _ => {
                        if let Some(n) = digit_index(key.physical_key) {
                            self.grab_pane_move(n);
                        }
                    }
                }
                return;
            }
            // else: stale prefix — fall through and handle this key normally.
        }
        // Once a `/` search is live, `n`/`N` step through matches and Esc clears it
        // (qutebrowser-style) — in every tab type, so this takes precedence over both
        // the vim pager and the normal tab/scroll bindings.
        if self.find.active && !self.modifiers.control_key() {
            match &key.logical_key {
                Key::Character(s) if *s == "n" => {
                    self.find_step(true);
                    return;
                }
                Key::Character(s) if *s == "N" => {
                    self.find_step(false);
                    return;
                }
                Key::Escape => {
                    self.find_clear();
                    return;
                }
                _ => {}
            }
        }
        // Terminal copy/vi mode (Shift+Esc): vi motions move a cursor over the live
        // grid, v/V select, y yanks, i/Enter resumes. Unhandled keys (`:`/x/n/p) fall
        // through to the normal browser bindings.
        if self.active_is_term() {
            self.ensure_term_vi(); // a tab/pane switch can leave vi mode off (stuck cursor)
        }
        if self.active_term_vi() && self.key_term_vi(key) {
            return;
        }
        // On the `:history` and `:aihist` pickers, `d` deletes the entry/chat under
        // the cursor (or the whole visual selection); `u` is inert. Half-page scrolling
        // stays on the Ctrl chords. Intercept before the generic pager, where plain
        // `d`/`u` would otherwise half-page scroll (the reported "d acts like Ctrl+D" bug).
        if self.active_is_vim() && !self.modifiers.control_key() {
            let is_hist = self.active_url() == Some("browser://history");
            let is_aihist = self.active_url() == Some("browser://aihist");
            if is_hist || is_aihist {
                if let Key::Character(s) = &key.logical_key {
                    match *s {
                        "d" => {
                            if is_aihist {
                                self.delete_ai_chat_lines();
                            } else {
                                self.delete_history_lines();
                            }
                            return;
                        }
                        "u" => return,
                        _ => {}
                    }
                }
            }
        }
        // A `:error`/`:errors` tab is a read-only vim pager: let it claim the motion/
        // visual/yank keys first; anything it doesn't want (`:`, n/p, x, …) falls
        // through to the normal browser bindings below.
        if self.active_is_vim() && self.key_vim(key) {
            return;
        }
        // Read-mode caret/visual selection (engine-free read tab): once active it
        // claims motion/visual/yank keys; `v`/`V` below enter it.
        if self.read_caret_active() && self.key_read_caret(key) {
            return;
        }
        // AI tab: it's a read-only vim buffer in Normal mode (motions/visual/yank,
        // Ctrl+D/U), exactly like the `:error`/`:res` pager. Keys it doesn't claim
        // (`i`/`:`/x/n/p/digits…) fall through to the normal browser bindings.
        if self.active_is_ai() && self.key_ai_nav(key) {
            return;
        }
        // Chords (with Ctrl) take precedence over plain keys.
        if self.modifiers.control_key() {
            match key.physical_key {
                KeyCode::KeyV => self.enter_passthrough(),
                // Ctrl+W: arm the window/pane prefix (next key picks the action), with a
                // timestamp so a dangling prefix expires instead of eating a later key.
                KeyCode::KeyW => {
                    self.pending_window_key = true;
                    self.pending_window_at = std::time::Instant::now();
                }
                // Reopen the last closed tab (the familiar browser shortcut).
                KeyCode::KeyT if self.modifiers.shift_key() => self.reopen_closed(),
                // Half-page scroll (vim Ctrl+D / Ctrl+U).
                KeyCode::KeyD => self.scroll(self.half_page()),
                KeyCode::KeyU => self.scroll(-self.half_page()),
                // Browser (chrome) zoom: the bars + terminal font. Web page content is
                // the separate plain `+/-` binding below.
                KeyCode::Equal => self.zoom_by(1),
                KeyCode::Minus => self.zoom_by(-1),
                KeyCode::Digit0 => self.zoom_reset(),
                _ => {}
            }
            return;
        }
        // Shift+digit: the tab strip past 9 — Shift+1 → the 11th entry … Shift+9 →
        // the 19th (plain 1-9 jump to 1-9 below; Shift+0 stays `)` = zoom reset).
        // Matched on the PHYSICAL key because the logical character is layout-
        // dependent punctuation (!@#…) — but only when the logical key is NOT itself
        // a digit, so layouts where digits already need Shift (AZERTY) keep their
        // plain 1-9 jumps instead of landing in the second bank.
        if self.modifiers.shift_key()
            && !matches!(&key.logical_key,
                Key::Character(s) if s.len() == 1 && s.as_bytes()[0].is_ascii_digit())
        {
            if let Some(n) = digit_index(key.physical_key) {
                self.jump_to(10 + n);
                return;
            }
        }
        match &key.logical_key {
            Key::Character(s) => match *s {
                ":" => self.enter_command(""),
                // `o` opens in THIS tab; `O` opens in a new tab (prefills `:open -t`).
                "o" => self.enter_command("open "),
                "O" => self.enter_command("open -t "),
                "j" => self.scroll(80),
                "k" => self.scroll(-80),
                // Horizontal page scroll (H/L are history back/forward).
                "h" => self.scroll_x(-80),
                "l" => self.scroll_x(80),
                // Reopen the last closed tab (vim-style undo; also Ctrl+Shift+T).
                "u" => self.reopen_closed(),
                "g" => self.scroll_edge(false),
                "G" => self.scroll_edge(true),
                "/" => self.enter_find(),
                "i" => {
                    // `i` types into the content. A web page enters the light Insert mode
                    // (auto-leaves on click-away / navigation); a terminal or the AI field
                    // have no lighter mode, so they go straight to the sticky passthrough.
                    if self.active_is_ai() {
                        self.enter_ai_passthrough();
                    } else if self.active_is_term() {
                        self.enter_passthrough();
                    } else {
                        self.enter_insert();
                    }
                }
                // Hint mode labels clickable things — but a terminal has none, and
                // its copy/vi mode (handled above) uses f/F/t/T for vim find-char.
                // So suppress hints on terminal tabs (the source of the f/F conflict).
                "f" if !self.active_is_term() => self.enter_hint(false),
                // Shift+F: hints open the picked link in a NEW tab (like `:open -t`).
                "F" if !self.active_is_term() => self.enter_hint(true),
                // Caret mode — a vim cursor on the page; a second v/V starts the
                // charwise/linewise selection to yank. Read tabs use the native
                // caret; web tabs (open/research) use the injected page caret.
                // Terminal tabs are excluded.
                "v" | "V" => {
                    if self.active_is_read_native() {
                        self.enter_read_caret();
                    } else if self.active_webview().is_some() && !self.active_is_term() {
                        self.enter_web_caret();
                    }
                }
                "x" => self.close_active(),
                "r" => self.reload_active(),
                // On an AI tab, H/L step through saved chats; elsewhere they're
                // page history (back/forward).
                "H" => {
                    if self.active_is_ai() {
                        self.ai_history(false);
                    } else {
                        self.history(false);
                    }
                }
                "L" => {
                    if self.active_is_ai() {
                        self.ai_history(true);
                    } else {
                        self.history(true);
                    }
                }
                "n" => self.switch_tab(1),
                "p" => self.switch_tab(-1),
                "<" => self.move_tab(-1),
                ">" => self.move_tab(1),
                // Web-content zoom (page scale), separate from the chrome `Ctrl +/-`.
                // Both the unshifted (`=`/`-`) and shifted (`+`/`_`) keys work, so
                // "+/-" and "Shift +/-" both hit it; `)` (Shift+0) resets to 100%.
                "+" | "=" => self.content_zoom_by(1),
                "-" | "_" => self.content_zoom_by(-1),
                ")" => self.content_zoom_reset(),
                d if d.len() == 1 && d.as_bytes()[0].is_ascii_digit() => {
                    let n = (d.as_bytes()[0] - b'0') as usize;
                    if n >= 1 {
                        self.jump_to(n - 1);
                    }
                }
                _ => {}
            },
            Key::ArrowDown => self.scroll(80),
            Key::ArrowUp => self.scroll(-80),
            Key::ArrowLeft => self.scroll_x(-80),
            Key::ArrowRight => self.scroll_x(80),
            // Enter on a `:history` line opens that entry (Shift+Enter → new tab);
            // on a `:aihist` line it opens that saved chat; a no-op anywhere else.
            Key::Enter => match self.active_url() {
                Some("browser://aihist") => self.open_ai_history_entry(),
                Some("browser://extensions") => self.toggle_extension_entry(),
                _ => self.open_history_entry(self.modifiers.shift_key()),
            },
            _ => {}
        }
    }

    /// Enter on a line of the `:history` pager: open the URL on the cursor line —
    /// in this tab, or a new one when `new_tab` (Shift+Enter). A no-op on any other
    /// tab or on a non-URL line (the header/blank), so plain Enter is otherwise inert.
    pub(crate) fn open_history_entry(&mut self, new_tab: bool) {
        if self.active_url() != Some("browser://history") {
            return;
        }
        let line = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim())
            .and_then(|b| b.lines.get(b.cy))
            .map(|chars| chars.iter().collect::<String>());
        let Some(line) = line else { return };
        let url = line.trim();
        if url.starts_with("http") {
            self.open_tab(url, self.nojs, new_tab);
        }
    }

    /// Enter on an `:extensions` row: flip that extension on/off (WebView2 enable/disable),
    /// update the cached list, and refresh the picker in place. A no-op on any other tab or a
    /// header/blank line (the id parse simply finds no matching extension).
    pub(crate) fn toggle_extension_entry(&mut self) {
        if self.active_url() != Some("browser://extensions") {
            return;
        }
        let line = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim())
            .and_then(|b| b.lines.get(b.cy))
            .map(|chars| chars.iter().collect::<String>());
        let Some(line) = line else { return };
        // Row form: "[on ]  Name    <id>" — the id is the last whitespace-delimited token.
        let Some(id) = line.split_whitespace().last().map(str::to_string) else { return };
        let Some(pos) = self.extensions.iter().position(|e| e.id == id) else { return };
        let want = !self.extensions[pos].enabled;
        #[cfg(windows)]
        if let Some(wv) = self.any_webview() {
            crate::extensions::set_enabled(wv, id, want);
        }
        self.extensions[pos].enabled = want;
        let name = self.extensions[pos].name.trim().to_string();
        self.show_extensions_page();
        let name = if name.is_empty() { "extension".to_string() } else { name };
        self.set_status(format!("{} {name}", if want { "enabled" } else { "disabled" }));
    }

    /// `d` on the `:history` pager: delete the visited entry under the cursor — or,
    /// with a visual selection, every selected entry — from the history list, then
    /// rebuild the page. A no-op on any other tab. The change is in memory (like
    /// `:clear history`); it's persisted on the next session write.
    pub(crate) fn delete_history_lines(&mut self) {
        if self.active_url() != Some("browser://history") {
            return;
        }
        let Some(i) = self.active else { return };
        // The row span to delete: the visual selection if any, else the cursor line.
        let (lo, hi) = {
            let Some(buf) = self.tabs.get(i).and_then(|t| t.vim()) else { return };
            match buf.anchor {
                Some((ay, _)) => (ay.min(buf.cy), ay.max(buf.cy)),
                None => (buf.cy, buf.cy),
            }
        };
        // The page is a 2-line header (title + blank) followed by the URLs in the
        // same order as `history`, so row r maps to history index r - HEADER.
        const HEADER: usize = 2;
        if self.history.is_empty() || hi < HEADER {
            return;
        }
        let start = lo.max(HEADER) - HEADER;
        let end = (hi - HEADER).min(self.history.len() - 1);
        if start > end {
            return;
        }
        let removed = end - start + 1;
        // Drop the slice from both parallel vecs, keeping them aligned.
        self.history.drain(start..=end);
        let end_at = end.min(self.history_at.len().saturating_sub(1));
        if start <= end_at && start < self.history_at.len() {
            self.history_at.drain(start..=end_at);
        }
        // Rebuild and park the cursor on whatever slid up into the deleted row.
        let lines = crate::pages::history_lines(&self.history);
        if let Some(buf) = self.tabs.get_mut(i).and_then(|t| t.vim_mut()) {
            buf.set_lines(lines);
            buf.anchor = None;
            buf.cy = lo.min(buf.lines.len().saturating_sub(1));
            buf.cx = 0;
        }
        let plural = if removed == 1 { "entry" } else { "entries" };
        self.set_status(format!("deleted {removed} history {plural}"));
        self.window.request_redraw();
    }

    /// `d` on the `:aihist` picker: delete the saved chat under the cursor — or every
    /// chat in the visual selection — from `ai_chats`, persist, and rebuild the page.
    /// The rows are NEWEST-FIRST (header + one per chat), so a buffer row span maps to
    /// a contiguous chat-index range `[n-1-(hi-HEADER), n-1-(lo-HEADER)]`. Every open
    /// `:ai` tab's `chat_idx` is realigned (a deleted chat → draft; a higher index
    /// shifts down) so it keeps pointing at the right conversation, exactly like the
    /// cap-trim in [`commit_ai`](App::commit_ai). A no-op on any other tab.
    pub(crate) fn delete_ai_chat_lines(&mut self) {
        if self.active_url() != Some("browser://aihist") {
            return;
        }
        let Some(i) = self.active else { return };
        let n = self.ai_chats.len();
        // The buffer row span to delete: the visual selection if any, else the cursor.
        let (lo, hi) = {
            let Some(buf) = self.tabs.get(i).and_then(|t| t.vim()) else { return };
            match buf.anchor {
                Some((ay, _)) => (ay.min(buf.cy), ay.max(buf.cy)),
                None => (buf.cy, buf.cy),
            }
        };
        // Invert the newest-first rows to the contiguous chat-index range they cover.
        let Some((start, end)) = crate::pages::aihist_rows_to_chat_range(n, lo, hi) else {
            return;
        };
        let removed = end - start + 1;
        self.ai_chats.drain(start..=end);
        for tab in &mut self.tabs {
            if let Some(a) = tab.ai_mut() {
                a.chat_idx = match a.chat_idx {
                    Some(c) if c >= start && c <= end => None,
                    Some(c) if c > end => Some(c - removed),
                    other => other,
                };
            }
        }
        crate::ai::save_chats(&self.ai_chats);
        // Rebuild and park the cursor on whatever slid up into the deleted span.
        let lines = crate::pages::ai_history_lines(&self.ai_chats);
        if let Some(buf) = self.tabs.get_mut(i).and_then(|t| t.vim_mut()) {
            buf.set_lines(lines);
            buf.anchor = None;
            buf.cy = lo.min(buf.lines.len().saturating_sub(1));
            buf.cx = 0;
        }
        let plural = if removed == 1 { "chat" } else { "chats" };
        self.set_status(format!("deleted {removed} {plural}"));
        self.window.request_redraw();
    }

    /// Translate a raw key event into a vim-pager [`vim::Key`], or `None` if it
    /// doesn't map (so the shell can handle it). Ctrl+D/Ctrl+U are half-page; any
    /// other Ctrl chord (zoom, …) is left for the shell. Shared by the `:error`/
    /// `:res` pagers and the read-mode caret.
    pub(crate) fn map_vim_key(&self, key: &KeyEvent) -> Option<vim::Key> {
        if self.modifiers.control_key() {
            return match key.physical_key {
                KeyCode::KeyD => Some(vim::Key::HalfDown),
                KeyCode::KeyU => Some(vim::Key::HalfUp),
                _ => None,
            };
        }
        match &key.logical_key {
            Key::ArrowLeft => Some(vim::Key::Left),
            Key::ArrowRight => Some(vim::Key::Right),
            Key::ArrowUp => Some(vim::Key::Up),
            Key::ArrowDown => Some(vim::Key::Down),
            Key::Home => Some(vim::Key::Home),
            Key::End => Some(vim::Key::End),
            Key::Escape => Some(vim::Key::Esc),
            Key::Character(s) => {
                let mut chars = s.chars();
                match (chars.next(), chars.next()) {
                    (Some(c), None) => Some(vim::Key::Char(c)),
                    _ => None,
                }
            }
            _ => None,
        }
    }

    /// Feed a key to the active `:error`/`:errors`/`:res` vim pager. Returns `true`
    /// if the pager consumed it (the shell should then do nothing else with the key).
    pub(crate) fn key_vim(&mut self, key: &KeyEvent) -> bool {
        let Some(vk) = self.map_vim_key(key) else { return false };
        // Viewport in cells (monospace), matching the draw geometry below.
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = ((w as usize).saturating_sub(2 * 8)) / cw;
        let rows = (self.content_view_h() as usize) / line_h;
        let Some(buf) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.vim_mut())
        else {
            return false;
        };
        let res = buf.key(vk, rows, cols);
        if let Some(text) = res.yanked {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        }
        if res.consumed {
            self.window.request_redraw();
        }
        res.consumed
    }

    /// Whether the active tab is an engine-free read tab (native render).
    pub(crate) fn active_is_read_native(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.native().is_some())
    }

    /// Whether read-mode caret/visual selection is currently active.
    pub(crate) fn read_caret_active(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.native())
            .is_some_and(|n| n.caret.is_some())
    }

    /// Enter read-mode caret browsing (`v`/`V`): build a caret over the read view's
    /// visual lines and place it near the middle of the viewport — WITHOUT selecting.
    /// A second `v`/`V` (handled by the vim buffer) starts the charwise/linewise
    /// visual selection.
    pub(crate) fn enter_read_caret(&mut self) {
        // Geometry first (immutable borrows of painter), before borrowing the tab.
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let view = self.content_view_h();
        let rows = (view as usize / line_h).max(1);
        let Some(nr) = self.active_native_mut() else { return };
        let lines = nr.layout.text_lines();
        if lines.iter().all(|l| l.is_empty()) {
            return;
        }
        let n = lines.len();
        let lh = nr.layout.line_h.max(1);
        // Keep the view put: the caret starts at the line already at the middle of
        // the CURRENT viewport, so entering visual doesn't scroll (it's redundant to
        // re-center on a cursor we just placed mid-screen). Seed the buffer's own
        // scroll to match, and snap the read scroll to that line boundary.
        let top_line = (nr.scroll / lh).max(0) as usize;
        let mid_line = (top_line + rows / 2).min(n.saturating_sub(1));
        let mut tb = vim::TextBuffer::new(lines.to_vec());
        tb.top = top_line;
        tb.place_cursor(mid_line, 0, rows, cols);
        nr.scroll = tb.top as i32 * lh;
        nr.caret = Some(tb);
        self.cursor_on = true; // start the blink on the solid phase
        self.set_status("[CARET]  hjkl/w/b/0/$/gg/G move · v/V select · y yank · Esc exit");
        self.window.request_redraw();
    }

    /// Place a selection-less read-mode caret at `(line, col)` so the cursor sits on
    /// a specific word (e.g. a find match) and `hjkl` move relative to it instead of
    /// scrolling the page. Keeps the current scroll context.
    pub(crate) fn place_read_caret_at(&mut self, line: usize, col: usize) {
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let rows = (self.content_view_h() as usize / line_h).max(1);
        let Some(nr) = self.active_native_mut() else { return };
        let lines = nr.layout.text_lines();
        if lines.is_empty() {
            return;
        }
        let lh = nr.layout.line_h.max(1);
        let mut tb = vim::TextBuffer::new(lines.to_vec());
        tb.top = (nr.scroll / lh).max(0) as usize;
        tb.place_cursor(line, col, rows, cols);
        nr.scroll = tb.top as i32 * lh;
        nr.caret = Some(tb);
        self.cursor_on = true;
    }

    /// Feed a key to the read-mode caret. Returns `true` if it was consumed. `Esc`
    /// with no selection leaves caret mode; the read view's pixel scroll is kept in
    /// sync with the caret's line so the cursor stays on-screen.
    pub(crate) fn key_read_caret(&mut self, key: &KeyEvent) -> bool {
        let Some(vk) = self.map_vim_key(key) else { return false };
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let rows = (self.content_view_h() as usize / line_h).max(1);

        let mut yanked: Option<String> = None;
        let mut consumed = true;
        let mut exit = false;
        {
            let Some(nr) = self.active_native_mut() else { return false };
            let Some(buf) = nr.caret.as_mut() else { return false };
            if vk == vim::Key::Esc && !buf.has_selection() {
                exit = true;
            } else {
                let res = buf.key(vk, rows, cols);
                yanked = res.yanked;
                consumed = res.consumed;
                nr.scroll = buf.top as i32 * nr.layout.line_h.max(1);
            }
            if exit {
                nr.caret = None;
            }
        }
        if let Some(text) = yanked {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        } else if exit {
            self.clear_status();
        }
        if consumed || exit {
            self.cursor_on = true; // a motion resets the blink to solid
            self.window.request_redraw();
        }
        consumed || exit
    }

    /// Enter caret browsing on a WEB tab: tell the injected page caret to place
    /// itself at the viewport center — without selecting; a second `v`/`V` starts
    /// the visual selection. The shell keeps keyboard focus (like hint mode) and
    /// forwards motions via `key_caret`.
    pub(crate) fn enter_web_caret(&mut self) {
        let Some(wv) = self.active_webview() else { return };
        let _ = wv.evaluate_script("window.__caretEnter&&window.__caretEnter()");
        self.mode = ModeKind::Caret;
        self.set_status("[CARET]  hjkl/w/b/0/$/gg/G move · v/V select · y yank · Esc exit");
        self.window.request_redraw();
    }

    /// Forward a key to the web tab's injected caret. Motions/visual go to
    /// `__caretKey`; `y` yanks; `Esc` collapses-then-exits (the page posts
    /// `caret-exit` when it actually leaves, which returns the shell to Normal).
    pub(crate) fn key_caret(&mut self, key: &KeyEvent) {
        let Some(wv) = self.active_webview() else {
            self.mode = ModeKind::Normal;
            return;
        };
        match &key.logical_key {
            Key::Escape => {
                let _ = wv.evaluate_script("window.__caretEsc&&window.__caretEsc()");
            }
            Key::Character(s) => {
                if *s == "y" {
                    let _ = wv.evaluate_script("window.__caretYank&&window.__caretYank()");
                } else if matches!(
                    *s,
                    "h" | "j" | "k" | "l" | "w" | "b" | "e" | "0" | "$" | "g" | "G" | "v" | "V"
                ) {
                    let _ = wv.evaluate_script(&format!("window.__caretKey&&window.__caretKey('{s}')"));
                }
            }
            _ => {}
        }
        self.window.request_redraw();
    }

    pub(crate) fn key_command(&mut self, key: &KeyEvent) {
        let shift = self.modifiers.shift_key();
        // Ctrl chords: clipboard, word movement/deletion, line editing. They take
        // precedence over text input. (Alt+Backspace is handled just below.)
        if self.modifiers.control_key() {
            match key.physical_key {
                // Clipboard. Ctrl+C copies the selection (or, with nothing selected,
                // cancels — the old behavior); Ctrl+X cuts; Ctrl+V pastes.
                KeyCode::KeyC => {
                    if let Some((a, b)) = self.sel_range() {
                        clipboard_set(&self.command[a..b]);
                    } else {
                        self.cancel_command();
                        return;
                    }
                }
                KeyCode::KeyX => {
                    if let Some((a, b)) = self.sel_range() {
                        clipboard_set(&self.command[a..b]);
                        self.delete_selection();
                    }
                }
                KeyCode::KeyV => {
                    if let Some(text) = clipboard_get() {
                        self.cmd_insert(&text);
                    }
                }
                KeyCode::KeyA => {
                    self.command_anchor = Some(0);
                    self.command_cursor = self.command.len();
                }
                // Word-wise caret movement (Ctrl+Shift extends the selection).
                KeyCode::ArrowLeft => {
                    let p = self.prev_word(self.command_cursor);
                    self.move_caret(p, shift);
                }
                // Ctrl+Right accepts the autocomplete suggestion (if any) — else moves
                // a word, as before.
                KeyCode::ArrowRight => {
                    if !self.accept_suggestion(true) {
                        let p = self.next_word(self.command_cursor);
                        self.move_caret(p, shift);
                    }
                }
                // Ctrl+W / Ctrl+Backspace: delete the word before the caret.
                KeyCode::KeyW | KeyCode::Backspace => self.cmd_delete_word(),
                // Ctrl+Delete: delete the word after the caret.
                KeyCode::Delete => self.cmd_delete_word_forward(),
                // Ctrl+U: delete from the caret back to the start of the line.
                KeyCode::KeyU => {
                    self.command.replace_range(0..self.command_cursor, "");
                    self.command_cursor = 0;
                    self.command_anchor = None;
                }
                KeyCode::KeyH => self.cmd_backspace(),
                _ => {}
            }
            self.cursor_on = true;
            if self.mode == ModeKind::Find {
                self.find_update();
            }
            return;
        }
        // Alt+Backspace: delete the word before the caret (a common alias).
        if self.modifiers.alt_key() && key.physical_key == KeyCode::Backspace {
            self.cmd_delete_word();
            self.cursor_on = true;
            if self.mode == ModeKind::Find {
                self.find_update();
            }
            return;
        }
        match &key.logical_key {
            Key::Enter if self.mode == ModeKind::Find => {
                self.find_confirm();
                return;
            }
            // Tab accepts the autocomplete suggestion, or cycles a fixed-choice
            // argument (`:model`, `:adblock`, `:clear`, … — see
            // `commands::arg_candidates`); Shift+Tab steps the cycle backward.
            Key::Tab => {
                self.accept_suggestion(!shift);
            }
            Key::Enter => {
                // Quick maths: if the line is an arithmetic expression, evaluate it
                // in place — replace the bar contents with the result so you can copy
                // it or keep calculating (`20*8` → `160` → `160+10`) instead of
                // running it as a command.
                if let Some(result) = self.math_preview() {
                    self.command = result;
                    self.command_cursor = self.command.len();
                    self.command_anchor = None;
                    self.cursor_on = true;
                    self.clear_status();
                    return;
                }
                let line = std::mem::take(&mut self.command);
                self.command_cursor = 0;
                self.command_anchor = None;
                self.mode = ModeKind::Normal;
                self.run_command(&line);
                // Back to Normal re-hides the bar over a fullscreen page; refit it.
                self.relayout_active();
            }
            Key::Escape => self.cancel_command(),
            Key::Backspace => {
                if !self.delete_selection() {
                    self.cmd_backspace();
                }
            }
            Key::Delete => {
                if !self.delete_selection() {
                    self.cmd_delete_forward();
                }
            }
            // Plain arrow with a selection collapses to that edge; otherwise moves a
            // character. Shift extends (or starts) the selection.
            Key::ArrowLeft => {
                if !shift {
                    if let Some((a, _)) = self.sel_range() {
                        self.command_cursor = a;
                        self.command_anchor = None;
                    } else {
                        self.move_caret(self.prev_char(self.command_cursor), false);
                    }
                } else {
                    self.move_caret(self.prev_char(self.command_cursor), true);
                }
            }
            Key::ArrowRight => {
                if !shift {
                    if let Some((_, b)) = self.sel_range() {
                        self.command_cursor = b;
                        self.command_anchor = None;
                    } else {
                        self.move_caret(self.next_char(self.command_cursor), false);
                    }
                } else {
                    self.move_caret(self.next_char(self.command_cursor), true);
                }
            }
            Key::Home => self.move_caret(0, shift),
            Key::End => self.move_caret(self.command.len(), shift),
            Key::Space => self.cmd_insert(" "),
            Key::Character(s) => self.cmd_insert(s),
            _ => {}
        }
        // Any edit should show the cursor immediately (don't wait for the blink).
        self.cursor_on = true;
        // In Find mode, search live as the query changes.
        if self.mode == ModeKind::Find {
            self.find_update();
        }
    }

    /// Leave the command bar, discarding the line (Esc / Ctrl+C with no selection).
    /// In Find mode this also drops the search and its highlights.
    pub(crate) fn cancel_command(&mut self) {
        self.command.clear();
        self.command_cursor = 0;
        self.command_anchor = None;
        if self.mode == ModeKind::Find {
            self.find_clear();
        }
        self.mode = ModeKind::Normal;
        // Leaving the bar re-hides it over a fullscreen page; grow the page back.
        self.relayout_active();
    }

    /// The current selection as an ordered byte range, or `None` if empty.
    pub(crate) fn sel_range(&self) -> Option<(usize, usize)> {
        let a = self.command_anchor?;
        let c = self.command_cursor;
        (a != c).then(|| (a.min(c), a.max(c)))
    }

    /// Map a physical x pixel in the command bar to the nearest caret byte offset
    /// in `self.command`, replicating the draw layout: the line is `<pre><command>`
    /// drawn at `MARGIN - bar_cmd_scroll`, so boundary `k` sits at that origin plus
    /// the measured width of the prefix up to `k`. Picks the boundary whose midpoint
    /// the click falls before (so clicking a glyph's left/right half lands sensibly).
    pub(crate) fn bar_caret_at_x(&self, x: f64) -> usize {
        const MARGIN: i32 = 8;
        let pre = if self.mode == ModeKind::Find { '/' } else { ':' };
        let x_of = |k: usize| -> f64 {
            (MARGIN - self.bar_cmd_scroll) as f64
                + self.painter.measure(&format!("{pre}{}", &self.command[..k])) as f64
        };
        let mut best = 0;
        let mut prev = x_of(0);
        for (i, ch) in self.command.char_indices() {
            let k = i + ch.len_utf8();
            let cur = x_of(k);
            if x < (prev + cur) / 2.0 {
                return i;
            }
            best = k;
            prev = cur;
        }
        best
    }

    /// A left-press inside the command/status bar. In Command/Find mode it parks the
    /// caret at the click (and arms a drag-select); in Normal mode it opens the URL
    /// for editing — a mouse `:edit`, so the address can be selected, copied, or
    /// changed — falling back to a bare `:` prompt when there's no web URL.
    pub(crate) fn bar_click(&mut self, x: f64) {
        match self.mode {
            ModeKind::Command | ModeKind::Find => {
                self.command_cursor = self.bar_caret_at_x(x);
                self.command_anchor = None;
                self.bar_dragging = true;
                self.cursor_on = true;
            }
            ModeKind::Normal => {
                match self.active_url() {
                    Some(u) if u.starts_with("http") => {
                        let line = format!("{} {u}", self.active_reopen_verb());
                        self.enter_command(&line);
                    }
                    _ => self.enter_command(""),
                }
            }
            _ => return,
        }
        // The click left keyboard focus on the web child (the click-focus trap); pull
        // it back to the shell so Escape and typing reach the command line.
        self.reclaim_shell_focus();
        self.window.request_redraw();
    }

    /// Extend the command-line selection to the click x while left-dragging in the
    /// bar (mouse highlight). No-op unless a bar drag is in progress.
    pub(crate) fn bar_drag(&mut self, x: f64) {
        if !self.bar_dragging || !matches!(self.mode, ModeKind::Command | ModeKind::Find) {
            return;
        }
        let off = self.bar_caret_at_x(x);
        self.move_caret(off, true);
        self.cursor_on = true;
        self.window.request_redraw();
    }

    /// Move the caret to `pos`. `extend` keeps/starts a selection (Shift held);
    /// otherwise the selection is dropped. A zero-width selection is normalized away.
    pub(crate) fn move_caret(&mut self, pos: usize, extend: bool) {
        if extend {
            if self.command_anchor.is_none() {
                self.command_anchor = Some(self.command_cursor);
            }
        } else {
            self.command_anchor = None;
        }
        self.command_cursor = pos;
        if self.command_anchor == Some(pos) {
            self.command_anchor = None;
        }
    }

    /// Replace the selection (if any) with `text`, then place the caret after it.
    /// Control characters (e.g. newlines from a paste) are dropped — it's one line.
    pub(crate) fn cmd_insert(&mut self, text: &str) {
        self.delete_selection();
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.command.insert_str(self.command_cursor, &clean);
        self.command_cursor += clean.len();
    }

    /// Remove the selection if there is one; returns whether anything was deleted.
    pub(crate) fn delete_selection(&mut self) -> bool {
        if let Some((a, b)) = self.sel_range() {
            self.command.replace_range(a..b, "");
            self.command_cursor = a;
            self.command_anchor = None;
            true
        } else {
            self.command_anchor = None;
            false
        }
    }

    /// Byte offset of the char before `pos` (or `pos` if at the start).
    pub(crate) fn prev_char(&self, pos: usize) -> usize {
        self.command[..pos].char_indices().next_back().map(|(i, _)| i).unwrap_or(pos)
    }

    /// Byte offset just after the char at `pos` (or `pos` if at the end).
    pub(crate) fn next_char(&self, pos: usize) -> usize {
        self.command[pos..].chars().next().map(|c| pos + c.len_utf8()).unwrap_or(pos)
    }

    /// Start of the word before `pos` in the command line (see [`prev_word_boundary`]).
    pub(crate) fn prev_word(&self, pos: usize) -> usize {
        prev_word_boundary(&self.command, pos)
    }

    /// End of the word after `pos` in the command line (see [`next_word_boundary`]).
    pub(crate) fn next_word(&self, pos: usize) -> usize {
        next_word_boundary(&self.command, pos)
    }

    /// Delete the character before the caret (or the selection, if any).
    pub(crate) fn cmd_backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.prev_char(self.command_cursor);
        if start != self.command_cursor {
            self.command.replace_range(start..self.command_cursor, "");
            self.command_cursor = start;
        }
    }

    /// Delete the character after the caret (or the selection, if any).
    pub(crate) fn cmd_delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.next_char(self.command_cursor);
        self.command.replace_range(self.command_cursor..end, "");
    }

    /// Delete the word before the caret (or the selection, if any).
    pub(crate) fn cmd_delete_word(&mut self) {
        if self.delete_selection() {
            return;
        }
        let start = self.prev_word(self.command_cursor);
        self.command.replace_range(start..self.command_cursor, "");
        self.command_cursor = start;
    }

    /// Delete the word after the caret (or the selection, if any).
    pub(crate) fn cmd_delete_word_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let end = self.next_word(self.command_cursor);
        self.command.replace_range(self.command_cursor..end, "");
    }

    pub(crate) fn enter_command(&mut self, prefill: &str) {
        self.mode = ModeKind::Command;
        self.command = prefill.to_string();
        self.command_cursor = self.command.len();
        self.command_anchor = None;
        self.cursor_on = true;
        self.clear_status();
        // In fullscreen the bars were hidden; showing the command bar shrinks the page.
        self.relayout_active();
    }

    /// Command-bar autocomplete: the FULL completed line for the current input, or
    /// `None`. Completes a verb (`ver`→`version`) before the first space, otherwise
    /// an `:open`-style argument from visited history (`open yout`→`open youtube.com`).
    /// Only when in Command mode with the caret at the end and no selection.
    pub(crate) fn command_suggestion(&self) -> Option<String> {
        if self.mode != ModeKind::Command
            || self.command_cursor != self.command.len()
            || self.command_anchor.is_some()
        {
            return None;
        }
        let cmd = &self.command;
        if cmd.is_empty() {
            return None;
        }
        match cmd.split_once(char::is_whitespace) {
            // Verb completion.
            None => COMMANDS
                .iter()
                .find(|c| c.len() > cmd.len() && c.starts_with(cmd.as_str()))
                .map(|c| (*c).to_string()),
            // Argument completion.
            Some((verb, rest)) => {
                // Fixed-choice arguments (`:model`, `:adblock`, `:clear`, `:theme`, … —
                // see `commands::arg_candidates`): ghost the first choice matching the
                // token being typed; Tab then cycles the whole set (`accept_suggestion`).
                if let Some((v, prior, prefix)) = completion_parts(cmd) {
                    if let Some(cands) = crate::commands::arg_candidates(self, v, &prior) {
                        return cands
                            .iter()
                            .find(|c| c.starts_with(prefix))
                            .map(|c| format!("{cmd}{}", &c[prefix.len()..]));
                    }
                }
                let rest = rest.trim();
                // Otherwise, history completion for open-like verbs only.
                if rest.is_empty()
                    || !matches!(
                        verb,
                        "open" | "o" | "read" | "research" | "rs" | "nojs" | "tabopen" | "t"
                    )
                {
                    return None;
                }
                let needle = rest.to_ascii_lowercase();
                let disp = self.history.iter().map(|u| history_display(u)).find(|d| {
                    let dl = d.to_ascii_lowercase();
                    dl.starts_with(&needle) || dl.contains(&format!("/{needle}"))
                })?;
                Some(format!("{verb} {disp}"))
            }
        }
    }

    /// Accept the current autocomplete suggestion into the command line (caret to
    /// end). Returns whether there was one to accept. For commands whose argument
    /// is one of a fixed set (`:model`, `:adblock`, `:clear`, `:theme`, … — see
    /// `commands::arg_candidates`), Tab instead cycles the token being typed
    /// through the choices, shell-completion style: each press steps to the
    /// next/previous one (wrapping, `forward` = Tab vs Shift+Tab), so you can
    /// flick through the options in either direction and Enter the one you want.
    pub(crate) fn accept_suggestion(&mut self, forward: bool) -> bool {
        if let Some((verb, prior, prefix)) = completion_parts(&self.command) {
            if let Some(cands) = crate::commands::arg_candidates(self, verb, &prior) {
                let len = cands.len();
                let next = if let Some(i) = cands.iter().position(|c| c == prefix) {
                    if forward { (i + 1) % len } else { (i + len - 1) % len }
                } else if forward {
                    cands.iter().position(|c| c.starts_with(prefix)).unwrap_or(0)
                } else {
                    // Step back from the first matching choice (or the end of the list).
                    cands
                        .iter()
                        .position(|c| c.starts_with(prefix))
                        .map_or(len - 1, |i| (i + len - 1) % len)
                };
                // Swap just the token being completed, keeping the line as typed
                // before it (inserting the separating space after a bare verb).
                let mut base = self.command[..self.command.len() - prefix.len()].to_string();
                if !base.ends_with(char::is_whitespace) {
                    base.push(' ');
                }
                self.command = base + &cands[next];
                self.command_cursor = self.command.len();
                self.command_anchor = None;
                self.cursor_on = true;
                return true;
            }
        }
        let Some(sug) = self.command_suggestion() else { return false };
        self.command = sug;
        self.command_cursor = self.command.len();
        self.command_anchor = None;
        self.cursor_on = true;
        true
    }

    /// Enter the light Insert mode on a web page: the page takes focus and types, but
    /// Esc / clicking away / navigating returns to Normal. Ctrl+V pastes into the field
    /// as usual (it does NOT promote to passthrough — leave Insert first for that).
    /// Web tabs only.
    pub(crate) fn enter_insert(&mut self) {
        if self.active_webview().is_none() {
            self.set_status("no page — open one first");
            return;
        }
        self.mode = ModeKind::Insert;
        self.set_page_mode("insert");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
        }
        // Insert un-hides the chrome while fullscreen (shows [INSERT]); relayout so the
        // page refits to the reserved bar band. No-op when not fullscreen.
        self.relayout_active();
    }

    /// Enter passthrough for the active content: a web page (keys → page, sticky) or a
    /// native terminal (keys → PTY, Ctrl+S leaves). The `:ai` field has its own entry,
    /// [`enter_ai_passthrough`](App::enter_ai_passthrough).
    pub(crate) fn enter_passthrough(&mut self) {
        // Native terminal: the SHELL keeps keyboard focus (there's no webview) and
        // forwards keys to the PTY.
        if self.active_is_term() {
            // Abandon any half-typed vi find-char (f/F/t/T awaiting its target).
            self.term_find_pending = None;
            // Leave copy/vi mode (if active) so the live grid takes input again.
            if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut())
            {
                if s.pty.is_vi() {
                    s.pty.toggle_vi();
                }
                // Resuming the live shell brings the camera back to the prompt:
                // copy-mode (or wheel) scrolling left the display parked in
                // scrollback, and only fresh PTY output would ever snap it back.
                s.pty.scroll_to_bottom();
            }
            self.mode = ModeKind::Passthrough;
            self.window.set_focus();
            self.set_status("terminal — Ctrl+S returns to the shell");
            self.window.request_redraw();
            return;
        }
        if self.active_webview().is_none() {
            self.set_status("no page — open one first");
            return;
        }
        self.mode = ModeKind::Passthrough;
        self.set_page_mode("passthrough");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus();
        }
        // While fullscreen, passthrough un-hides the chrome (so the [PASS] tag shows);
        // relayout so the page refits to the reserved bar band instead of the webview
        // HWND covering — and hiding — the bar. A no-op re-fit when not fullscreen.
        self.relayout_active();
    }

    /// Push the current mode name into the page so the injected bridge knows which
    /// keys to intercept. Called whenever the mode changes.
    pub(crate) fn set_page_mode(&self, mode: &str) {
        if let Some(wv) = self.active_webview() {
            let _ = wv.evaluate_script(&format!("window.__mode={mode:?}"));
        }
    }

    pub(crate) fn exit_to_normal(&mut self) {
        // Leaving a terminal's input (Shift+Esc) enters COPY MODE: flip the engine
        // into Alacritty's own vi mode — a vi cursor over the LIVE colored grid, with
        // grid selection + `selection_to_string` to yank. `i`/`Enter` resumes.
        if self.active_is_term() {
            if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut())
            {
                if !s.pty.is_vi() {
                    s.pty.toggle_vi();
                }
            }
            self.mode = ModeKind::Normal;
            self.window.set_focus();
            self.clear_status();
            self.window.request_redraw();
            return;
        }
        self.page_focus_yielded = false;
        self.set_page_mode("normal");
        if let Some(wv) = self.active_webview() {
            let _ = wv.focus_parent();
        }
        self.mode = ModeKind::Normal;
        // Leaving passthrough re-hides the chrome while fullscreen; relayout so the page
        // grows back over the freed bar band (immersive again). No-op when not fullscreen.
        self.relayout_active();
        self.window.set_focus();
        // Directly SetFocus the parent HWND too: `set_focus` no-ops when we're already
        // the foreground app, which is exactly the "leave passthrough" case — keyboard
        // focus is on the webview child (or an iframe), not the shell window.
        self.reclaim_shell_focus();
        self.window.request_redraw();
    }

    // --- window control (resize / move / fullscreen) --------------------------

    pub(crate) fn key_resize(&mut self, key: &KeyEvent) {
        const STEP: i32 = 40;
        match &key.logical_key {
            Key::Escape | Key::Enter => self.mode = ModeKind::Normal,
            Key::Character(s) => match *s {
                "h" => self.resize_window(-STEP, 0),
                "l" => self.resize_window(STEP, 0),
                "j" => self.resize_window(0, STEP),
                "k" => self.resize_window(0, -STEP),
                _ => {}
            },
            _ => {}
        }
    }

    pub(crate) fn key_move(&mut self, key: &KeyEvent) {
        const STEP: i32 = 40;
        match &key.logical_key {
            Key::Escape | Key::Enter => self.mode = ModeKind::Normal,
            Key::Character(s) => match *s {
                "h" => self.move_window(-STEP, 0),
                "l" => self.move_window(STEP, 0),
                "j" => self.move_window(0, STEP),
                "k" => self.move_window(0, -STEP),
                _ => {}
            },
            _ => {}
        }
    }

    /// Repeatable pane resize (see [`ModeKind::PaneResize`]): h/j/k/l keep sliding the
    /// focused pane's dividers — Shift optional, no need to re-arm Ctrl+W — refreshing
    /// the idle timer each time. Esc/Enter leave; ANY other key leaves too and is then
    /// handled as a normal binding, so nothing is swallowed. Matched on the physical key
    /// so a held Shift (from entering the mode) doesn't change which key is which.
    pub(crate) fn key_pane_resize(&mut self, key: &KeyEvent) {
        let dir = match key.physical_key {
            KeyCode::KeyH => Some('h'),
            KeyCode::KeyJ => Some('j'),
            KeyCode::KeyK => Some('k'),
            KeyCode::KeyL => Some('l'),
            _ => None,
        };
        match dir {
            Some(d) => {
                self.resize_pane(d);
                self.pane_resize_at = std::time::Instant::now();
            }
            None if key.physical_key == KeyCode::Escape || key.physical_key == KeyCode::Enter => {
                self.mode = ModeKind::Normal;
                self.clear_status();
            }
            None => {
                // Any other key ends resize mode and is re-dispatched, so it still acts.
                self.mode = ModeKind::Normal;
                self.clear_status();
                self.key_normal(key);
            }
        }
    }

    /// Move-pane mode (see [`ModeKind::PaneMove`]): h/j/k/l swap the grabbed pane with its
    /// neighbour that way, Enter commits the new arrangement, Esc reverts to the layout
    /// from when it was grabbed. Any other key is ignored so a stray press can't drop the
    /// pane in the wrong place — you leave deliberately with Enter or Esc.
    pub(crate) fn key_pane_move(&mut self, key: &KeyEvent) {
        match key.physical_key {
            KeyCode::KeyH => self.pane_move_swap('h'),
            KeyCode::KeyJ => self.pane_move_swap('j'),
            KeyCode::KeyK => self.pane_move_swap('k'),
            KeyCode::KeyL => self.pane_move_swap('l'),
            KeyCode::Enter => self.commit_pane_move(),
            KeyCode::Escape => self.revert_pane_move(),
            _ => {}
        }
    }
}

/// Split a command line for argument completion: `(verb, args fully typed before
/// the caret's token, the token being completed)`. A trailing space means the
/// caret starts a NEW argument (`"clear cache "` → `("clear", ["cache"], "")`);
/// a bare verb completes its first argument (`"model"` → `("model", [], "")`) —
/// whether that verb HAS fixed choices is `commands::arg_candidates`'s call.
/// `None` for an empty line.
fn completion_parts(line: &str) -> Option<(&str, Vec<&str>, &str)> {
    if line.trim().is_empty() {
        return None;
    }
    match line.split_once(char::is_whitespace) {
        None => Some((line, Vec::new(), "")),
        Some((verb, rest)) => {
            let mut args: Vec<&str> = rest.split_whitespace().collect();
            let prefix = if line.ends_with(char::is_whitespace) {
                ""
            } else {
                args.pop().unwrap_or("")
            };
            Some((verb, args, prefix))
        }
    }
}

/// Map a top-row digit key `1`–`9` to a zero-based index (`Digit1` → 0). Used by the
/// `Ctrl+W <n>` move-pane grab, where the digit is a tab-bar entry number.
fn digit_index(code: KeyCode) -> Option<usize> {
    Some(match code {
        KeyCode::Digit1 => 0,
        KeyCode::Digit2 => 1,
        KeyCode::Digit3 => 2,
        KeyCode::Digit4 => 3,
        KeyCode::Digit5 => 4,
        KeyCode::Digit6 => 5,
        KeyCode::Digit7 => 6,
        KeyCode::Digit8 => 7,
        KeyCode::Digit9 => 8,
        _ => return None,
    })
}

/// A "word" character for command-bar word motions (`Ctrl+W`, `Ctrl+←/→`):
/// alphanumerics and `_`. Everything else — `/ . : - ? & = # @ ~ + …` — is a
/// separator, so word jumps/deletes stop at URL and path boundaries.
pub(crate) fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset of the start of the word before `pos`: skip trailing separators,
/// then the word run. So `Ctrl+W` on `…/foo/bar` erases `bar` (then `/`, then
/// `foo`), not the entire URL.
pub(crate) fn prev_word_boundary(s: &str, pos: usize) -> usize {
    let trimmed = s[..pos].trim_end_matches(|c| !is_word_char(c));
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// Byte offset of the end of the word after `pos`: skip leading separators, then
/// the word run.
pub(crate) fn next_word_boundary(s: &str, pos: usize) -> usize {
    let rest = &s[pos..];
    let after_sep = rest.trim_start_matches(|c| !is_word_char(c));
    let sep = rest.len() - after_sep.len();
    let word = after_sep.find(|c| !is_word_char(c)).unwrap_or(after_sep.len());
    pos + sep + word
}

#[cfg(test)]
mod tests {
    use super::{completion_parts, next_word_boundary, prev_word_boundary};

    #[test]
    fn completion_parts_splits_verb_args_and_completed_token() {
        // A bare verb offers its first argument (empty prefix, space inserted later).
        assert_eq!(completion_parts("model"), Some(("model", vec![], "")));
        // Mid-token: the last token is what's being completed.
        assert_eq!(completion_parts("clear hi"), Some(("clear", vec![], "hi")));
        // A trailing space moves completion to the NEXT argument position.
        assert_eq!(completion_parts("clear cache "), Some(("clear", vec!["cache"], "")));
        assert_eq!(completion_parts("history clear 1"), Some(("history", vec!["clear"], "1")));
        // Empty / whitespace-only lines complete nothing.
        assert_eq!(completion_parts(""), None);
        assert_eq!(completion_parts("   "), None);
    }

    #[test]
    fn ctrl_w_deletes_one_url_segment_at_a_time() {
        let url = "https://example.com/foo/bar";
        // Caret at the end: prev word is `bar`, leaving the trailing slash.
        let p1 = prev_word_boundary(url, url.len());
        assert_eq!(&url[..p1], "https://example.com/foo/");
        // Again from there: skip the `/`, delete `foo`.
        let p2 = prev_word_boundary(url, p1);
        assert_eq!(&url[..p2], "https://example.com/");
        // Not the whole thing in one go.
        assert_ne!(p1, 0);
    }

    #[test]
    fn word_motions_step_over_separators() {
        let s = "ab.cd";
        assert_eq!(next_word_boundary(s, 0), 2); // end of `ab`
        assert_eq!(next_word_boundary(s, 2), 5); // skip `.`, end of `cd`
        assert_eq!(prev_word_boundary(s, 5), 3); // start of `cd`
    }
}
