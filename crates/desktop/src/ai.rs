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
/// Default model: OpenAI's open-weight gpt-oss-120b on Groq. It's free, fast on
/// Groq, and the most RELIABLE tool-caller of the free models for our agentic browser
/// control (it natively targets the OpenAI function-calling format this code uses) —
/// llama-3.3 frequently malforms tool calls (the `tool_use_failed` 400s we recover
/// from). `moonshotai/kimi-k2-instruct` is the strongest alternative for long
/// multi-step planning; switch with `:model`.
pub(crate) const DEFAULT_MODEL: &str = "openai/gpt-oss-120b";
/// A handful of known Groq model ids offered by `:model` (any id is accepted — these
/// are just the quick suggestions, best tool-callers first). An unknown id simply
/// surfaces Groq's own error in the tab, so the list doesn't have to be exhaustive.
pub(crate) const MODELS: &[&str] = &[
    "openai/gpt-oss-120b",
    "moonshotai/kimi-k2-instruct",
    "openai/gpt-oss-20b",
    "qwen/qwen3-32b",
    "llama-3.3-70b-versatile",
    "llama-3.1-8b-instant",
    "deepseek-r1-distill-llama-70b",
];
/// Keep answers terse — this is a quick-question surface, not a chat companion.
const SYSTEM_PROMPT: &str =
    "You are an assistant embedded in a terminal browser, and you can OPERATE it, not \
     just talk about it. You have tools to open pages, split the layout into panes, \
     switch/close/reopen tabs, navigate, clear browsing data, and set aliases. \
     \
     When the user asks you to DO something — in any phrasing — carry it out with the \
     tools instead of explaining how. Break a request into a sequence of tool calls \
     and make them ONE AT A TIME: you'll see each result and then continue with the \
     next step. For example, to put two sites side by side: open the first, then \
     split (vertical), then open the second in the new pane. Plan bigger requests \
     (\"set up a layout for web dev\") as such a sequence. Don't ask for confirmation \
     — just do it — and when the whole task is finished, reply with one short \
     sentence summarizing what you did. \
     \
     For questions or chit-chat that don't require operating the browser, just answer \
     directly and briefly, with no preamble. For conversions/calculations, give the \
     result first.";

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

/// A browser action the model chose to run (an OpenAI tool-call): the call `id` (for
/// the tool-result message that reports back), the action `name`, and its JSON `args`
/// object, routed to [`App::perform`](crate::App::perform).
pub(crate) struct ToolCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) args: serde_json::Value,
}

/// What one Groq turn produced: a final text answer, or tool-calls to run before
/// continuing. Tool-calls are executed on the main thread (they mutate browser
/// state); their results are fed back and the model is asked again, so it can chain
/// actions into a multi-step task (an "agentic" loop). `assistant_msg` is the raw
/// assistant message (with its `tool_calls`) that must precede the tool results in
/// the next request.
pub(crate) enum AiStep {
    Done(String),
    Calls { calls: Vec<ToolCall>, assistant_msg: serde_json::Value },
}

/// Cap on tool-execution rounds per user prompt, so a confused model can't loop
/// forever calling tools. Generous enough for a big "set up my layout" request.
pub(crate) const MAX_TOOL_ROUNDS: u32 = 16;

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

/// Build the OpenAI-format message list for a fresh turn: the system prompt followed
/// by the visible chat history (errors are local-only, so they're skipped). The new
/// user prompt must already be the last `AiMessage` in `history`.
pub(crate) fn build_convo(history: &[AiMessage]) -> Vec<serde_json::Value> {
    let mut msgs = vec![serde_json::json!({"role": "system", "content": SYSTEM_PROMPT})];
    for m in history {
        let role = match m.role {
            AiRole::You => "user",
            AiRole::Ai => "assistant",
            AiRole::Err => continue,
        };
        msgs.push(serde_json::json!({"role": role, "content": m.text}));
    }
    msgs
}

