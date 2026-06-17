//! The `:ai` tab — a native, engine-free tab (no WebView2) that asks Groq's
//! cloud LLMs. It is a first-class part of the shell: in Normal mode it's a
//! read-only **vim buffer** (the same one `:read`/`:res`/`:error` use) so every
//! motion, visual selection, yank and find works exactly like the rest of the
//! browser — including selecting/copying the "get a key" link. Press `i` to type
//! into the field (Insert mode); the request runs on a background thread so the
//! shell never blocks. The API key and chosen model are pasted/set once and
//! persisted, so later `:ai` invocations drop straight into the prompt.

use serde::{Deserialize, Serialize};
use tao::event::KeyEvent;
use tao::keyboard::{Key, KeyCode};

use std::path::PathBuf;

use crate::{clipboard_get, clipboard_set, draw, vim};
use crate::tabs::TabContent;
use crate::{App, ModeKind, Tab, UserEvent};

/// Groq's OpenAI-compatible chat-completions endpoint.
const GROQ_URL: &str = "https://api.groq.com/openai/v1/chat/completions";
/// Default model: cloud Llama 70B on Groq.
pub(crate) const DEFAULT_MODEL: &str = "llama-3.3-70b-versatile";
/// A handful of known Groq model ids offered by `:model` (any id is accepted —
/// these are just the quick suggestions). An unknown id simply surfaces Groq's own
/// error in the tab, so the list doesn't have to be exhaustive.
pub(crate) const MODELS: &[&str] = &[
    "llama-3.3-70b-versatile",
    "llama-3.1-8b-instant",
    "openai/gpt-oss-120b",
    "openai/gpt-oss-20b",
    "moonshotai/kimi-k2-instruct",
    "qwen/qwen3-32b",
    "deepseek-r1-distill-llama-70b",
];
/// Keep answers terse — this is a quick-question surface, not a chat companion.
const SYSTEM_PROMPT: &str =
    "You are a fast, concise assistant inside a terminal browser. Answer directly \
     and briefly, with no preamble or sign-off. For conversions or calculations, \
     give the result first.";

/// How many past conversations to keep on disk (oldest dropped first).
const AI_CHATS_CAP: usize = 100;

/// Who said a line in the conversation.
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) enum AiRole {
    You,
    Ai,
    Err,
}

/// One turn in the conversation.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AiMessage {
    pub(crate) role: AiRole,
    pub(crate) text: String,
}

/// State for a `:ai` tab. The conversation is rendered into `buf` (a vim text
/// buffer) every draw, so Normal-mode motions/visual/yank/find operate on it just
/// like a `:read`/`:res` tab; `colors` is the parallel per-line colour used only at
/// paint time. `input` is the field being typed in Insert mode.
pub(crate) struct AiState {
    /// Stable id so an async reply finds its tab even if the tab order changed.
    pub(crate) id: u64,
    pub(crate) messages: Vec<AiMessage>,
    pub(crate) input: String,
    pub(crate) pending: bool,
    /// A prompt to run as soon as a key is available (`:ai <q>` before any key).
    pub(crate) pending_prompt: Option<String>,
    /// The rendered conversation as a navigable read-only vim buffer.
    pub(crate) buf: vim::TextBuffer,
    /// Per-line colour, parallel to `buf.lines` (rebuilt with it each draw).
    pub(crate) colors: Vec<draw::Rgb>,
    /// Chat-style "stick to the bottom" on new content; cleared when the cursor
    /// leaves the last line, re-armed when it returns there (or on a new message).
    pub(crate) follow: bool,
    /// Index into [`App::ai_chats`] of the conversation this tab is showing, or
    /// `None` for a fresh draft not yet saved. `H`/`L` step it through the history.
    pub(crate) chat_idx: Option<usize>,
}

impl AiState {
    fn new(id: u64, pending_prompt: Option<String>) -> AiState {
        AiState {
            id,
            messages: Vec::new(),
            input: String::new(),
            pending: false,
            pending_prompt,
            buf: vim::TextBuffer::new(Vec::new()),
            colors: Vec::new(),
            follow: true,
            chat_idx: None,
        }
    }
}

