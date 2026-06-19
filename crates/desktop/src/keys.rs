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
        // Receiving a key here means the shell holds the keyboard, so any prior
        // page-focus yield is over (the hook only forwards Esc as ReclaimNormal).
        self.page_focus_yielded = false;
        match self.mode {
            ModeKind::Command | ModeKind::Find => self.key_command(key),
            ModeKind::Resize => self.key_resize(key),
            ModeKind::Move => self.key_move(key),
            ModeKind::Hint => self.key_hint(key),
            ModeKind::Caret => self.key_caret(key),
            // In Insert/Passthrough the page normally has OS focus and the injected
            // bridge handles the shell keys; these arms are fallbacks for when the
            // shell still holds focus (e.g. right after entering the mode).
            ModeKind::Insert => {
                if self.active_is_ai() {
                    // AI tabs type straight into their native field (no webview).
                    self.key_ai(key);
                } else if matches!(key.logical_key, Key::Escape) && !self.modifiers.shift_key() {
                    self.exit_to_normal();
                } else if self.modifiers.control_key() && key.physical_key == KeyCode::KeyV {
                    self.enter_passthrough();
                }
            }
            ModeKind::Passthrough => {
                // Leave passthrough with Ctrl+S (easy reach) or Shift+Esc (legacy).
                let leave = (self.modifiers.control_key()
                    && key.physical_key == KeyCode::KeyS)
                    || (matches!(key.logical_key, Key::Escape) && self.modifiers.shift_key());
                if leave {
                    self.exit_to_normal();
                } else if self.active_is_term() {
                    // Native terminal: forward every key to the PTY, except a few
                    // shell-owned chords — the global zoom keys (Ctrl +/-/0) and
                    // Ctrl+V, which pastes the clipboard into the PTY (the Windows
                    // convention) rather than sending a literal ^V.
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
                }
                // For a webview in passthrough the page itself has focus and the
                // injected bridge handles keys; nothing to do here.
            }
            ModeKind::Normal => self.key_normal(key),
        }
        self.window.request_redraw();
    }

    pub(crate) fn key_normal(&mut self, key: &KeyEvent) {
        // Ctrl+W window prefix: the next key picks a pane action (vim-window style).
        // h/j/k/l move focus, s/v split (stacked / side-by-side), c/q close the pane.
        // Matched on the physical key so it works whether or not Ctrl is still held.
        if self.pending_window_key {
            self.pending_window_key = false;
            match key.physical_key {
                KeyCode::KeyH => self.move_pane_focus('h'),
                KeyCode::KeyJ => self.move_pane_focus('j'),
                KeyCode::KeyK => self.move_pane_focus('k'),
                KeyCode::KeyL => self.move_pane_focus('l'),
                KeyCode::KeyS => self.split_pane(SplitDir::Col),
                KeyCode::KeyV => self.split_pane(SplitDir::Row),
                KeyCode::KeyC | KeyCode::KeyQ => self.close_active(),
                _ => {}
            }
            return;
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
        if self.active_term_vi() && self.key_term_vi(key) {
            return;
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
                // Ctrl+W: arm the window/pane prefix (next key picks the action).
                KeyCode::KeyW => self.pending_window_key = true,
                // Reopen the last closed tab (the familiar browser shortcut).
                KeyCode::KeyT if self.modifiers.shift_key() => self.reopen_closed(),
                // Half-page scroll (vim Ctrl+D / Ctrl+U).
                KeyCode::KeyD => self.scroll(self.half_page()),
                KeyCode::KeyU => self.scroll(-self.half_page()),
                // Browser-wide zoom (native chrome + web content + terminal).
                KeyCode::Equal => self.zoom_by(1),
                KeyCode::Minus => self.zoom_by(-1),
                KeyCode::Digit0 => self.zoom_reset(),
                _ => {}
            }
            return;
        }
        match &key.logical_key {
            Key::Character(s) => match *s {
                ":" => self.enter_command(""),
                // `o` opens in THIS tab; `O` opens in a new tab (prefills `:open -t`).
                "o" => self.enter_command("open "),
                "O" => self.enter_command("open -t "),
                "j" => self.scroll(80),
                "k" => self.scroll(-80),
                // Reopen the last closed tab (vim-style undo; also Ctrl+Shift+T).
                "u" => self.reopen_closed(),
                "g" => self.scroll_edge(false),
                "G" => self.scroll_edge(true),
                "/" => self.enter_find(),
                "i" => {
                    if self.active_is_term() {
                        self.enter_passthrough();
                    } else if self.active_is_ai() {
                        self.enter_ai_insert();
                    } else {
                        self.enter_insert();
                    }
                }
                "f" => self.enter_hint(false),
                // Shift+F: hints open the picked link in a NEW tab (like `:open -t`).
                "F" => self.enter_hint(true),
                // Caret/visual selection — highlight & yank text with vim motions.
                // Read tabs use the native caret; web tabs (open/research) use the
                // injected page caret. Terminal tabs are excluded.
                "v" => {
                    if self.active_is_read_native() {
                        self.enter_read_caret(false);
                    } else if self.active_webview().is_some() && !self.active_is_term() {
                        self.enter_web_caret(false);
                    }
                }
                "V" => {
                    if self.active_is_read_native() {
                        self.enter_read_caret(true);
                    } else if self.active_webview().is_some() && !self.active_is_term() {
                        self.enter_web_caret(true);
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
            // Enter on a `:history` line opens that entry (Shift+Enter → new tab);
            // a no-op anywhere else.
            Key::Enter => self.open_history_entry(self.modifiers.shift_key()),
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

    /// Enter read-mode caret/visual selection (`v` charwise, `V` linewise): build a
    /// caret over the read view's visual lines, place it near the middle of the
    /// viewport, and start a visual selection so motions immediately highlight text.
    pub(crate) fn enter_read_caret(&mut self, linewise: bool) {
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
        tb.key(if linewise { vim::Key::Char('V') } else { vim::Key::Char('v') }, rows, cols);
        nr.scroll = tb.top as i32 * lh;
        nr.caret = Some(tb);
        self.set_status("[VISUAL]  motions select · y yank · Esc exit");
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
            self.window.request_redraw();
        }
        consumed || exit
    }

    /// Enter caret/visual browsing on a WEB tab: tell the injected page caret to
    /// place itself at the viewport center and start a visual selection. The shell
    /// keeps keyboard focus (like hint mode) and forwards motions via `key_caret`.
    pub(crate) fn enter_web_caret(&mut self, linewise: bool) {
        let Some(wv) = self.active_webview() else { return };
        let _ = wv.evaluate_script(&format!(
            "window.__caretEnter&&window.__caretEnter({})",
            if linewise { "true" } else { "false" }
        ));
        self.mode = ModeKind::Caret;
        self.set_status("[CARET]  hjkl/w/b/0/$/gg/G move · v select · y yank · Esc exit");
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
                    if !self.accept_suggestion() {
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
            // Tab accepts the autocomplete suggestion (a no-op if there isn't one).
            Key::Tab => {
                self.accept_suggestion();
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
                let rest = rest.trim();
                // `:model` completes/cycles the model list (Tab advances — see
                // `accept_suggestion`); show the first matching id as the ghost.
                if verb == "model" {
                    let m = if rest.is_empty() {
                        Some(crate::ai::MODELS[0])
                    } else {
                        crate::ai::MODELS.iter().find(|m| m.starts_with(rest)).copied()
                    };
                    return m.map(|m| format!("model {m}"));
                }
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
    /// end). Returns whether there was one to accept.
    pub(crate) fn accept_suggestion(&mut self) -> bool {
        // `:model` Tab-cycles through the model list, shell-completion style: each
        // press advances to the next id (wrapping), so you can flick through the
        // options and Enter the one you want.
        let model_arg = match self.command.as_str() {
            "model" => Some(""),
            s => s.strip_prefix("model ").map(str::trim),
        };
        if let Some(arg) = model_arg {
            let models = crate::ai::MODELS;
            let next = if let Some(i) = models.iter().position(|m| *m == arg) {
                (i + 1) % models.len()
            } else {
                models.iter().position(|m| m.starts_with(arg)).unwrap_or(0)
            };
            self.command = format!("model {}", models[next]);
            self.command_cursor = self.command.len();
            self.command_anchor = None;
            self.cursor_on = true;
            return true;
        }
        let Some(sug) = self.command_suggestion() else { return false };
        self.command = sug;
        self.command_cursor = self.command.len();
        self.command_anchor = None;
        self.cursor_on = true;
        true
    }

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
    }

    pub(crate) fn enter_passthrough(&mut self) {
        // Native terminal: Passthrough is the terminal's input mode, but the SHELL
        // keeps keyboard focus (there's no webview) and forwards keys to the PTY.
        if self.active_is_term() {
            // Leave copy/vi mode (if active) so the live grid takes input again.
            if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut())
            {
                if s.pty.is_vi() {
                    s.pty.toggle_vi();
                }
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
    use super::{next_word_boundary, prev_word_boundary};

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