/// One round-trip to Groq (blocking — call from a background thread). Sends the
/// running `messages` plus the browser tools and returns the model's next step:
/// either a final text answer or tool-calls to run. Tool calls are forced
/// sequential (`parallel_tool_calls=false`) — the model emits one at a time, which
/// both sidesteps Groq's parallel-call generation failures and gives the agentic
/// loop a clean one-action-at-a-time cadence.
pub(crate) fn ask_raw(
    key: &str,
    model: &str,
    messages: &[serde_json::Value],
) -> Result<AiStep, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": messages,
        "temperature": 0.4,
        "tools": crate::actions::groq_tools(),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
    });
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
        let err = val.get("error");
        // Groq's `tool_use_failed` 400: the model tried to call a tool but its output
        // didn't conform to the API, so generation itself failed (common on llama-3.x).
        // The raw attempt is in `failed_generation`; recover the intended calls from it
        // so a flaky tool-formatter still drives the browser instead of just erroring.
        if let Some(fg) = err.and_then(|e| e.get("failed_generation")).and_then(|f| f.as_str()) {
            if let Some(step) = build_calls_step(recover_calls(fg), serde_json::Value::Null) {
                return Ok(step);
            }
        }
        let msg = err
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("request failed");
        return Err(format!("groq {}: {msg}", status.as_u16()));
    }
    let message = val.get("choices").and_then(|c| c.get(0)).and_then(|c| c.get("message"));
    // A tool-call response: the model chose to operate the browser. Normalise each call
    // (recovering args crammed into the name) and build the step; `content` is usually
    // empty here. `build_calls_step` drops unknown names and rebuilds the echoed
    // assistant message from the cleaned calls, so the next round can't fail validation.
    if let Some(tcs) =
        message.and_then(|m| m.get("tool_calls")).and_then(|t| t.as_array()).filter(|a| !a.is_empty())
    {
        let pairs: Vec<(String, serde_json::Value)> = tcs
            .iter()
            .filter_map(|tc| {
                let f = tc.get("function")?;
                let name_raw = f.get("name")?.as_str()?;
                let args_str = f.get("arguments").and_then(|a| a.as_str()).unwrap_or("{}");
                Some(normalize_call(name_raw, args_str))
            })
            .collect();
        let content =
            message.and_then(|m| m.get("content")).cloned().unwrap_or(serde_json::Value::Null);
        if let Some(step) = build_calls_step(pairs, content) {
            return Ok(step);
        }
    }
    message
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(AiStep::Done)
        .ok_or_else(|| "empty response".to_string())
}

/// Turn parsed/recovered `(name, args)` pairs into an [`AiStep::Calls`], synthesising
/// stable `call_<i>` ids and DROPPING any pair whose name isn't a real action — both
/// because executing a bogus name only errors and because echoing it would make the
/// next request 400 (Groq validates the assistant message's `tool_calls` against the
/// request's `tools`). The echoed `assistant_msg` is rebuilt from the surviving calls,
/// so its names/args are always clean. Returns `None` if nothing usable remains.
fn build_calls_step(
    pairs: Vec<(String, serde_json::Value)>,
    content: serde_json::Value,
) -> Option<AiStep> {
    let calls: Vec<ToolCall> = pairs
        .into_iter()
        .enumerate()
        .filter_map(|(i, (name, args))| {
            crate::actions::is_action(&name).then_some(ToolCall { id: format!("call_{i}"), name, args })
        })
        .collect();
    if calls.is_empty() {
        return None;
    }
    let tool_calls: Vec<serde_json::Value> = calls
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "type": "function",
                "function": { "name": c.name, "arguments": c.args.to_string() },
            })
        })
        .collect();
    let assistant_msg =
        serde_json::json!({ "role": "assistant", "content": content, "tool_calls": tool_calls });
    Some(AiStep::Calls { calls, assistant_msg })
}