/// Where the Groq API key / model are stored (plain text in the app data dir,
/// beside the session file).
fn data_file(name: &str) -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "browser").map(|d| d.data_dir().join(name))
}

fn read_trimmed(name: &str) -> Option<String> {
    let s = std::fs::read_to_string(data_file(name)?).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn write_file(name: &str, value: &str) {
    let Some(path) = data_file(name) else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, value.trim());
}

pub(crate) fn load_key() -> Option<String> {
    read_trimmed("groq.key")
}
pub(crate) fn save_key(key: &str) {
    write_file("groq.key", key);
}
pub(crate) fn load_model() -> Option<String> {
    read_trimmed("groq.model")
}
pub(crate) fn save_model(model: &str) {
    write_file("groq.model", model);
}

/// Load past conversations (newest last), or an empty list if none/unreadable.
pub(crate) fn load_chats() -> Vec<Vec<AiMessage>> {
    let Some(path) = data_file("groq.chats.json") else { return Vec::new() };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist all conversations (best-effort; failures are ignored).
pub(crate) fn save_chats(chats: &[Vec<AiMessage>]) {
    let Some(path) = data_file("groq.chats.json") else { return };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string(chats) {
        let _ = std::fs::write(path, s);
    }
}

/// Ask Groq (blocking — call from a background thread). Sends the whole
/// conversation so follow-ups keep context; returns the assistant's reply text or
/// a human-readable error string.
pub(crate) fn ask_blocking(key: &str, model: &str, history: &[AiMessage]) -> Result<String, String> {
    let mut msgs = vec![serde_json::json!({"role": "system", "content": SYSTEM_PROMPT})];
    for m in history {
        let role = match m.role {
            AiRole::You => "user",
            AiRole::Ai => "assistant",
            AiRole::Err => continue, // local errors aren't part of the conversation
        };
        msgs.push(serde_json::json!({"role": role, "content": m.text}));
    }
    let body = serde_json::json!({ "model": model, "messages": msgs, "temperature": 0.4 });
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(40))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .post(GROQ_URL)
        .bearer_auth(key)
        .json(&body)
        .send()
        .map_err(|e| e.to_string())?;
    let status = resp.status();
    let val: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    if !status.is_success() {
        let msg = val
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("request failed");
        return Err(format!("groq {}: {msg}", status.as_u16()));
    }
    val.get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "empty response".to_string())
}

/// Greedily wrap `text` to `cols` monospace columns, hard-breaking any word longer
/// than the column count, and preserving explicit newlines (blank lines included).
fn wrap(text: &str, cols: usize, out: &mut Vec<String>) {
    let cols = cols.max(1);
    for para in text.split('\n') {
        let mut line = String::new();
        let mut any = false;
        for word in para.split_whitespace() {
            any = true;
            if word.chars().count() > cols {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                let chars: Vec<char> = word.chars().collect();
                for chunk in chars.chunks(cols) {
                    let c: String = chunk.iter().collect();
                    if c.chars().count() == cols {
                        out.push(c);
                    } else {
                        line = c;
                    }
                }
                continue;
            }
            let need = if line.is_empty() {
                word.chars().count()
            } else {
                line.chars().count() + 1 + word.chars().count()
            };
            if need > cols {
                out.push(std::mem::take(&mut line));
            } else if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        if !line.is_empty() {
            out.push(line);
        } else if !any {
            out.push(String::new());
        }
    }
}

/// Append `text` to `lines`/`colors` as `cols`-wrapped lines, all in `color`.
fn push_line(
    lines: &mut Vec<String>,
    colors: &mut Vec<draw::Rgb>,
    text: &str,
    color: draw::Rgb,
    cols: usize,
) {
    let mut w = Vec::new();
    wrap(text, cols, &mut w);
    if w.is_empty() {
        w.push(String::new());
    }
    for l in w {
        lines.push(l);
        colors.push(color);
    }
}

/// Build the wrapped (line, colour) pairs for a `:ai` tab at `cols` columns: a
/// header, then the key-entry prompt (no key yet) or the conversation. The LAST
/// line is always the live input field (or the "thinking" indicator), so the caret
/// renderer can find it.
fn render(ai: &AiState, model: &str, has_key: bool, cols: usize) -> (Vec<String>, Vec<draw::Rgb>) {
    let mut lines = Vec::new();
    let mut colors = Vec::new();
    push_line(&mut lines, &mut colors, &format!("ai — {model}"), draw::AI, cols);
    push_line(&mut lines, &mut colors, "", draw::DIM, cols);

    if !has_key {
        push_line(&mut lines, &mut colors, "Paste your Groq API key to begin.", draw::FG, cols);
        push_line(&mut lines, &mut colors, "Press i to type/paste, then Enter to save.", draw::DIM, cols);
        push_line(
            &mut lines,
            &mut colors,
            "Get a free key at https://console.groq.com/keys",
            draw::DIM,
            cols,
        );
        push_line(&mut lines, &mut colors, "", draw::DIM, cols);
        // Mask the key as it's pasted/typed.
        let masked: String = "•".repeat(ai.input.chars().count());
        push_line(&mut lines, &mut colors, &format!("› {masked}"), draw::ACCENT, cols);
        return (lines, colors);
    }

    if ai.messages.is_empty() {
        push_line(&mut lines, &mut colors, "Press i, ask a question, Enter to send.", draw::DIM, cols);
        push_line(&mut lines, &mut colors, "", draw::DIM, cols);
    }
    for m in &ai.messages {
        match m.role {
            AiRole::You => push_line(&mut lines, &mut colors, &format!("you › {}", m.text), draw::ACCENT, cols),
            AiRole::Ai => push_line(&mut lines, &mut colors, &m.text, draw::FG, cols),
            AiRole::Err => push_line(&mut lines, &mut colors, &format!("⚠ {}", m.text), draw::ERR, cols),
        }
        push_line(&mut lines, &mut colors, "", draw::DIM, cols);
    }
    if ai.pending {
        push_line(&mut lines, &mut colors, "· thinking…", draw::DIM, cols);
    } else {
        push_line(&mut lines, &mut colors, &format!("› {}", ai.input), draw::ACCENT, cols);
    }
    (lines, colors)
}

impl App {
    /// `:ai [prompt]` — open a native AI tab. With a key already saved and a prompt
    /// given, the question is asked immediately (Normal mode, so the answer can be
    /// read/scrolled/yanked). Otherwise it drops straight into the field in Insert
    /// mode, ready to paste a key or type a question; a prompt given before any key
    /// is stashed and run the moment the key is entered.
    pub(crate) fn open_ai_tab(&mut self, prompt: &str) {
        let id = self.next_ai_id;
        self.next_ai_id += 1;
        let prompt = prompt.trim();
        let has_key = self.groq_key.is_some();
        let pending_prompt = (!prompt.is_empty() && !has_key).then(|| prompt.to_string());
        let tab = Tab {
            url: "browser://ai".into(),
            nojs: false,
            read: false,
            research: false,
            content: TabContent::Ai(AiState::new(id, pending_prompt)),
        };
        self.place_tab(tab, true);
        self.window.set_focus();
        self.clear_status();
        if !prompt.is_empty() && has_key {
            self.ask_ai(prompt.to_string());
            self.mode = ModeKind::Normal;
        } else {
            self.enter_ai_insert();
        }
        self.window.request_redraw();
    }

    /// Whether the active tab is a `:ai` tab.
    pub(crate) fn active_is_ai(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.ai().is_some())
    }

    /// Mutable access to the active `:ai` tab's state, if any.
    pub(crate) fn active_ai_mut(&mut self) -> Option<&mut AiState> {
        self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.ai_mut())
    }

    /// Enter Insert mode on the AI tab (type into its field). The shell keeps
    /// keyboard focus — there's no webview — and forwards keys via [`key_ai`].
    pub(crate) fn enter_ai_insert(&mut self) {
        if let Some(ai) = self.active_ai_mut() {
            ai.follow = true; // keep the input line in view while typing
        } else {
            return;
        }
        self.mode = ModeKind::Insert;
        self.window.set_focus();
        self.window.request_redraw();
    }

    /// Set/persist the model used for future requests (`:model <id>`).
    pub(crate) fn set_ai_model(&mut self, name: &str) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return;
        }
        save_model(&name);
        self.set_status(format!("model set to {name}"));
        self.ai_model = name;
        self.window.request_redraw();
    }

    /// Handle a key while typing in an AI tab's field (Insert mode). Enter submits,
    /// Esc returns to Normal, Ctrl+V pastes, Ctrl+U clears the line.
    pub(crate) fn key_ai(&mut self, key: &KeyEvent) {
        if self.modifiers.control_key() {
            match key.physical_key {
                KeyCode::KeyV => {
                    if let Some(text) = clipboard_get() {
                        if let Some(ai) = self.active_ai_mut() {
                            // One-line field: drop control chars (e.g. a key copied
                            // with a trailing newline).
                            let clean: String = text.chars().filter(|c| !c.is_control()).collect();
                            ai.input.push_str(clean.trim());
                        }
                    }
                }
                KeyCode::KeyU => {
                    if let Some(ai) = self.active_ai_mut() {
                        ai.input.clear();
                    }
                }
                _ => {}
            }
            self.window.request_redraw();
            return;
        }
        match &key.logical_key {
            Key::Escape => self.exit_to_normal(),
            Key::Enter => self.submit_ai(),
            Key::Backspace => {
                if let Some(ai) = self.active_ai_mut() {
                    ai.input.pop();
                }
            }
            Key::Space => {
                if let Some(ai) = self.active_ai_mut() {
                    ai.input.push(' ');
                }
            }
            Key::Character(s) => {
                if let Some(ai) = self.active_ai_mut() {
                    ai.input.push_str(s);
                }
            }
            _ => {}
        }
        if let Some(ai) = self.active_ai_mut() {
            ai.follow = true;
        }
        self.window.request_redraw();
    }

    /// Feed a key to the active AI tab's vim buffer (Normal mode): motions, visual
    /// selection, and yank — exactly like the `:error`/`:res` pager. Returns whether
    /// it was consumed. `i`/`a` are NOT consumed, so they fall through to the shell's
    /// Insert-mode binding; everything the buffer doesn't recognise (`:`/x/digits/…)
    /// also falls through to the normal browser bindings.
    pub(crate) fn key_ai_nav(&mut self, key: &KeyEvent) -> bool {
        // Let `i`/`a` reach the shell so they can enter the AI field.
        if let Key::Character(s) = &key.logical_key {
            if *s == "i" || *s == "a" {
                return false;
            }
        }
        let Some(vk) = self.map_vim_key(key) else { return false };
        let (w, _) = self.inner();
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let cols = (((w as usize).saturating_sub(16)) / cw).max(1);
        let rows = (self.content_view_h() as usize / line_h).max(1);

        let mut yanked = None;
        let consumed;
        {
            let Some(ai) = self.active_ai_mut() else { return false };
            let res = ai.buf.key(vk, rows, cols);
            consumed = res.consumed;
            yanked = res.yanked.or(yanked);
            if consumed {
                // Re-arm "follow the bottom" only when the cursor is on the last line.
                ai.follow = ai.buf.cy + 1 >= ai.buf.lines.len();
            }
        }
        if let Some(text) = yanked {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        }
        if consumed {
            self.window.request_redraw();
        }
        consumed
    }

    /// Enter pressed in the AI field: the first submission with no saved key takes
    /// the line as the key (and runs any stashed prompt); afterwards each line is a
    /// question.
    fn submit_ai(&mut self) {
        if self.groq_key.is_none() {
            let key = self
                .active_ai_mut()
                .map(|ai| std::mem::take(&mut ai.input))
                .unwrap_or_default()
                .trim()
                .to_string();
            if key.is_empty() {
                return;
            }
            save_key(&key);
            self.groq_key = Some(key);
            self.set_status("groq key saved");
            if let Some(p) = self.active_ai_mut().and_then(|ai| ai.pending_prompt.take()) {
                self.ask_ai(p);
            }
            self.window.request_redraw();
            return;
        }
        // Don't pile on a second request while one is still in flight.
        if self.active_ai_mut().is_some_and(|ai| ai.pending) {
            return;
        }
        let prompt = self
            .active_ai_mut()
            .map(|ai| std::mem::take(&mut ai.input))
            .unwrap_or_default()
            .trim()
            .to_string();
        if !prompt.is_empty() {
            self.ask_ai(prompt);
        }
    }

    /// Record `prompt` as a question, mark the tab pending, and fire the Groq
    /// request on a background thread; the reply returns via [`UserEvent::AiReply`].
    pub(crate) fn ask_ai(&mut self, prompt: String) {
        let Some(key) = self.groq_key.clone() else { return };
        let model = self.ai_model.clone();
        let (id, history) = {
            let Some(ai) = self.active_ai_mut() else { return };
            ai.messages.push(AiMessage { role: AiRole::You, text: prompt });
            ai.pending = true;
            ai.follow = true;
            (ai.id, ai.messages.clone())
        };
        // Save the conversation (with the new question) right away, so it survives
        // even if the reply never arrives.
        self.commit_ai(id);
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = ask_blocking(&key, &model, &history);
            let _ = proxy.send_event(UserEvent::AiReply { id, result });
        });
        self.window.request_redraw();
    }

    /// Persist the conversation in the AI tab with this id into [`App::ai_chats`]
    /// (creating its entry on first save, then updating it in place) and write the
    /// whole history to disk. Trims the oldest chats past the cap, fixing up every
    /// open tab's `chat_idx` so it keeps pointing at the right conversation.
    fn commit_ai(&mut self, id: u64) {
        let found = self.tabs.iter().find_map(|t| {
            t.ai().filter(|a| a.id == id).map(|a| (a.chat_idx, a.messages.clone()))
        });
        let Some((idx, msgs)) = found else { return };
        if msgs.is_empty() {
            return;
        }
        let mut new_idx = match idx {
            Some(i) if i < self.ai_chats.len() => {
                self.ai_chats[i] = msgs;
                i
            }
            _ => {
                self.ai_chats.push(msgs);
                self.ai_chats.len() - 1
            }
        };
        if self.ai_chats.len() > AI_CHATS_CAP {
            let drop = self.ai_chats.len() - AI_CHATS_CAP;
            self.ai_chats.drain(0..drop);
            new_idx = new_idx.saturating_sub(drop);
            // A tab pointing into the dropped range becomes a draft (None).
            for tab in &mut self.tabs {
                if let Some(a) = tab.ai_mut() {
                    a.chat_idx = a.chat_idx.and_then(|c| c.checked_sub(drop));
                }
            }
        }
        for tab in &mut self.tabs {
            if let Some(a) = tab.ai_mut() {
                if a.id == id {
                    a.chat_idx = Some(new_idx);
                    break;
                }
            }
        }
        save_chats(&self.ai_chats);
    }

    /// `H` / `L` on an AI tab: step back (`forward = false`) / forward through the
    /// saved conversations, loading the target chat into the tab. The position just
    /// past the newest chat is a fresh draft, so `L` from the newest opens a clean
    /// screen and `H` from a draft returns to the latest chat.
    pub(crate) fn ai_history(&mut self, forward: bool) {
        let n = self.ai_chats.len();
        if self.active_ai_mut().is_some_and(|a| a.pending) {
            self.set_status("waiting for the reply…");
            return;
        }
        if n == 0 {
            self.set_status("no past chats");
            return;
        }
        // Draft (no chat_idx) sits at position `n`, just past the newest chat.
        let cur = self.active_ai_mut().map(|a| a.chat_idx.unwrap_or(n)).unwrap_or(n);
        let target = if forward { (cur + 1).min(n) } else { cur.saturating_sub(1) };
        if target == cur {
            self.set_status(if forward { "newest chat" } else { "oldest chat" });
            return;
        }
        // Clone the target conversation before re-borrowing the tab mutably.
        let load = (target < n).then(|| self.ai_chats[target].clone());
        let Some(ai) = self.active_ai_mut() else { return };
        match load {
            Some(msgs) => {
                ai.messages = msgs;
                ai.chat_idx = Some(target);
            }
            None => {
                ai.messages.clear();
                ai.chat_idx = None;
            }
        }
        ai.input.clear();
        ai.follow = true;
        let label = if target == n {
            "new chat".to_string()
        } else {
            format!("chat {}/{}", target + 1, n)
        };
        self.set_status(label);
        self.window.request_redraw();
    }

    /// A Groq reply (or error) arrived for the AI tab with this id: append it and
    /// stick the view to the bottom.
    pub(crate) fn ai_reply(&mut self, id: u64, result: Result<String, String>) {
        for tab in &mut self.tabs {
            if let Some(ai) = tab.ai_mut() {
                if ai.id == id {
                    ai.pending = false;
                    match result {
                        Ok(text) => ai.messages.push(AiMessage { role: AiRole::Ai, text }),
                        Err(e) => ai.messages.push(AiMessage { role: AiRole::Err, text: e }),
                    }
                    ai.follow = true;
                    break;
                }
            }
        }
        // Re-save so the persisted chat reflects the latest answer (or error).
        self.commit_ai(id);
        self.window.request_redraw();
    }

    /// Rebuild each visible AI pane's vim buffer (wrapped to its current width) and
    /// snap it to the bottom while following. Called at the top of `draw` — cheap,
    /// the text is small, and `set_lines` preserves the cursor/selection so a
    /// rebuild never disturbs an in-progress navigation. Mirrors
    /// [`refresh_read_layout`](App::refresh_read_layout).
    pub(crate) fn refresh_ai_layout(&mut self) {
        let cw = self.painter.measure("M").max(1);
        let line_h = self.painter.line_height().max(1);
        let model = self.ai_model.clone();
        let has_key = self.groq_key.is_some();
        let (panes, _) = self.pane_layout();
        for (tab, rect) in panes {
            if self.tabs.get(tab).map(|t| t.ai().is_none()).unwrap_or(true) {
                continue;
            }
            let cols = (((rect.w as usize).saturating_sub(16)) / cw).max(1);
            let rows = ((rect.h as usize) / line_h).max(1);
            let (lines, colors) = render(self.tabs[tab].ai().unwrap(), &model, has_key, cols);
            let ai = self.tabs[tab].ai_mut().unwrap();
            ai.buf.set_lines(lines);
            ai.colors = colors;
            // Follow the bottom (chat-style) unless the user is mid-selection.
            if ai.follow && ai.buf.anchor.is_none() {
                let n = ai.buf.lines.len();
                ai.buf.cy = n.saturating_sub(1);
                ai.buf.cx = 0;
                ai.buf.left = 0;
                ai.buf.top = n.saturating_sub(rows);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_on_words_and_hard_breaks_long_tokens() {
        let mut out = Vec::new();
        wrap("the quick brown fox", 9, &mut out);
        assert_eq!(out, vec!["the quick", "brown fox"]);

        let mut out = Vec::new();
        wrap("supercalifragilistic", 5, &mut out);
        assert_eq!(out, vec!["super", "calif", "ragil", "istic"]);
    }

    #[test]
    fn wrap_preserves_blank_lines() {
        let mut out = Vec::new();
        wrap("a\n\nb", 10, &mut out);
        assert_eq!(out, vec!["a", "", "b"]);
    }

    #[test]
    fn render_input_is_always_last_and_the_key_link_is_its_own_line() {
        // With a key: the input field is the final line, ready for the caret.
        let mut ai = AiState::new(0, None);
        ai.messages.push(AiMessage { role: AiRole::You, text: "hi".into() });
        ai.input = "562 seconds".into();
        let (lines, _) = render(&ai, "m", true, 40);
        assert_eq!(lines.last().unwrap(), "› 562 seconds");

        // Without a key: the get-a-key URL is a standalone line, so it can be
        // selected/yanked with vim motions.
        let ai = AiState::new(1, None);
        let (lines, _) = render(&ai, "m", false, 60);
        assert!(lines.iter().any(|l| l == "Get a free key at https://console.groq.com/keys"));
    }
}