/// Recover tool calls from Groq's `failed_generation` text — the raw output the model
/// produced when its tool call didn't conform to the API (the `tool_use_failed` 400).
/// Handles the two shapes seen in the wild: llama's `<function=NAME>{json}</function>`
/// pseudo-syntax, and bare `{"name":...,"arguments":{...}}` JSON objects. Returns
/// `(name, args)` pairs (possibly several); empty if nothing parseable is found.
fn recover_calls(text: &str) -> Vec<(String, serde_json::Value)> {
    let mut out = Vec::new();
    // Shape A: <function=NAME ...>{ ...json... } , one or more times.
    let mut hay = text;
    while let Some(pos) = hay.find("<function") {
        let after = hay[pos + "<function".len()..].trim_start_matches(['=', ':', ' ']);
        let name_end = after
            .find(|c: char| c == '>' || c == '{' || c == '(' || c.is_whitespace())
            .unwrap_or(after.len());
        let name = after[..name_end].trim().trim_matches('"').to_string();
        let tail = &after[name_end..];
        if let Some(brace) = tail.find('{') {
            if let Some((obj, consumed)) = take_json_object(&tail[brace..]) {
                if !name.is_empty() {
                    out.push((name, obj));
                }
                hay = &tail[brace + consumed..];
                continue;
            }
        }
        hay = tail;
    }
    if !out.is_empty() {
        return out;
    }
    // Shape B: one or more {"name": "...", "arguments"/"parameters": {...}} objects.
    let mut rest = text;
    while let Some(brace) = rest.find('{') {
        let Some((obj, consumed)) = take_json_object(&rest[brace..]) else { break };
        if let Some(name) = obj.get("name").and_then(|n| n.as_str()) {
            let args = match obj.get("arguments").or_else(|| obj.get("parameters")) {
                Some(serde_json::Value::String(s)) => {
                    serde_json::from_str(s).unwrap_or_else(|_| serde_json::json!({}))
                }
                Some(v) => v.clone(),
                None => serde_json::json!({}),
            };
            out.push((name.to_string(), args));
        }
        rest = &rest[brace + consumed..];
    }
    out
}

/// From a string STARTING with `{`, return the parsed JSON object and the byte length
/// of its `{...}` span, scanning balanced braces while respecting string literals and
/// escapes. `None` if it doesn't begin with a parseable object. (Brace/quote bytes are
/// ASCII, so byte-indexed slicing stays on char boundaries even with UTF-8 content.)
fn take_json_object(s: &str) -> Option<(serde_json::Value, usize)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'{') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            match b {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    let val = serde_json::from_str(&s[..=i]).ok()?;
                    return Some((val, i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

/// Split a (possibly mangled) tool name + arguments string into a clean
/// `(name, args)`. Well-behaved models return `name = "open"` and the JSON in
/// `arguments`, which passes straight through. Some models (seen on Groq's llama-3.3)
/// instead emit the whole call in the name field — `open {"target":"gmail.com"}` —
/// with empty `arguments`; here we split at the first `{`, take the leading token as
/// the action name, and parse the trailing JSON as the args. Falls back to the first
/// whitespace token + the `arguments` JSON when there's no embedded object.
fn normalize_call(name_raw: &str, args_str: &str) -> (String, serde_json::Value) {
    let name_raw = name_raw.trim();
    if let Some(brace) = name_raw.find('{') {
        let (head, rest) = name_raw.split_at(brace);
        let head = head.trim();
        if !head.is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest.trim()) {
                return (head.to_string(), v);
            }
        }
    }
    let name = name_raw.split_whitespace().next().unwrap_or(name_raw).to_string();
    let args = serde_json::from_str(args_str).unwrap_or(serde_json::json!({}));
    (name, args)
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
    /// `:ai [prompt]` — the AI is a single, persistent **background** tab. `:ai` with
    /// no prompt brings that tab into view (creating it the first time) and drops into
    /// its input field. `:ai <prompt>` runs the prompt in the background WITHOUT
    /// switching to the AI tab — you stay where you are while it works (and it acts on
    /// your current view); the reply/actions land in the chat, viewable later with
    /// `:ai`. With no key saved yet we must surface the tab so the key can be pasted.
    pub(crate) fn open_ai_tab(&mut self, prompt: &str) {
        let prompt = prompt.trim();
        let has_key = self.groq_key.is_some();

        if prompt.is_empty() {
            // Remember where we were so closing the AI tab returns here (or the welcome
            // screen), not a stray blank pane — but don't overwrite it if we're already
            // looking at the AI tab.
            if !self.active_is_ai() {
                self.ai_prev_active = self.active;
            }
            // Summon the AI tab into view (create + show if it doesn't exist yet). It
            // opens as a new tab and we move to it.
            match self.tabs.iter().position(|t| t.ai().is_some()) {
                Some(idx) => self.show_tab(idx),
                None => {
                    let tab = self.new_ai_tab(None);
                    self.place_tab(tab, true);
                }
            }
            self.clear_status();
            self.enter_ai_insert();
            self.window.request_redraw();
            return;
        }

        // `:ai <prompt>`: ensure the singleton exists, then run in the background.
        let id = match self.tabs.iter().find_map(|t| t.ai().map(|a| a.id)) {
            Some(id) => id,
            None => {
                // Create it WITHOUT activating (stay on the current tab). With no key
                // we stash the prompt to run once the key is pasted.
                let stash = (!has_key).then(|| prompt.to_string());
                let tab = self.new_ai_tab(stash);
                self.tabs.push(tab);
                self.refresh_visibility();
                self.tabs.iter().find_map(|t| t.ai().map(|a| a.id)).unwrap()
            }
        };
        if has_key {
            self.ask_ai(id, prompt.to_string());
            self.set_status("ai is working… (:ai to watch)");
        } else {
            // Can't run headless without a key — bring the tab up to paste it.
            if let Some(ai) = self.ai_by_id_mut(id) {
                ai.pending_prompt = Some(prompt.to_string());
            }
            if let Some(idx) = self.tabs.iter().position(|t| t.ai().is_some()) {
                self.show_tab(idx);
            }
            self.enter_ai_insert();
            self.set_status("paste your Groq key, then it'll run that");
        }
        self.window.request_redraw();
    }

    /// Build a fresh AI tab (allocating its stable id), optionally pre-stashing a
    /// prompt to run once a key is entered.
    fn new_ai_tab(&mut self, pending_prompt: Option<String>) -> Tab {
        let id = self.next_ai_id;
        self.next_ai_id += 1;
        Tab {
            url: "browser://ai".into(),
            nojs: false,
            read: false,
            research: false,
            content: TabContent::Ai(AiState::new(id, pending_prompt)),
        }
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
        let Some(id) = self.active_ai_mut().map(|ai| ai.id) else { return };
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
                self.ask_ai(id, p);
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
            self.ask_ai(id, prompt);
        }
    }

    /// Start a turn for the AI tab `id` (which need NOT be active — it can run in the
    /// background): record the user `prompt`, mark the tab pending, and fire the first
    /// Groq round. The reply returns via [`UserEvent::AiReply`], which drives the
    /// agentic loop in [`ai_reply`](App::ai_reply).
    pub(crate) fn ask_ai(&mut self, id: u64, prompt: String) {
        if self.groq_key.is_none() {
            return;
        }
        let model = self.ai_model.clone();
        let convo = {
            let Some(ai) = self.ai_by_id_mut(id) else { return };
            if ai.pending {
                // A turn is already running for this tab; don't pile on.
                return;
            }
            ai.messages.push(AiMessage { role: AiRole::You, text: prompt });
            ai.pending = true;
            ai.follow = true;
            build_convo(&ai.messages)
        };
        // Save the conversation (with the new question) right away, so it survives
        // even if the reply never arrives.
        self.commit_ai(id);
        self.spawn_ai_round(id, model, convo, 0);
        self.window.request_redraw();
    }

    /// Fire one Groq round on a background thread; its result returns as
    /// [`UserEvent::AiReply`] carrying `convo`/`round` so the loop can continue.
    fn spawn_ai_round(&self, id: u64, model: String, convo: Vec<serde_json::Value>, round: u32) {
        let Some(key) = self.groq_key.clone() else { return };
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = ask_raw(&key, &model, &convo);
            let _ = proxy.send_event(UserEvent::AiReply { id, convo, round, result });
        });
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

    /// One step of the agentic loop for the AI tab `id`. `convo` is the OpenAI-format
    /// message list that was sent this round; `round` counts tool rounds so far. A
    /// text answer is appended and finishes the turn. Tool-calls are each run through
    /// [`perform`](App::perform) (their result shown in the chat); then the assistant
    /// message and the tool results are appended to `convo` and the NEXT round fires,
    /// so the model can continue the task — until it returns text or
    /// [`MAX_TOOL_ROUNDS`] is hit. Tools run BEFORE the tab is re-borrowed so
    /// `perform` has clean access to the whole `App`.
    pub(crate) fn ai_reply(
        &mut self,
        id: u64,
        convo: Vec<serde_json::Value>,
        round: u32,
        result: Result<AiStep, String>,
    ) {
        let (calls, assistant_msg) = match result {
            Ok(AiStep::Done(text)) => return self.ai_finish(id, Some(AiMessage { role: AiRole::Ai, text })),
            Err(e) => return self.ai_finish(id, Some(AiMessage { role: AiRole::Err, text: e })),
            Ok(AiStep::Calls { calls, assistant_msg }) => (calls, assistant_msg),
        };
        // The AI tab must never be the target of a browser action (open/split would
        // clobber the chat); move to a content tab/pane first.
        self.ensure_content_focus();
        // Mark this tab as the actor so an async action (a data wipe) routes its
        // "Done — …" confirmation back to this chat.
        self.acting_ai = Some(id);
        let mut visible = Vec::new();
        let mut tool_msgs = Vec::new();
        for c in &calls {
            let (text, role) = match self.perform(&c.name, &c.args) {
                Ok(msg) => (msg, AiRole::Ai),
                Err(e) => (e, AiRole::Err),
            };
            visible.push(AiMessage { role, text: text.clone() });
            tool_msgs.push(serde_json::json!({ "role": "tool", "tool_call_id": c.id, "content": text }));
        }
        self.acting_ai = None;
        if let Some(ai) = self.ai_by_id_mut(id) {
            ai.messages.extend(visible);
            ai.follow = true;
        }
        self.commit_ai(id);
        // Continue the loop unless we've hit the safety cap.
        if round + 1 >= MAX_TOOL_ROUNDS {
            return self.ai_finish(
                id,
                Some(AiMessage { role: AiRole::Err, text: "stopped after too many steps".into() }),
            );
        }
        let mut next = convo;
        next.push(assistant_msg);
        next.extend(tool_msgs);
        let model = self.ai_model.clone();
        self.spawn_ai_round(id, model, next, round + 1);
        self.window.request_redraw();
    }

    /// End an AI turn: clear `pending`, append an optional final message, persist. When
    /// the turn ran in the BACKGROUND (the AI tab isn't on screen — the `:ai <prompt>`
    /// case), surface its outcome on the status bar: the success summary flashes for a
    /// couple of seconds, an error shows a brief pointer to the `:ai` tab. While the
    /// user is watching the AI tab we stay quiet — they already see it in the chat.
    fn ai_finish(&mut self, id: u64, msg: Option<AiMessage>) {
        let watching = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.ai())
            .is_some_and(|a| a.id == id);
        if let Some(ai) = self.ai_by_id_mut(id) {
            ai.pending = false;
            if let Some(m) = &msg {
                ai.messages.push(m.clone());
            }
            ai.follow = true;
        }
        if !watching {
            match msg {
                Some(m) if m.role == AiRole::Err => {
                    self.warn_status("the ai hit an error — see the :ai tab")
                }
                Some(m) if m.role == AiRole::Ai => self.flash_status(m.text),
                _ => {}
            }
        }
        self.commit_ai(id);
        self.window.request_redraw();
    }

    /// Mutable access to the AI tab with this id (it may not be the active tab — the
    /// AI tab runs in the background).
    pub(crate) fn ai_by_id_mut(&mut self, id: u64) -> Option<&mut AiState> {
        self.tabs.iter_mut().find_map(|t| t.ai_mut().filter(|a| a.id == id))
    }

    /// If the active tab is the AI tab, move focus to a fresh blank content tab so a
    /// browser action lands on real content (never clobbering the chat or an existing
    /// page). A no-op when the user is already on a content tab/pane — the common
    /// case for a background `:ai <prompt>`.
    fn ensure_content_focus(&mut self) {
        if !self.active_is_ai() {
            return;
        }
        match self.tabs.iter().position(|t| t.is_blank()) {
            Some(idx) => self.show_tab(idx),
            None => {
                self.tabs.push(Tab::blank());
                let last = self.tabs.len() - 1;
                self.show_tab(last);
            }
        }
    }

    /// An async browser action initiated from the `:ai` tab `id` finished (e.g. an
    /// engine data wipe): append a natural-language confirmation to that chat and
    /// re-persist it. Returns whether that tab is the one currently on screen — the
    /// caller skips the status bar in that case, since the chat already shows it.
    pub(crate) fn ai_action_done(&mut self, id: u64, label: &str) -> bool {
        let active =
            self.active.and_then(|i| self.tabs.get(i)).and_then(|t| t.ai()).is_some_and(|a| a.id == id);
        if let Some(ai) = self.tabs.iter_mut().find_map(|t| t.ai_mut().filter(|a| a.id == id)) {
            ai.messages.push(AiMessage { role: AiRole::Ai, text: format!("Done — cleared {label}.") });
            ai.follow = true;
        }
        self.commit_ai(id);
        self.window.request_redraw();
        active
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
    fn normalize_call_recovers_args_crammed_into_the_name() {
        // Well-behaved: name is clean, args come from the arguments string.
        let (n, a) = normalize_call("open", r#"{"target":"gmail.com","where":"new"}"#);
        assert_eq!(n, "open");
        assert_eq!(a["target"], "gmail.com");
        assert_eq!(a["where"], "new");

        // Mangled: the model put the JSON in the name with empty arguments.
        let (n, a) = normalize_call(r#"open {"target": "gmail.com", "where": "current"}"#, "{}");
        assert_eq!(n, "open");
        assert_eq!(a["target"], "gmail.com");
        assert_eq!(a["where"], "current");

        // No args at all: name only.
        let (n, a) = normalize_call("restore", "{}");
        assert_eq!(n, "restore");
        assert_eq!(a, serde_json::json!({}));
    }

    #[test]
    fn take_json_object_scans_balanced_braces_with_strings() {
        let (v, n) = take_json_object(r#"{"a":{"b":"}"},"c":1} trailing"#).unwrap();
        assert_eq!(v["a"]["b"], "}"); // the brace inside the string isn't a close
        assert_eq!(v["c"], 1);
        assert_eq!(&r#"{"a":{"b":"}"},"c":1} trailing"#[..n], r#"{"a":{"b":"}"},"c":1}"#);
        assert!(take_json_object("not an object").is_none());
    }

    #[test]
    fn recover_calls_parses_llama_function_syntax() {
        // The `<function=NAME>{json}</function>` shape llama emits on a tool_use_failed.
        let fg = r#"<function=open>{"target": "gmail.com", "where": "current"}</function>"#;
        let calls = recover_calls(fg);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "open");
        assert_eq!(calls[0].1["target"], "gmail.com");

        // Two calls (compound prompt) recovered in order.
        let fg = "<function=open>{\"target\":\"gmail.com\"}</function>\
                  <function=open>{\"target\":\"youtube.com\",\"where\":\"new\"}</function>";
        let calls = recover_calls(fg);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].1["where"], "new");
    }

    #[test]
    fn recover_calls_parses_bare_name_arguments_json() {
        let fg = r#"{"name": "clear", "arguments": {"what": "cache", "period": "15m"}}"#;
        let calls = recover_calls(fg);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "clear");
        assert_eq!(calls[0].1["period"], "15m");

        // `arguments` itself may be a JSON-encoded string.
        let fg = r#"{"name":"open","arguments":"{\"target\":\"github.com\"}"}"#;
        let calls = recover_calls(fg);
        assert_eq!(calls[0].1["target"], "github.com");
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
