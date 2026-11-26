//! Native terminal: an in-process `alacritty_terminal` VT engine, rendered by our
//! own softbuffer/fontdue painter — no WebView2. The PTY still lives in the
//! `browser-pty-host` companion (ConPTY isolation); this just parses its byte
//! stream into a cell grid and paints it. Replaces the old xterm.js-in-WebView2
//! terminal, so a `:te` tab now spawns zero WebView2 processes.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::{Flags, Hyperlink};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::search::{Match, RegexIter, RegexSearch};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Color, CursorShape, CursorStyle, NamedColor, Processor, Rgb as VtRgb,
};

// Re-exported so the shell can drive vi-mode without importing alacritty directly.
pub use alacritty_terminal::vi_mode::ViMotion;

/// What one mouse press selects: a plain drag, the word under the pointer
/// (double-click), or the whole line (triple-click).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SelectKind {
    Char,
    Word,
    Line,
}

/// The mouse reporting a terminal program turned on (see [`PtyTerm::mouse_report`]).
/// Clicks are always reported once any mode is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MouseReport {
    /// Motion while a button is held (DEC 1002).
    pub drag: bool,
    /// Every motion, button or not (DEC 1003).
    pub motion: bool,
    pub encoding: MouseEncoding,
}

/// How mouse reports are written to the PTY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseEncoding {
    /// `ESC [ M` and three bytes of value + 32 (the original; coordinates stop at 223).
    X10,
    /// Like X10, but coordinates past 95 are UTF-8 encoded (DEC 1005).
    Utf8,
    /// `ESC [ < b ; x ; y M/m` (DEC 1006), what modern programs ask for.
    Sgr,
}

use crate::draw::{self, Painter, Rgb};

/// Default 16-color ANSI palette — Windows Terminal's "Campbell" scheme, so colors
/// match what the user sees elsewhere on Windows. Used when the program hasn't set
/// its own via OSC. 0–7 normal, 8–15 bright.
const ANSI: [Rgb; 16] = [
    (0x0c, 0x0c, 0x0c), // black
    (0xc5, 0x0f, 0x1f), // red
    (0x13, 0xa1, 0x0e), // green
    (0xc1, 0x9c, 0x00), // yellow
    (0x00, 0x37, 0xda), // blue
    (0x88, 0x17, 0x98), // magenta
    (0x3a, 0x96, 0xdd), // cyan
    (0xcc, 0xcc, 0xcc), // white
    (0x76, 0x76, 0x76), // bright black
    (0xe7, 0x48, 0x56), // bright red
    (0x16, 0xc6, 0x0c), // bright green
    (0xf9, 0xf1, 0xa5), // bright yellow
    (0x3b, 0x78, 0xff), // bright blue
    (0xb4, 0x00, 0x9e), // bright magenta
    (0x61, 0xd6, 0xd6), // bright cyan
    (0xf2, 0xf2, 0xf2), // bright white
];
/// Default foreground / background when the program uses the default colors. The
/// background stays the UI's dark grey (not Campbell's pure black) for cohesion.
const FG: Rgb = (0xcc, 0xcc, 0xcc);
/// Terminal background — also the content-area fill behind the grid.
pub const BG: Rgb = (0x1a, 0x1a, 0x1a);

/// The resolved terminal render style: default fg/bg plus the 16-color ANSI palette.
/// Built from the persisted [`TermConfig`](crate::config::TermConfig) by
/// [`rebuild_term_style`](crate::App::rebuild_term_style) — a named scheme first,
/// then explicit fg/bg overrides on top. Default = Campbell on the UI grey.
#[derive(Clone, Copy)]
pub struct TermStyle {
    pub fg: Rgb,
    pub bg: Rgb,
    pub ansi: [Rgb; 16],
}

impl Default for TermStyle {
    fn default() -> Self {
        TermStyle {
            fg: FG,
            bg: BG,
            ansi: ANSI,
        }
    }
}

/// Scheme names accepted by [`scheme`], in the order shown to the user/AI.
pub const SCHEMES: &[&str] = &[
    "campbell",
    "dracula",
    "gruvbox",
    "nord",
    "solarized",
    "onedark",
    "monokai",
];

/// Look up a built-in colour scheme by name (case-insensitive). Palettes are the
/// widely-published values for each theme; `campbell` is the built-in default.
pub fn scheme(name: &str) -> Option<TermStyle> {
    // (fg, bg, [16 ANSI colors 0..=7 normal, 8..=15 bright])
    let hex = |n: u32| -> Rgb {
        (
            ((n >> 16) & 0xff) as u8,
            ((n >> 8) & 0xff) as u8,
            (n & 0xff) as u8,
        )
    };
    let build = |fg: u32, bg: u32, pal: [u32; 16]| -> TermStyle {
        TermStyle {
            fg: hex(fg),
            bg: hex(bg),
            ansi: pal.map(hex),
        }
    };
    Some(match name.to_ascii_lowercase().as_str() {
        "campbell" | "default" => TermStyle::default(),
        "dracula" => build(
            0xf8f8f2,
            0x282a36,
            [
                0x21222c, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9, 0xff79c6, 0x8be9fd, 0xf8f8f2,
                0x6272a4, 0xff6e6e, 0x69ff94, 0xffffa5, 0xd6acff, 0xff92df, 0xa4ffff, 0xffffff,
            ],
        ),
        "gruvbox" => build(
            0xebdbb2,
            0x282828,
            [
                0x282828, 0xcc241d, 0x98971a, 0xd79921, 0x458588, 0xb16286, 0x689d6a, 0xa89984,
                0x928374, 0xfb4934, 0xb8bb26, 0xfabd2f, 0x83a598, 0xd3869b, 0x8ec07c, 0xebdbb2,
            ],
        ),
        "nord" => build(
            0xd8dee9,
            0x2e3440,
            [
                0x3b4252, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x88c0d0, 0xe5e9f0,
                0x4c566a, 0xbf616a, 0xa3be8c, 0xebcb8b, 0x81a1c1, 0xb48ead, 0x8fbcbb, 0xeceff4,
            ],
        ),
        "solarized" => build(
            0x839496,
            0x002b36,
            [
                0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                0x002b36, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
            ],
        ),
        "onedark" => build(
            0xabb2bf,
            0x282c34,
            [
                0x282c34, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xabb2bf,
                0x5c6370, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xffffff,
            ],
        ),
        "monokai" => build(
            0xf8f8f2,
            0x272822,
            [
                0x272822, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf8f8f2,
                0x75715e, 0xf92672, 0xa6e22e, 0xf4bf75, 0x66d9ef, 0xae81ff, 0xa1efe4, 0xf9f8f5,
            ],
        ),
        _ => return None,
    })
}

/// A reply the engine owes the program, in the order it was asked. Most are ready
/// bytes; colour and size queries need the renderer's palette and cell size, which
/// the engine doesn't know, so they're answered when the app drains them.
enum Reply {
    Bytes(String),
    Color(usize, Arc<dyn Fn(VtRgb) -> String + Sync + Send>),
    TextAreaSize(Arc<dyn Fn(WindowSize) -> String + Sync + Send>),
}

/// What the engine reported since the app last looked.
#[derive(Default)]
struct Reported {
    replies: Vec<Reply>,
    /// The last text a program copied (OSC 52).
    clipboard: Option<String>,
    /// A program rang the bell.
    bell: bool,
}

/// Event sink for the VT engine. It queues what the app must act on: replies to
/// device queries (notably the `ESC[6n` cursor-position report the shell sends on
/// startup and STALLS waiting for, and the colour queries programs use to pick a
/// light or dark theme), clipboard copies, the bell, and the title. The app drains
/// them after each feed.
#[derive(Clone)]
pub struct TermListener {
    reported: Arc<Mutex<Reported>>,
    /// The terminal's current OSC title (set by the running program: e.g. vim sets
    /// it to the open file, Claude Code to "Claude Code"). `None` once reset/unset.
    title: Arc<Mutex<Option<String>>>,
}

impl EventListener for TermListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => {
                self.reported
                    .lock()
                    .unwrap()
                    .replies
                    .push(Reply::Bytes(text));
            }
            Event::ColorRequest(index, format) => {
                let reply = Reply::Color(index, format);
                self.reported.lock().unwrap().replies.push(reply);
            }
            Event::TextAreaSizeRequest(format) => {
                let reply = Reply::TextAreaSize(format);
                self.reported.lock().unwrap().replies.push(reply);
            }
            // Windows has one clipboard, so a copy to the X "selection" lands there too.
            Event::ClipboardStore(_, text) => {
                self.reported.lock().unwrap().clipboard = Some(text);
            }
            Event::Bell => self.reported.lock().unwrap().bell = true,
            Event::Title(t) => {
                let t = t.trim();
                *self.title.lock().unwrap() = (!t.is_empty()).then(|| t.to_string());
            }
            Event::ResetTitle => *self.title.lock().unwrap() = None,
            _ => {}
        }
    }
}

/// One native terminal: the VT engine + parser + its current grid size. The PTY
/// process handles live alongside this in the shell's `TermSession`.
pub struct PtyTerm {
    pub vt: Term<TermListener>,
    parser: Processor,
    pub cols: usize,
    pub rows: usize,
    reported: Arc<Mutex<Reported>>,
    title: Arc<Mutex<Option<String>>>,
    /// Incremental scanner for the shell-integration cwd escapes (see [`CwdScan`]).
    cwd_scan: CwdScan,
    /// The shell's current working directory, as last reported over OSC 9;9 / OSC 7.
    /// A Windows path (`C:\…`, `\\wsl.localhost\…`) or a Linux one (`/home/…` from
    /// inside WSL). `None` until a shell-integration prompt first reports it.
    cwd: Option<String>,
    /// The engine's options, kept so one can change (`set_options` takes them all).
    config: Config,
    /// The live `/` search over the scrollback, if any (see [`PtyTerm::search_set`]).
    search: Option<TermSearch>,
    /// Shell integration: where the shell is in its prompt/command cycle, and what's
    /// known about each prompt (see [`Mark`]).
    shell: ShellState,
    /// The link under the mouse while Ctrl is held: drawn underlined.
    pub link_hover: Option<(Point, Point)>,
}

/// A `/` search over a terminal's whole buffer, scrollback included.
struct TermSearch {
    regex: RegexSearch,
    /// Every match, oldest (top) first.
    matches: Vec<Match>,
    /// The current one, an index into `matches`.
    current: usize,
    /// Output arrived since the matches were found (they've moved up the buffer).
    stale: bool,
}

/// The most matches a search keeps: searching for one letter in a full scrollback
/// shouldn't build a list the size of the buffer.
const SEARCH_CAP: usize = 10_000;

/// A shell-integration mark (OSC 133, the FinalTerm convention Windows Terminal, VS
/// Code and kitty read; VS Code's OSC 633 uses the same letters): the shell prints
/// one at each step of its prompt/command cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mark {
    /// `A`: a prompt starts.
    PromptStart,
    /// `B`: the prompt ends; what's typed next is the command.
    InputStart,
    /// `C`: the command was entered and runs; its output follows.
    OutputStart,
    /// `D[;code]`: the command finished (with its exit code, when the shell says).
    Finished(Option<i32>),
}

/// The mark an OSC payload carries, if it's one (`133;A`, `633;D;1`, …).
fn osc_mark(payload: &[u8]) -> Option<Mark> {
    let s = std::str::from_utf8(payload).ok()?;
    let rest = s.strip_prefix("133;").or_else(|| s.strip_prefix("633;"))?;
    let mut parts = rest.split(';');
    Some(match parts.next()? {
        "A" => Mark::PromptStart,
        "B" => Mark::InputStart,
        "C" => Mark::OutputStart,
        "D" => Mark::Finished(parts.next().and_then(|c| c.trim().parse().ok())),
        _ => return None,
    })
}

/// The hyperlink scheme shell-integration tags carry. Prompt cells get
/// `browser-prompt:<id>`, a command's typed text `browser-cmd:<id>`. Tags live on the
/// cells themselves (the grid's own OSC 8 slot), so they move with their text through
/// scrolling, scrollback trimming and reflow, which no stored line number could.
/// They're never shown or opened as links.
const PROMPT_TAG: &str = "browser-prompt:";
const CMD_TAG: &str = "browser-cmd:";

/// Whether a hyperlink is a shell-integration tag rather than a real link.
pub fn is_shell_tag(uri: &str) -> bool {
    uri.starts_with(PROMPT_TAG) || uri.starts_with(CMD_TAG)
}

/// The prompt id in a tag's URI.
fn tag_id(uri: &str) -> Option<u64> {
    uri.strip_prefix(PROMPT_TAG)
        .or_else(|| uri.strip_prefix(CMD_TAG))?
        .parse()
        .ok()
}

/// What's known about one prompt and the command typed at it.
#[derive(Clone, Copy, Debug, Default)]
struct PromptInfo {
    /// When the command started (a `C` mark, or Enter at the prompt).
    started: Option<std::time::Instant>,
    /// The exit code, once it finished (`None` inside: the shell didn't say).
    finished: Option<Option<i32>>,
}

/// Where the shell is in its cycle.
#[derive(Clone, Copy, Debug, Default)]
enum Phase {
    #[default]
    Idle,
    /// Between `A` and `B`: the prompt is being drawn from `start`.
    Prompt { start: Point },
    /// Between `B` and the command starting: typing at the prompt, from `start`.
    Input { start: Point },
    /// The command runs.
    Running,
}

/// Shell-integration state of one terminal.
#[derive(Default)]
struct ShellState {
    /// The newest prompt's id (ids count up from 1).
    current: u64,
    phase: Phase,
    /// The most recent prompts, by id.
    prompts: std::collections::BTreeMap<u64, PromptInfo>,
    /// Commands that finished since the app last looked.
    done: Vec<CommandDone>,
    /// Whether this shell has sent any mark (so features can tell it's integrated).
    seen: bool,
}

/// The most prompts remembered (their tags stay on screen; only the exit code and
/// timing of older ones is forgotten).
const PROMPTS_KEPT: usize = 4096;

/// A command that finished: how long it ran and how it ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommandDone {
    pub duration: std::time::Duration,
    /// `Some(0)` success, `Some(n)` failure, `None` the shell didn't say.
    pub exit: Option<i32>,
}

/// One command's output, for `Y` in copy mode.
#[derive(Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// What was typed at the prompt (empty when the shell doesn't mark it).
    pub command: String,
    pub output: String,
}

/// A link on screen: from `start` to `end` (inclusive, absolute grid points).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TermLink {
    pub start: Point,
    pub end: Point,
    pub url: String,
}

/// Web addresses as they appear in program output (the same idea as Alacritty's URL
/// hint): a scheme or `www.`, then everything up to whitespace or a character URLs
/// don't contain. Local `file://` links are left out: opening one in a web tab isn't
/// supported.
const URL_REGEX: &str = r#"(?:https?://|www\.)[^\x00-\x1F\x7F-\x9F\s<>"'`{}|\\^⟨⟩]+"#;

/// Trim what a sentence wraps around a URL: trailing punctuation, and a closing
/// bracket that has no opening one inside the URL ("(see https://x.org/a)").
fn trim_url(url: &str) -> &str {
    let mut url = url;
    loop {
        let before = url;
        url = url.trim_end_matches(['.', ',', ';', ':', '!', '?', '\'', '"']);
        for (open, close) in [('(', ')'), ('[', ']')] {
            if url.ends_with(close) && url.matches(open).count() < url.matches(close).count() {
                url = &url[..url.len() - 1];
            }
        }
        if url == before {
            return url;
        }
    }
}

/// A link the browser can open: a web address (a bare `www.` gets `https://`).
/// Other schemes (`file:`, `mailto:`, `ssh:`…) come back `None`.
pub fn openable_url(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        Some(url.to_string())
    } else if lower.starts_with("www.") {
        Some(format!("https://{url}"))
    } else {
        None
    }
}

/// `text` as a regex that matches it literally, ignoring case (like the browser's
/// find everywhere else).
fn literal_regex(text: &str) -> String {
    let mut out = String::from("(?i)");
    for c in text.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Incremental scanner for the two cwd escape sequences shells emit each prompt
/// when shell integration is on: `OSC 9;9;<path> ST/BEL` (the ConEmu / Windows
/// Terminal convention — cmd gets it injected automatically via `PROMPT`, pwsh
/// via a one-line `$PROFILE` hook) and `OSC 7;file://<host><path> ST/BEL` (the
/// Unix convention — bash/zsh inside WSL). Alacritty's VT engine drops OSCs it
/// doesn't know, so these are scanned from the RAW byte stream — statefully,
/// because a sequence can split across PTY read chunks.
#[derive(Default)]
struct CwdScan {
    /// 0 = ground, 1 = saw ESC, 2 = inside OSC, 3 = saw ESC inside OSC (ST?).
    state: u8,
    buf: Vec<u8>,
}

/// Longest OSC payload kept; anything bigger (e.g. a base64 clipboard write) is
/// discarded rather than buffered.
const OSC_CAP: usize = 4096;

impl CwdScan {
    /// Advance by one raw byte; returns a completed OSC payload when one ends.
    fn push(&mut self, b: u8) -> Option<Vec<u8>> {
        match self.state {
            0 => {
                if b == 0x1b {
                    self.state = 1;
                }
            }
            1 => {
                if b == b']' {
                    self.state = 2;
                    self.buf.clear();
                } else {
                    self.state = if b == 0x1b { 1 } else { 0 };
                }
            }
            2 => match b {
                0x07 => {
                    self.state = 0;
                    return Some(std::mem::take(&mut self.buf));
                }
                0x1b => self.state = 3,
                _ => {
                    if self.buf.len() < OSC_CAP {
                        self.buf.push(b);
                    }
                }
            },
            _ => {
                self.state = if b == 0x1b { 1 } else { 0 };
                if b == b'\\' {
                    return Some(std::mem::take(&mut self.buf));
                }
                self.buf.clear();
            }
        }
        None
    }
}

/// Decode `%XX` percent-escapes (an OSC 7 `file://` URI, a URL `#fragment`).
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // Parsed from the bytes: slicing the str would panic on a `%` followed by a
        // multi-byte character.
        let decoded = (bytes[i] == b'%' && i + 2 < bytes.len())
            .then(|| std::str::from_utf8(&bytes[i + 1..i + 3]).ok())
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match decoded {
            Some(b) => {
                out.push(b);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The cwd carried by a completed OSC payload, if it is one of the two cwd
/// sequences. `9;9;<path>` → the path verbatim (surrounding quotes stripped —
/// some prompts emit `"%cd%"` quoted); `7;file://<host><path>` → `C:\…` for a
/// Windows `file:///C:/…` URI, else the percent-decoded Linux path.
fn osc_cwd(payload: &[u8]) -> Option<String> {
    let s = String::from_utf8_lossy(payload);
    if let Some(path) = s.strip_prefix("9;9;") {
        let path = path.trim().trim_matches('"');
        return (!path.is_empty()).then(|| path.to_string());
    }
    let uri = s.strip_prefix("7;")?;
    let rest = uri.strip_prefix("file://")?;
    let path = percent_decode(&rest[rest.find('/').unwrap_or(rest.len())..]);
    if path.is_empty() || path == "/" {
        return None;
    }
    // `/C:/Users/x` → `C:\Users\x`; anything else is a Linux path, kept as-is.
    let b = path.as_bytes();
    if b.len() >= 3 && b[2] == b':' && b[1].is_ascii_alphabetic() {
        return Some(path[1..].replace('/', "\\"));
    }
    Some(path)
}

/// Default scrollback kept per terminal, in lines. Each kept line costs grid
/// memory for the terminal's lifetime, so heavy multi-terminal users can lower it.
pub const DEFAULT_SCROLLBACK: usize = 5000;

impl PtyTerm {
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        let reported = Arc::new(Mutex::new(Reported::default()));
        let title = Arc::new(Mutex::new(None));
        let listener = TermListener {
            reported: reported.clone(),
            title: title.clone(),
        };
        // Keep scrollback so copy-mode (Shift+Esc) can page back through history.
        let config = Config {
            scrolling_history: scrollback,
            // Copy mode's cursor is a steady block whatever the program set.
            vi_mode_cursor_style: Some(CursorStyle {
                shape: CursorShape::Block,
                blinking: false,
            }),
            ..Config::default()
        };
        let vt = Term::new(config.clone(), &TermSize::new(cols, rows), listener);
        PtyTerm {
            vt,
            parser: Processor::new(),
            cols,
            rows,
            reported,
            title,
            cwd_scan: CwdScan::default(),
            cwd: None,
            config,
            search: None,
            link_hover: None,
            shell: ShellState::default(),
        }
    }

    /// Whether the cursor blinks when no program has said (`term.cursor_blink`). A
    /// program can still ask for a steady or a blinking cursor (DECSCUSR).
    pub fn set_cursor_blink(&mut self, on: bool) {
        if self.config.default_cursor_style.blinking != on {
            self.config.default_cursor_style.blinking = on;
            self.vt.set_options(self.config.clone());
        }
    }

    /// Whether the cursor blinks now: the program's choice, else the default above.
    pub fn cursor_blinking(&self) -> bool {
        self.vt.cursor_style().blinking
    }

    /// The shell's current working directory, as last reported by shell
    /// integration (OSC 9;9 / OSC 7). `None` when no prompt has reported one.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    /// The terminal's current OSC title, if the running program set one.
    pub fn title(&self) -> Option<String> {
        self.title.lock().unwrap().clone()
    }

    // --- copy / vi mode (Alacritty's own) -------------------------------------
    // Shift+Esc toggles the engine into vi mode: a vi cursor moves over the LIVE
    // colored grid (no text snapshot), `v`/`V` start a grid selection that motions
    // extend, and `selection_to_string` yanks the real text. This is how Alacritty/
    // WezTerm do copy-mode.

    pub fn is_vi(&self) -> bool {
        self.vt.mode().contains(TermMode::VI)
    }
    pub fn toggle_vi(&mut self) {
        self.vt.toggle_vi_mode();
    }

    /// Extend the active selection to the vi cursor. Alacritty's own recompute only
    /// extends a NON-empty selection, so a just-started `v` (zero-width) would never
    /// grow — force it so the very first motion already extends.
    fn force_extend(&mut self) {
        let p = self.vt.vi_mode_cursor.point;
        if let Some(sel) = self.vt.selection.as_mut() {
            sel.update(p, Side::Left);
            sel.include_all();
        }
    }

    pub fn vi_motion(&mut self, m: ViMotion) {
        self.vt.vi_motion(m);
        // Keep the viewport on the cursor (alacritty's vi_motion moves the cursor
        // into scrollback but does NOT scroll the display itself).
        let p = self.vt.vi_mode_cursor.point;
        self.vt.scroll_to_point(p);
        self.force_extend();
    }

    /// Move the vi cursor (and viewport) to a line, clamped to the buffer.
    fn vi_goto_line(&mut self, line: i32, column: Column) {
        let lo = self.vt.topmost_line().0;
        let hi = self.vt.bottommost_line().0;
        let p = Point::new(Line(line.clamp(lo, hi)), column);
        self.vt.vi_goto_point(p);
        self.force_extend();
    }
    pub fn vi_top(&mut self) {
        let line = self.vt.topmost_line().0;
        self.vi_goto_line(line, Column(0));
    }
    pub fn vi_bottom(&mut self) {
        let line = self.vt.bottommost_line().0;
        self.vi_goto_line(line, Column(0));
    }
    /// Scroll the vi cursor by `lines` (negative = up), keeping it in view.
    pub fn vi_scroll(&mut self, lines: i32) {
        let cur = self.vt.vi_mode_cursor.point;
        self.vi_goto_line(cur.line.0 + lines, cur.column);
    }

    /// Vi find-char on the current line (`f`/`F`/`t`/`T`): move the vi cursor to the
    /// next/previous occurrence of `target`. `forward` = search rightward (`f`/`t`);
    /// `till` = stop one cell short of the match (`t`/`T`). A no-op if the char isn't
    /// found on the line, so an unmatched find leaves the cursor put (like vim).
    pub fn vi_find_char(&mut self, target: char, forward: bool, till: bool) {
        let point = self.vt.vi_mode_cursor.point;
        let line = point.line;
        let start = point.column.0;
        let cols = self.cols;
        let grid = self.vt.grid();
        let hit = |c: usize| grid[Point::new(line, Column(c))].c == target;
        let found = if forward {
            (start + 1..cols).find(|&c| hit(c))
        } else {
            (0..start).rev().find(|&c| hit(c))
        };
        if let Some(mut col) = found {
            // `t`/`T` land just before the target (toward the cursor's side).
            if till {
                col = if forward {
                    col.saturating_sub(1)
                } else {
                    col + 1
                };
            }
            self.vt.vi_goto_point(Point::new(line, Column(col)));
            self.force_extend();
        }
    }

    /// Scroll the viewport through scrollback by `lines` (positive = up, toward
    /// older output) without entering vi mode — for mouse-wheel scrolling a live
    /// shell. New PTY output snaps the view back to the bottom, as terminals do.
    pub fn scroll_display(&mut self, lines: i32) {
        self.vt.grid_mut().scroll_display(Scroll::Delta(lines));
    }

    /// Snap the viewport back to the live (bottom) line. Copy/vi-mode motions park
    /// the display in scrollback and NOTHING else resets it on exit — Alacritty's
    /// `toggle_vi_mode` only moves the cursor, and quiet shells produce no output
    /// to snap the view — so resuming the shell calls this (the "camera stuck
    /// after Esc" bug).
    pub fn scroll_to_bottom(&mut self) {
        self.vt.grid_mut().scroll_display(Scroll::Bottom);
    }

    /// Whether the viewport is showing scrollback rather than the live screen.
    pub fn scrolled_back(&self) -> bool {
        self.vt.grid().display_offset() > 0
    }

    /// Start a selection at the vi cursor (`lines` = linewise `V` vs charwise `v`).
    pub fn start_selection(&mut self, lines: bool) {
        let ty = if lines {
            SelectionType::Lines
        } else {
            SelectionType::Simple
        };
        let p = self.vt.vi_mode_cursor.point;
        self.vt.selection = Some(Selection::new(ty, p, Side::Left));
    }
    /// Clear any active selection; returns whether there was one.
    pub fn clear_selection(&mut self) -> bool {
        self.vt.selection.take().is_some()
    }

    // --- mouse selection ------------------------------------------------------
    // Drag-select with the pointer, independent of vi/copy mode. Cells arrive as
    // VIEWPORT coordinates (row 0 = the pane's top line, what the user sees), which
    // scrollback makes different from the grid's absolute lines — see `to_absolute`.

    /// Turn a visible (col, row) cell into the absolute buffer point the selection
    /// and grid index use: scrolling back moves the viewport up over history, whose
    /// lines are negative. The inverse of the `+ display_offset` the renderer does.
    fn to_absolute(&self, col: usize, row: usize) -> Point {
        let off = self.vt.grid().display_offset() as i32;
        let line = Line(row.min(self.rows.saturating_sub(1)) as i32 - off);
        Point::new(line, Column(col.min(self.cols.saturating_sub(1))))
    }

    /// Begin a mouse selection at a visible cell. `right_half` (the pointer sits past
    /// the cell's midpoint) anchors on that side, so dragging right from mid-glyph
    /// doesn't include the character you started after.
    pub fn mouse_select(&mut self, col: usize, row: usize, right_half: bool, kind: SelectKind) {
        let p = self.to_absolute(col, row);
        let side = if right_half { Side::Right } else { Side::Left };
        let ty = match kind {
            SelectKind::Char => SelectionType::Simple,
            // Word/line selections expand themselves when the range is resolved, so a
            // bare double/triple click already covers the word or row under the cursor.
            SelectKind::Word => SelectionType::Semantic,
            SelectKind::Line => SelectionType::Lines,
        };
        self.vt.selection = Some(Selection::new(ty, p, side));
        self.follow_vi_cursor(p);
    }

    /// Extend the in-progress mouse selection to a visible cell (pointer drag).
    pub fn mouse_select_to(&mut self, col: usize, row: usize, right_half: bool) {
        let p = self.to_absolute(col, row);
        let side = if right_half { Side::Right } else { Side::Left };
        if let Some(sel) = self.vt.selection.as_mut() {
            sel.update(p, side);
        }
        self.follow_vi_cursor(p);
    }

    /// Park the copy-mode cursor on the cell the mouse is working, when copy mode is
    /// on. Alacritty's vi motions extend whatever selection exists, so without this a
    /// `j` after a drag would yank the selection's end back to wherever the keyboard
    /// cursor had been left — instead, the keyboard picks up where the mouse stopped.
    fn follow_vi_cursor(&mut self, p: Point) {
        if self.is_vi() {
            self.vt.vi_goto_point(p);
        }
    }

    /// The selected text, leaving the selection in place (so a drag can be copied
    /// while staying highlighted). `None` when nothing is selected.
    pub fn selection_text(&self) -> Option<String> {
        self.vt.selection_to_string().filter(|s| !s.is_empty())
    }
    /// Yank the current selection's text (and clear it).
    pub fn yank(&mut self) -> Option<String> {
        let text = self.vt.selection_to_string();
        self.vt.selection = None;
        text
    }

    /// Feed raw PTY output bytes through the VT parser into the grid — and through
    /// the cwd scanner, since the VT engine drops the OSC 9;9 / OSC 7 sequences.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Shell-integration marks need the cursor position at the moment each one
        // arrives, so the parser runs up to every mark before it's handled.
        let mut parsed = 0;
        for (i, &b) in bytes.iter().enumerate() {
            if let Some(payload) = self.cwd_scan.push(b) {
                if let Some(dir) = osc_cwd(&payload) {
                    self.cwd = Some(dir);
                }
                if let Some(mark) = osc_mark(&payload) {
                    self.parser.advance(&mut self.vt, &bytes[parsed..=i]);
                    parsed = i + 1;
                    self.on_mark(mark);
                }
            }
        }
        self.parser.advance(&mut self.vt, &bytes[parsed..]);
        if let Some(s) = self.search.as_mut() {
            s.stale = true;
        }
    }

    /// Drain the bytes the engine wants written back to the PTY: the cursor-position
    /// report answering `ESC[6n` (call after `feed`; without this the shell hangs),
    /// and answers to colour queries (OSC 4/10/11/12, from `st` where the program set
    /// no colour of its own) and text-area size queries (`CSI 14 t`, from the
    /// `cell` size in pixels).
    pub fn take_reply(&mut self, st: &TermStyle, cell: (u16, u16)) -> Vec<u8> {
        let replies = std::mem::take(&mut self.reported.lock().unwrap().replies);
        let mut out = Vec::new();
        for reply in replies {
            let text = match reply {
                Reply::Bytes(text) => text,
                Reply::Color(index, format) => {
                    let (r, g, b) = self.query_color(index, st);
                    format(VtRgb { r, g, b })
                }
                Reply::TextAreaSize(format) => format(WindowSize {
                    num_lines: self.rows as u16,
                    num_cols: self.cols as u16,
                    cell_width: cell.0,
                    cell_height: cell.1,
                }),
            };
            out.extend_from_slice(text.as_bytes());
        }
        out
    }

    /// The colour a query for palette entry `index` gets: what the program set, else
    /// ours (0–255 the palette, then the default foreground, background, cursor).
    fn query_color(&self, index: usize, st: &TermStyle) -> Rgb {
        if let Some(c) = self.vt.colors()[index] {
            return to_rgb(c);
        }
        match index {
            0..=255 => indexed_default(index as u8, st),
            i if i == NamedColor::Background as usize => st.bg,
            _ => st.fg,
        }
    }

    /// The text a program last copied to the clipboard (OSC 52), once.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.reported.lock().unwrap().clipboard.take()
    }

    /// Whether a program rang the bell since the last call.
    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.reported.lock().unwrap().bell)
    }

    /// Resize the grid (after the window/zoom changes the cell count). Returns
    /// whether the size actually changed (so the caller can resize the PTY too).
    pub fn resize(&mut self, cols: usize, rows: usize) -> bool {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return false;
        }
        self.cols = cols;
        self.rows = rows;
        self.vt.resize(TermSize::new(cols, rows));
        true
    }

    /// Whether the program requested application cursor keys (DECCKM) — changes the
    /// arrow-key escape sequences we send (`ESC O A` vs `ESC [ A`).
    pub fn app_cursor(&self) -> bool {
        self.vt.mode().contains(TermMode::APP_CURSOR)
    }

    /// Whether the program enabled bracketed-paste mode (DEC 2004). If so, pasted
    /// text must be wrapped in `ESC[200~`…`ESC[201~` so the app treats it as one
    /// literal block (e.g. a shell won't run each pasted line on its own).
    /// Which mouse events the running program asked for (vim `mouse=a`, htop,
    /// lazygit, tmux) and how to encode them. `None` when it reads no mouse at all,
    /// so clicks select and the wheel scrolls our own scrollback.
    pub fn mouse_report(&self) -> Option<MouseReport> {
        let m = self.vt.mode();
        m.intersects(TermMode::MOUSE_MODE).then(|| MouseReport {
            drag: m.contains(TermMode::MOUSE_DRAG),
            motion: m.contains(TermMode::MOUSE_MOTION),
            encoding: if m.contains(TermMode::SGR_MOUSE) {
                MouseEncoding::Sgr
            } else if m.contains(TermMode::UTF8_MOUSE) {
                MouseEncoding::Utf8
            } else {
                MouseEncoding::X10
            },
        })
    }

    /// Whether the wheel should turn into arrow keys: a full-screen program (less,
    /// man, git's pager) is on the alternate screen, which has no scrollback, and
    /// didn't ask for the mouse itself. xterm's "alternate scroll" mode, on unless
    /// the program turns it off.
    pub fn alternate_scroll(&self) -> bool {
        let m = self.vt.mode();
        m.contains(TermMode::ALT_SCREEN | TermMode::ALTERNATE_SCROLL)
            && !m.intersects(TermMode::MOUSE_MODE)
    }

    pub fn bracketed_paste(&self) -> bool {
        self.vt.mode().contains(TermMode::BRACKETED_PASTE)
    }

    // --- shell integration ------------------------------------------------------

    /// Act on a shell-integration mark, with the cursor where the shell put it.
    fn on_mark(&mut self, mark: Mark) {
        self.shell.seen = true;
        let cursor = self.vt.grid().cursor.point;
        match mark {
            Mark::PromptStart => {
                // A command that never reported finishing (no `D`) ends here.
                if matches!(self.shell.phase, Phase::Running) {
                    self.finish(None);
                }
                self.shell.current += 1;
                let id = self.shell.current;
                self.shell.prompts.insert(id, PromptInfo::default());
                while self.shell.prompts.len() > PROMPTS_KEPT {
                    self.shell.prompts.pop_first();
                }
                self.shell.phase = Phase::Prompt { start: cursor };
            }
            Mark::InputStart => {
                if let Phase::Prompt { start } = self.shell.phase {
                    self.tag(start, cursor, PROMPT_TAG);
                }
                self.shell.phase = Phase::Input { start: cursor };
            }
            Mark::OutputStart => {
                if let Phase::Input { start } = self.shell.phase {
                    self.tag(start, cursor, CMD_TAG);
                    self.start_command();
                }
            }
            Mark::Finished(exit) => {
                if matches!(self.shell.phase, Phase::Running) {
                    self.finish(exit);
                }
            }
        }
    }

    /// Enter was pressed: if the shell is at its prompt, the command starts now (for
    /// shells that don't send `C`, such as cmd and PowerShell without a key hook).
    pub fn command_entered(&mut self) {
        if matches!(self.shell.phase, Phase::Input { .. }) {
            self.start_command();
        }
    }

    fn start_command(&mut self) {
        if let Some(p) = self.shell.prompts.get_mut(&self.shell.current) {
            p.started = Some(std::time::Instant::now());
        }
        self.shell.phase = Phase::Running;
    }

    fn finish(&mut self, exit: Option<i32>) {
        self.shell.phase = Phase::Idle;
        if let Some(p) = self.shell.prompts.get_mut(&self.shell.current) {
            p.finished = Some(exit);
            if let Some(started) = p.started {
                self.shell.done.push(CommandDone {
                    duration: started.elapsed(),
                    exit,
                });
            }
        }
    }

    /// Tag the cells from `from` up to (not including) `to` as the current prompt's
    /// (`kind` = [`PROMPT_TAG`] or [`CMD_TAG`]). Cells a program already linked keep
    /// their link.
    fn tag(&mut self, from: Point, to: Point, kind: &str) {
        let link = Hyperlink::new(
            Some(format!("b{}", self.shell.current)),
            format!("{kind}{}", self.shell.current),
        );
        let last_col = self.cols.saturating_sub(1);
        let top = self.vt.topmost_line();
        let mut p = from;
        while p < to && p.line >= top {
            let cell = &mut self.vt.grid_mut()[p];
            if cell.hyperlink().is_none() {
                cell.set_hyperlink(Some(link.clone()));
            }
            p = if p.column.0 >= last_col {
                Point::new(p.line + 1, Column(0))
            } else {
                Point::new(p.line, p.column + 1)
            };
        }
    }

    /// Commands that finished since the last call.
    pub fn take_commands_done(&mut self) -> Vec<CommandDone> {
        std::mem::take(&mut self.shell.done)
    }

    /// Whether the shell has sent shell-integration marks.
    pub fn shell_integrated(&self) -> bool {
        self.shell.seen
    }

    /// The tag on a line: the id of the first prompt or command cell on it.
    fn line_tag(&self, line: Line) -> Option<(u64, bool)> {
        let grid = self.vt.grid();
        (0..self.cols).find_map(|c| {
            let link = grid[Point::new(line, Column(c))].hyperlink()?;
            let id = tag_id(link.uri())?;
            Some((id, link.uri().starts_with(PROMPT_TAG)))
        })
    }

    /// Each prompt in the buffer as `(id, first line, last tagged line)`, oldest first.
    /// The last tagged line is the prompt's, or its typed command's when the shell
    /// marks that.
    fn prompt_spans(&self) -> Vec<(u64, Line, Line)> {
        let mut spans: Vec<(u64, Line, Line)> = Vec::new();
        let mut line = self.vt.topmost_line();
        let bottom = self.vt.bottommost_line();
        while line <= bottom {
            if let Some((id, _)) = self.line_tag(line) {
                match spans.last_mut() {
                    Some(last) if last.0 == id => last.2 = line,
                    _ => spans.push((id, line, line)),
                }
            }
            line += 1;
        }
        spans
    }

    /// Move the copy-mode cursor to the previous (`older`) or next prompt, keeping it
    /// in view. Returns whether there was one to go to.
    pub fn vi_prompt_jump(&mut self, older: bool) -> bool {
        let here = self.vt.vi_mode_cursor.point.line;
        let mut starts = self.prompt_spans().into_iter().map(|(_, first, _)| first);
        let target = if older {
            starts.rfind(|l| *l < here)
        } else {
            starts.find(|l| *l > here)
        };
        let Some(line) = target else {
            return false;
        };
        let p = Point::new(line, Column(0));
        self.vt.vi_goto_point(p);
        self.vt.scroll_to_point(p);
        self.force_extend();
        true
    }

    /// The output of the command whose prompt is at or above `line` (the copy-mode
    /// cursor): from after its prompt (and typed command) to before the next prompt
    /// or the end. Trailing blank lines are left out. `None` without a prompt there.
    pub fn command_output_at(&self, line: Line) -> Option<CommandOutput> {
        let spans = self.prompt_spans();
        let i = spans.iter().rposition(|(_, first, _)| *first <= line)?;
        let (id, first, last) = spans[i];
        let end = match spans.get(i + 1) {
            Some((_, next, _)) => *next - 1,
            None => self.vt.bottommost_line(),
        };
        let cols = self.cols.saturating_sub(1);
        let output = if last < end {
            self.vt.bounds_to_string(
                Point::new(last + 1, Column(0)),
                Point::new(end, Column(cols)),
            )
        } else {
            String::new()
        };
        let output = output.trim_end().to_string();
        // The typed command: tagged command cells, else what follows the prompt on its
        // last line.
        let grid = self.vt.grid();
        let mut command = String::new();
        let mut l = first;
        while l <= last {
            for c in 0..self.cols {
                let cell = &grid[Point::new(l, Column(c))];
                if cell
                    .hyperlink()
                    .is_some_and(|h| h.uri() == format!("{CMD_TAG}{id}"))
                {
                    command.push(cell.c);
                }
            }
            l += 1;
        }
        if command.trim().is_empty() {
            let row: String = (0..self.cols)
                .map(|c| grid[Point::new(last, Column(c))].c)
                .collect();
            let prompt_end = (0..self.cols)
                .rev()
                .find(|&c| {
                    grid[Point::new(last, Column(c))]
                        .hyperlink()
                        .is_some_and(|h| h.uri().starts_with(PROMPT_TAG))
                })
                .map_or(0, |c| c + 1);
            command = row.chars().skip(prompt_end).collect();
        }
        Some(CommandOutput {
            command: command.trim().to_string(),
            output,
        })
    }

    /// The exit code of each prompt starting on screen, as `(viewport row, exit)`,
    /// for the failed-command marks in the gutter.
    fn visible_prompt_exits(&self) -> Vec<(i32, Option<i32>)> {
        let (top, bottom) = self.viewport_bounds();
        let off = self.vt.grid().display_offset() as i32;
        let mut out = Vec::new();
        let mut prev: Option<u64> = None;
        // Start a line above the screen so a prompt cut by the top isn't mistaken for
        // one starting on the first row.
        let mut line = Line(top.line.0 - 1).max(self.vt.topmost_line());
        while line <= bottom.line {
            let tag = self
                .line_tag(line)
                .filter(|(_, prompt)| *prompt)
                .map(|(id, _)| id);
            if let Some(id) = tag {
                if prev != Some(id) && line >= top.line {
                    let exit = self
                        .shell
                        .prompts
                        .get(&id)
                        .and_then(|p| p.finished)
                        .flatten();
                    out.push((line.0 + off, exit));
                }
            }
            prev = tag;
            line += 1;
        }
        out
    }

    // --- links ------------------------------------------------------------------

    /// The first and last absolute points of what's on screen.
    fn viewport_bounds(&self) -> (Point, Point) {
        let top = Line(-(self.vt.grid().display_offset() as i32));
        let bottom = top + (self.rows as i32 - 1);
        (
            Point::new(top, Column(0)),
            Point::new(bottom, Column(self.cols.saturating_sub(1))),
        )
    }

    /// Every link on screen: hyperlinks programs marked (OSC 8: `ls --hyperlink`,
    /// gcc, cargo, delta), then web addresses in plain text that aren't inside one.
    pub fn visible_links(&self) -> Vec<TermLink> {
        let (start, end) = self.viewport_bounds();
        let grid = self.vt.grid();
        let mut links: Vec<TermLink> = Vec::new();
        // OSC 8: runs of cells carrying the same hyperlink.
        let mut line = start.line;
        while line <= end.line {
            for col in 0..self.cols {
                let point = Point::new(line, Column(col));
                let Some(link) = grid[point].hyperlink().filter(|l| !is_shell_tag(l.uri())) else {
                    continue;
                };
                match links.last_mut() {
                    Some(last)
                        if last.url == link.uri()
                            && last.end.line == line
                            && last.end.column.0 + 1 == col =>
                    {
                        last.end = point;
                    }
                    _ => links.push(TermLink {
                        start: point,
                        end: point,
                        url: link.uri().to_string(),
                    }),
                }
            }
            line += 1;
        }
        // Plain web addresses (across wrapped lines too).
        let Ok(mut regex) = RegexSearch::new(URL_REGEX) else {
            return links;
        };
        let marked = links.len();
        for m in RegexIter::new(start, end, Direction::Right, &self.vt, &mut regex) {
            let (from, to) = (*m.start(), *m.end());
            if links[..marked]
                .iter()
                .any(|l| l.start <= to && from <= l.end)
            {
                continue;
            }
            let text = self.vt.bounds_to_string(from, to);
            let text: String = text.chars().filter(|c| *c != '\n').collect();
            let trimmed = trim_url(&text);
            if trimmed.is_empty() {
                continue;
            }
            // Cells dropped from the end by the trim come off the range too.
            let cut = text.chars().count() - trimmed.chars().count();
            let mut to = to;
            for _ in 0..cut {
                to = if to.column.0 == 0 {
                    Point::new(to.line - 1, Column(self.cols.saturating_sub(1)))
                } else {
                    Point::new(to.line, to.column - 1)
                };
            }
            links.push(TermLink {
                start: from,
                end: to,
                url: trimmed.to_string(),
            });
        }
        links
    }

    /// The link at a visible cell (row 0 = the pane's top line), if any.
    pub fn link_at(&self, col: usize, row: usize) -> Option<TermLink> {
        let p = self.to_absolute(col, row);
        self.visible_links()
            .into_iter()
            .find(|l| l.start <= p && p <= l.end)
    }

    /// Where an absolute point is on screen: its (col, row) in viewport cells.
    pub fn viewport_cell(&self, p: Point) -> Option<(usize, usize)> {
        let row = p.line.0 + self.vt.grid().display_offset() as i32;
        (0..self.rows as i32)
            .contains(&row)
            .then_some((p.column.0, row as usize))
    }

    // --- search -----------------------------------------------------------------

    /// Search the whole buffer for `query` (literally, ignoring case), replacing any
    /// earlier search. The current match is the newest one at or above the copy-mode
    /// cursor (else the newest of all): a terminal is searched from where you are
    /// back through older output. Returns how many matched.
    pub fn search_set(&mut self, query: &str) -> usize {
        self.search = None;
        if query.is_empty() {
            return 0;
        }
        let Ok(regex) = RegexSearch::new(&literal_regex(query)) else {
            return 0;
        };
        let mut search = TermSearch {
            regex,
            matches: Vec::new(),
            current: 0,
            stale: false,
        };
        self.run_search(&mut search);
        let origin = if self.is_vi() {
            self.vt.vi_mode_cursor.point
        } else {
            Point::new(
                self.vt.bottommost_line(),
                Column(self.cols.saturating_sub(1)),
            )
        };
        search.current = search
            .matches
            .iter()
            .rposition(|m| *m.start() <= origin)
            .unwrap_or(search.matches.len().saturating_sub(1));
        let n = search.matches.len();
        self.search = Some(search);
        n
    }

    fn run_search(&self, search: &mut TermSearch) {
        let start = Point::new(self.vt.topmost_line(), Column(0));
        let end = Point::new(
            self.vt.bottommost_line(),
            Column(self.cols.saturating_sub(1)),
        );
        search.matches = RegexIter::new(start, end, Direction::Right, &self.vt, &mut search.regex)
            .take(SEARCH_CAP)
            .collect();
        search.stale = false;
    }

    /// Find the matches again after output moved them. New output lands below, so
    /// the current match keeps its place counted from the oldest and stays on the
    /// same text (until a full scrollback starts dropping its oldest lines).
    pub fn search_refresh(&mut self) {
        let Some(mut search) = self.search.take() else {
            return;
        };
        if search.stale {
            let current = search.current;
            self.run_search(&mut search);
            search.current = current.min(search.matches.len().saturating_sub(1));
        }
        self.search = Some(search);
    }

    /// Step to the next match back through the output (`older`) or forward.
    pub fn search_step(&mut self, older: bool) {
        if let Some(s) = self.search.as_mut().filter(|s| !s.matches.is_empty()) {
            let n = s.matches.len();
            s.current = if older {
                (s.current + n - 1) % n
            } else {
                (s.current + 1) % n
            };
        }
    }

    /// Bring the current match on screen, with the copy-mode cursor on it.
    pub fn search_reveal(&mut self) {
        let Some(m) = self
            .search
            .as_ref()
            .and_then(|s| s.matches.get(s.current).cloned())
        else {
            return;
        };
        if self.is_vi() {
            self.vt.vi_goto_point(*m.start());
            self.force_extend();
        }
        self.vt.scroll_to_point(*m.start());
    }

    /// Drop the search and its highlights.
    pub fn search_clear(&mut self) {
        self.search = None;
    }

    /// `(current, total)` with `current` counted from the newest match (1 = the
    /// newest), as the status bar shows it; `None` without a search.
    pub fn search_position(&self) -> Option<(usize, usize)> {
        let s = self.search.as_ref()?;
        let n = s.matches.len();
        Some((if n == 0 { 0 } else { n - s.current }, n))
    }

    /// The matches on screen, each with whether it's the current one.
    fn visible_matches(&self) -> Vec<(Match, bool)> {
        let Some(s) = self.search.as_ref() else {
            return Vec::new();
        };
        let (top, bottom) = self.viewport_bounds();
        s.matches
            .iter()
            .enumerate()
            .filter(|(_, m)| *m.start() <= bottom && top <= *m.end())
            .map(|(i, m)| (m.clone(), i == s.current))
            .collect()
    }
}

fn to_rgb(c: VtRgb) -> Rgb {
    (c.r, c.g, c.b)
}

fn named_default(n: NamedColor, st: &TermStyle) -> Rgb {
    use NamedColor::*;
    match n {
        Black | DimBlack => st.ansi[0],
        Red | DimRed => st.ansi[1],
        Green | DimGreen => st.ansi[2],
        Yellow | DimYellow => st.ansi[3],
        Blue | DimBlue => st.ansi[4],
        Magenta | DimMagenta => st.ansi[5],
        Cyan | DimCyan => st.ansi[6],
        White | DimWhite => st.ansi[7],
        BrightBlack => st.ansi[8],
        BrightRed => st.ansi[9],
        BrightGreen => st.ansi[10],
        BrightYellow => st.ansi[11],
        BrightBlue => st.ansi[12],
        BrightMagenta => st.ansi[13],
        BrightCyan => st.ansi[14],
        BrightWhite => st.ansi[15],
        Foreground | BrightForeground | DimForeground | Cursor => st.fg,
        Background => st.bg,
    }
}

/// xterm 256-color default for an index the program hasn't overridden.
fn indexed_default(i: u8, st: &TermStyle) -> Rgb {
    match i {
        0..=15 => st.ansi[i as usize],
        16..=231 => {
            let i = i - 16;
            let step = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
            (step(i / 36), step((i % 36) / 6), step(i % 6))
        }
        _ => {
            let v = 8 + (i - 232) * 10;
            (v, v, v)
        }
    }
}

/// Blend two colors: `t` (0..=255) of `b` over `a`.
fn lerp(a: Rgb, b: Rgb, t: u32) -> Rgb {
    let f = |x: u8, y: u8| ((x as u32 * (255 - t) + y as u32 * t) / 255) as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// Draw box-drawing (U+2500..) and block (U+2580..) glyphs PROCEDURALLY so they tile
/// seamlessly (table borders connect, blocks fill the cell) instead of relying on the
/// font's glyph metrics, which leave gaps/misalignment. Returns `true` if `ch` was a
/// special it handled (caller then skips the normal glyph).
#[allow(clippy::too_many_arguments)]
fn draw_special(
    buf: &mut [u32],
    w: usize,
    h: usize,
    cx: i32,
    cy: i32,
    cw: i32,
    ch_h: i32,
    c: char,
    fg: Rgb,
    bg: Rgb,
) -> bool {
    let fill = |buf: &mut [u32], x0: i32, y0: i32, x1: i32, y1: i32, col: Rgb| {
        draw::fill_rect(
            buf,
            w,
            h,
            x0.max(0) as usize,
            y0.max(0) as usize,
            x1.max(0) as usize,
            y1.max(0) as usize,
            col,
        );
    };
    // Block elements (U+2580..U+259F) — drawn as exact cell fills so block/pixel art
    // (e.g. the Claude Code logo) tiles cleanly.
    let mid_x = cx + cw / 2;
    let mid_y = cy + ch_h / 2;
    // Fill the bottom `n/8` of the cell (for lower-eighth blocks ▁▂▃…).
    let lower =
        |buf: &mut [u32], n: i32| fill(buf, cx, cy + ch_h * (8 - n) / 8, cx + cw, cy + ch_h, fg);
    // Fill the left `n/8` of the cell (for left-eighth blocks ▏▎▍…).
    let left = |buf: &mut [u32], n: i32| fill(buf, cx, cy, cx + cw * n / 8, cy + ch_h, fg);
    // Quadrant fills (UL/UR/LL/LR), for ▖▗▘▙▚▛▜▝▞▟ and the half blocks.
    let ul = |buf: &mut [u32]| fill(buf, cx, cy, mid_x, mid_y, fg);
    let ur = |buf: &mut [u32]| fill(buf, mid_x, cy, cx + cw, mid_y, fg);
    let ll = |buf: &mut [u32]| fill(buf, cx, mid_y, mid_x, cy + ch_h, fg);
    let lr = |buf: &mut [u32]| fill(buf, mid_x, mid_y, cx + cw, cy + ch_h, fg);
    match c {
        '\u{2580}' => {
            fill(buf, cx, cy, cx + cw, mid_y, fg);
            return true;
        } // ▀ upper half
        '\u{2581}'..='\u{2587}' => {
            lower(buf, c as i32 - 0x2580);
            return true;
        } // ▁..▇ lower n/8
        '\u{2588}' => {
            fill(buf, cx, cy, cx + cw, cy + ch_h, fg);
            return true;
        } // █ full
        '\u{2589}'..='\u{258f}' => {
            left(buf, 8 - (c as i32 - 0x2588));
            return true;
        } // ▉..▏ left n/8
        '\u{2590}' => {
            fill(buf, mid_x, cy, cx + cw, cy + ch_h, fg);
            return true;
        } // ▐ right half
        '\u{2591}' => {
            fill(buf, cx, cy, cx + cw, cy + ch_h, lerp(bg, fg, 64));
            return true;
        } // ░
        '\u{2592}' => {
            fill(buf, cx, cy, cx + cw, cy + ch_h, lerp(bg, fg, 128));
            return true;
        } // ▒
        '\u{2593}' => {
            fill(buf, cx, cy, cx + cw, cy + ch_h, lerp(bg, fg, 192));
            return true;
        } // ▓
        '\u{2594}' => {
            fill(buf, cx, cy, cx + cw, cy + ch_h / 8, fg);
            return true;
        } // ▔ upper 1/8
        '\u{2595}' => {
            fill(buf, cx + cw * 7 / 8, cy, cx + cw, cy + ch_h, fg);
            return true;
        } // ▕ right 1/8
        '\u{2596}' => {
            ll(buf);
            return true;
        }
        '\u{2597}' => {
            lr(buf);
            return true;
        }
        '\u{2598}' => {
            ul(buf);
            return true;
        }
        '\u{2599}' => {
            ul(buf);
            ll(buf);
            lr(buf);
            return true;
        }
        '\u{259a}' => {
            ul(buf);
            lr(buf);
            return true;
        }
        '\u{259b}' => {
            ul(buf);
            ur(buf);
            ll(buf);
            return true;
        }
        '\u{259c}' => {
            ul(buf);
            ur(buf);
            lr(buf);
            return true;
        }
        '\u{259d}' => {
            ur(buf);
            return true;
        }
        '\u{259e}' => {
            ur(buf);
            ll(buf);
            return true;
        }
        '\u{259f}' => {
            ur(buf);
            ll(buf);
            lr(buf);
            return true;
        }
        _ => {}
    }
    // Line-drawing: which arms reach the cell centre (up/down/left/right). Heavy and
    // double variants are drawn as their light single-line equivalent.
    let (u, d, l, r) = match c {
        '\u{2500}' | '\u{2501}' | '\u{2550}' => (false, false, true, true), // ─ ━ ═
        '\u{2502}' | '\u{2503}' | '\u{2551}' => (true, true, false, false), // │ ┃ ║
        '\u{250c}' | '\u{250f}' | '\u{2554}' | '\u{256d}' => (false, true, false, true), // ┌ ┏ ╔ ╭
        '\u{2510}' | '\u{2513}' | '\u{2557}' | '\u{256e}' => (false, true, true, false), // ┐ ┓ ╗ ╮
        '\u{2514}' | '\u{2517}' | '\u{255a}' | '\u{2570}' => (true, false, false, true), // └ ┗ ╚ ╰
        '\u{2518}' | '\u{251b}' | '\u{255d}' | '\u{256f}' => (true, false, true, false), // ┘ ┛ ╝ ╯
        '\u{251c}' | '\u{2523}' | '\u{2560}' => (true, true, false, true),  // ├ ┣ ╠
        '\u{2524}' | '\u{252b}' | '\u{2563}' => (true, true, true, false),  // ┤ ┫ ╣
        '\u{252c}' | '\u{2533}' | '\u{2566}' => (false, true, true, true),  // ┬ ┳ ╦
        '\u{2534}' | '\u{253b}' | '\u{2569}' => (true, false, true, true),  // ┴ ┻ ╩
        '\u{253c}' | '\u{254b}' | '\u{256c}' => (true, true, true, true),   // ┼ ╋ ╬
        _ => return false,
    };
    let th = (ch_h / 9).max(1);
    let midx = cx + cw / 2;
    let midy = cy + ch_h / 2;
    let (vx0, vx1) = (midx - th / 2, midx - th / 2 + th);
    let (hy0, hy1) = (midy - th / 2, midy - th / 2 + th);
    if u {
        fill(buf, vx0, cy, vx1, hy1, fg);
    }
    if d {
        fill(buf, vx0, hy0, vx1, cy + ch_h, fg);
    }
    if l {
        fill(buf, cx, hy0, vx1, hy1, fg);
    }
    if r {
        fill(buf, vx0, hy0, cx + cw, hy1, fg);
    }
    true
}

/// Resolve a cell color to RGB: a program-set palette entry wins, else our default.
fn resolve(c: Color, colors: &Colors, st: &TermStyle) -> Rgb {
    match c {
        Color::Spec(rgb) => to_rgb(rgb),
        Color::Named(n) => colors[n]
            .map(to_rgb)
            .unwrap_or_else(|| named_default(n, st)),
        Color::Indexed(i) => colors[i as usize]
            .map(to_rgb)
            .unwrap_or_else(|| indexed_default(i, st)),
    }
}

/// How a terminal is drawn this frame, beyond its grid: the user's settings and
/// whether its cursor has the keyboard.
#[derive(Clone, Copy, Debug)]
pub struct RenderOpts {
    /// Draw bold, italic, dim, hidden, underlined and struck-through text as such
    /// (`term.text_styles`); off, only colours and inverse show.
    pub text_styles: bool,
    /// Draw the cursor in the shape the program asked for, hollow while the terminal
    /// doesn't have the keyboard (`term.cursor_shape`); off, always a solid block.
    pub cursor_shapes: bool,
    /// The terminal has the keyboard: its pane is focused, the window is, and keys
    /// go to it (Passthrough) or to its copy mode.
    pub focused: bool,
    /// The blink cycle is in its visible half (always true when nothing blinks).
    pub blink_on: bool,
    /// Mark the prompts of failed commands in the gutter (`term.shell_integration`).
    pub shell_marks: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        RenderOpts {
            text_styles: true,
            cursor_shapes: true,
            focused: true,
            blink_on: true,
            shell_marks: true,
        }
    }
}

/// The face a cell's text is drawn in.
fn font_style(flags: Flags) -> draw::FontStyle {
    match (flags.contains(Flags::BOLD), flags.contains(Flags::ITALIC)) {
        (true, true) => draw::FontStyle::BoldItalic,
        (true, false) => draw::FontStyle::Bold,
        (false, true) => draw::FontStyle::Italic,
        (false, false) => draw::FontStyle::Regular,
    }
}

/// Draw a cell's underline (single, double, curly, dotted or dashed) and
/// strikethrough over the `cw`-wide cell at (`cx`, `cy`). Patterns follow the
/// absolute x so they run on unbroken across cells.
#[allow(clippy::too_many_arguments)]
fn draw_decorations(
    buf: &mut [u32],
    w: usize,
    h: usize,
    (cx, cy, cw): (i32, i32, i32),
    (cell_w, cell_h, baseline_off): (i32, i32, i32),
    flags: Flags,
    underline: Rgb,
    strike: Rgb,
) {
    let mut fill = |x0: i32, y0: i32, x1: i32, y1: i32, c: Rgb| {
        if x1 > x0 && y1 > y0 && x1 > 0 && y1 > 0 {
            draw::fill_rect(
                buf,
                w,
                h,
                x0.max(0) as usize,
                y0.max(0) as usize,
                x1 as usize,
                y1 as usize,
                c,
            );
        }
    };
    let th = (cell_h / 14).max(1);
    let bottom = cy + cell_h;
    // A third of the way into the descender band, kept inside the cell.
    let descent = cell_h - baseline_off;
    let uy = (cy + baseline_off + (descent / 3).max(1)).min(bottom - th);
    if flags.contains(Flags::DOUBLE_UNDERLINE) {
        let second = if uy + 3 * th <= bottom {
            uy + 2 * th
        } else {
            uy - 2 * th
        };
        fill(cx, uy, cx + cw, uy + th, underline);
        fill(cx, second, cx + cw, second + th, underline);
    } else if flags.contains(Flags::UNDERCURL) {
        let amp = ((descent - th) / 3).max(1) as f32;
        let mid = (uy as f32).min((bottom - th) as f32 - amp);
        for x in cx..cx + cw {
            let phase = x as f32 / cell_w.max(1) as f32 * std::f32::consts::TAU;
            let y = (mid - amp * phase.sin()).round() as i32;
            fill(x, y, x + 1, y + th, underline);
        }
    } else if flags.contains(Flags::DOTTED_UNDERLINE) {
        for x in cx..cx + cw {
            if (x / th) % 2 == 0 {
                fill(x, uy, x + 1, uy + th, underline);
            }
        }
    } else if flags.contains(Flags::DASHED_UNDERLINE) {
        let dash = (cell_w / 3).max(2);
        for x in cx..cx + cw {
            if (x / dash) % 3 != 2 {
                fill(x, uy, x + 1, uy + th, underline);
            }
        }
    } else if flags.contains(Flags::UNDERLINE) {
        fill(cx, uy, cx + cw, uy + th, underline);
    }
    if flags.contains(Flags::STRIKEOUT) {
        // Through the middle of lowercase letters (about half the x-height).
        let sy = cy + baseline_off - cell_h * 2 / 9;
        fill(cx, sy, cx + cw, sy + th, strike);
    }
}

/// Paint the terminal grid into `buf`. The grid's top-left cell is at (`x0`, `y0`);
/// `cell_w`/`cell_h` are the monospace cell size. Cells past the bottom of the
/// content band (`>= clip_bottom`) are skipped; the caller paints the bars on top.
/// `st` supplies the default fg/bg + ANSI palette (the `:theme term_*` overrides),
/// `opts` the text-style and cursor settings and focus.
#[allow(clippy::too_many_arguments)]
pub fn render(
    pty: &PtyTerm,
    p: &Painter,
    buf: &mut [u32],
    w: usize,
    h: usize,
    x0: i32,
    y0: i32,
    cell_w: i32,
    cell_h: i32,
    clip_bottom: i32,
    st: &TermStyle,
    opts: RenderOpts,
) {
    let content = pty.vt.renderable_content();
    let colors = content.colors;
    let cursor = content.cursor;
    let selection = content.selection;
    let baseline_off = cell_h * 3 / 4;
    let mut cursor_cell = (' ', draw::FontStyle::Regular, false);
    // `/` matches on screen, and the Ctrl-hovered link.
    let hits = pty.visible_matches();
    let hover = pty.link_hover;

    // Both `display_iter` cells AND `cursor.point` use ABSOLUTE buffer coordinates
    // (lines are negative for scrollback): `display_iter` starts at
    // `Line(-display_offset - 1)`. To paint them in the visible band we convert each
    // to a viewport row the way Alacritty's own frontend does (`point_to_viewport`):
    // `row = absolute_line + display_offset`. Without this, scrolling copy-mode into
    // scrollback clips the scrolled-in lines (negative rows) and pins the rest at
    // their absolute rows — the "camera doesn't follow the cursor" bug, where the
    // bottom goes blank as you scroll up. At display_offset 0 this is a no-op.
    let display_offset = pty.vt.grid().display_offset() as i32;
    let cursor_row = cursor.point.line.0 + display_offset;
    let cursor_col = cursor.point.column.0 as i32;

    for item in content.display_iter {
        let cell = item.cell;
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        let mut flags = if opts.text_styles {
            cell.flags
        } else {
            cell.flags & (Flags::INVERSE | Flags::WIDE_CHAR)
        };
        if hover.is_some_and(|(from, to)| from <= item.point && item.point <= to) {
            flags.insert(Flags::UNDERLINE);
        }
        let style = font_style(flags);
        let row = item.point.line.0 + display_offset;
        let col = item.point.column.0 as i32;
        if row == cursor_row && col == cursor_col {
            let shown = if flags.contains(Flags::HIDDEN) {
                ' '
            } else {
                cell.c
            };
            cursor_cell = (shown, style, flags.contains(Flags::WIDE_CHAR));
        }
        let cx = x0 + col * cell_w;
        let cy = y0 + row * cell_h;
        if cy < y0 || cy + cell_h > clip_bottom {
            continue;
        }
        let mut fg = resolve(cell.fg, colors, st);
        let mut bg = resolve(cell.bg, colors, st);
        // Dim (SGR 2: hints, inactive text) blends a third of the way into the
        // background, which works on light schemes as well as dark ones.
        if flags.contains(Flags::DIM) {
            fg = lerp(fg, bg, 0x55);
        }
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // `/` matches (the current one brighter), then the selection on top —
        // copy mode's `v`/`V` and mouse drags share it.
        if let Some((_, current)) = hits.iter().find(|(m, _)| m.contains(&item.point)) {
            bg = if *current { draw::FIND_CUR } else { draw::FIND };
        }
        if selection.is_some_and(|s| s.contains(item.point)) {
            bg = draw::SEL;
        }
        let cw = if flags.contains(Flags::WIDE_CHAR) {
            cell_w * 2
        } else {
            cell_w
        };
        if bg != st.bg {
            draw::fill_rect(
                buf,
                w,
                h,
                cx.max(0) as usize,
                cy.max(0) as usize,
                (cx + cw).max(0) as usize,
                (cy + cell_h).max(0) as usize,
                bg,
            );
        }
        if cell.c != ' '
            && cell.c != '\0'
            && !flags.contains(Flags::HIDDEN)
            && !draw_special(buf, w, h, cx, cy, cell_w, cell_h, cell.c, fg, bg)
        {
            p.put_char_styled(
                buf,
                w,
                h,
                cx.max(0) as usize,
                (cy + baseline_off) as usize,
                cell.c,
                fg,
                style,
            );
        }
        if flags.intersects(Flags::ALL_UNDERLINES | Flags::STRIKEOUT) {
            let underline = cell
                .underline_color()
                .map_or(fg, |c| resolve(c, colors, st));
            draw_decorations(
                buf,
                w,
                h,
                (cx, cy, cw),
                (cell_w, cell_h, baseline_off),
                flags,
                underline,
                fg,
            );
        }
    }

    // A failed command: a red bar in the gutter beside its prompt.
    if opts.shell_marks {
        for (row, exit) in pty.visible_prompt_exits() {
            let cy = y0 + row * cell_h;
            if exit.is_some_and(|code| code != 0) && cy >= y0 && cy + cell_h <= clip_bottom {
                draw::fill_rect(
                    buf,
                    w,
                    h,
                    (x0 - 3).max(0) as usize,
                    cy as usize,
                    (x0 - 1).max(0) as usize,
                    (cy + cell_h) as usize,
                    draw::ERR,
                );
            }
        }
    }

    draw_cursor(
        pty,
        p,
        buf,
        (w, h),
        (x0, y0, clip_bottom),
        (cell_w, cell_h, baseline_off),
        (cursor_col, cursor_row),
        cursor.shape,
        cursor_cell,
        colors,
        st,
        opts,
    );
}

/// The cursor's look for a frame: `None` hides it (the program did, or it's in the
/// off half of a blink). Copy mode's cursor is always a steady block; otherwise the
/// program's shape when `cursor_shapes` is on (hollow and steady without the
/// keyboard), else a solid block; either blinks when the program's style does.
fn cursor_look(
    shape: CursorShape,
    blinking: bool,
    vi: bool,
    opts: RenderOpts,
) -> Option<CursorShape> {
    if shape == CursorShape::Hidden {
        return None;
    }
    if vi {
        return Some(CursorShape::Block);
    }
    let shape = match (opts.cursor_shapes, opts.focused) {
        (false, _) => CursorShape::Block,
        (true, false) => return Some(CursorShape::HollowBlock),
        (true, true) => shape,
    };
    (!blinking || opts.blink_on).then_some(shape)
}

/// Paint the cursor at viewport cell (`col`, `row`) in the shape [`cursor_look`]
/// picks. A block inverts the character under it (`cell`: its char, face, width).
#[allow(clippy::too_many_arguments)]
fn draw_cursor(
    pty: &PtyTerm,
    p: &Painter,
    buf: &mut [u32],
    (w, h): (usize, usize),
    (x0, y0, clip_bottom): (i32, i32, i32),
    (cell_w, cell_h, baseline_off): (i32, i32, i32),
    (col, row): (i32, i32),
    shape: CursorShape,
    (ch, style, wide): (char, draw::FontStyle, bool),
    colors: &Colors,
    st: &TermStyle,
    opts: RenderOpts,
) {
    let Some(shape) = cursor_look(shape, pty.cursor_blinking(), pty.is_vi(), opts) else {
        return;
    };
    let cx = x0 + col * cell_w;
    let cy = y0 + row * cell_h;
    if cy < y0 || cy + cell_h > clip_bottom {
        return;
    }
    let cw = if wide { cell_w * 2 } else { cell_w };
    // A program can set the cursor colour (OSC 12).
    let color = colors[NamedColor::Cursor].map_or(st.fg, to_rgb);
    let mut fill = |x0: i32, y0: i32, x1: i32, y1: i32| {
        if x1 > x0 && y1 > y0 && x1 > 0 && y1 > 0 {
            draw::fill_rect(
                buf,
                w,
                h,
                x0.max(0) as usize,
                y0.max(0) as usize,
                x1 as usize,
                y1 as usize,
                color,
            );
        }
    };
    // Bar and underline thickness, as Alacritty draws them: 15% of a cell's width.
    let th = ((cell_w as f32 * 0.15).round() as i32).max(1);
    match shape {
        CursorShape::Beam => fill(cx, cy, cx + th, cy + cell_h),
        CursorShape::Underline => fill(cx, cy + cell_h - th, cx + cw, cy + cell_h),
        CursorShape::HollowBlock => {
            let t = (cell_w / 10).max(1);
            fill(cx, cy, cx + cw, cy + t);
            fill(cx, cy + cell_h - t, cx + cw, cy + cell_h);
            fill(cx, cy, cx + t, cy + cell_h);
            fill(cx + cw - t, cy, cx + cw, cy + cell_h);
        }
        _ => {
            fill(cx, cy, cx + cw, cy + cell_h);
            if ch != ' '
                && ch != '\0'
                && !draw_special(buf, w, h, cx, cy, cell_w, cell_h, ch, st.bg, color)
            {
                p.put_char_styled(
                    buf,
                    w,
                    h,
                    cx.max(0) as usize,
                    (cy + baseline_off) as usize,
                    ch,
                    st.bg,
                    style,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vi_copy_mode_selects_and_yanks() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        pty.feed(b"hello world\r\n"); // row 0 = "hello world", cursor drops to row 1
        assert!(!pty.is_vi());
        pty.toggle_vi();
        assert!(pty.is_vi());
        // Move the vi cursor to the start of "hello world".
        pty.vi_motion(ViMotion::Up);
        pty.vi_motion(ViMotion::First);
        // Start a charwise selection and extend across the word — the force-extend in
        // `vi_motion` must grow the just-started (zero-width) selection.
        pty.start_selection(false);
        for _ in 0..10 {
            pty.vi_motion(ViMotion::Right);
        }
        let yanked = pty.yank().expect("selection should yank text");
        assert!(yanked.contains("hello"), "yank was {yanked:?}");
        // Yank clears the selection; leaving vi mode works.
        pty.toggle_vi();
        assert!(!pty.is_vi());
    }

    #[test]
    fn mouse_drag_selects_across_the_row() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        pty.feed(b"hello world\r\n");
        // Press on the 'h' (row 0, col 0) and drag onto the 'd' (col 10). The right
        // half of the last cell is what makes the drag INCLUSIVE of it.
        pty.mouse_select(0, 0, false, SelectKind::Char);
        pty.mouse_select_to(10, 0, true);
        assert_eq!(pty.selection_text().as_deref(), Some("hello world"));
        // Reading the text must not consume the selection (the highlight stays).
        assert_eq!(pty.selection_text().as_deref(), Some("hello world"));
        // A double-click anywhere in "world" takes the whole word, no drag needed.
        pty.mouse_select(8, 0, false, SelectKind::Word);
        assert_eq!(pty.selection_text().as_deref(), Some("world"));
        // A triple-click takes the line — with its newline, so pasting a run of lines
        // keeps them on separate lines (alacritty's linewise convention).
        pty.mouse_select(8, 0, false, SelectKind::Line);
        assert_eq!(pty.selection_text().as_deref(), Some("hello world\n"));
        assert!(pty.clear_selection());
        assert_eq!(pty.selection_text(), None);
    }

    #[test]
    fn mouse_selection_follows_the_viewport_into_scrollback() {
        // 3 visible rows and 5 written lines (the last \r\n opens the cursor's own
        // empty line), so "one"/"two" are pushed up into scrollback.
        let mut pty = PtyTerm::new(20, 3, DEFAULT_SCROLLBACK);
        pty.feed(b"one\r\ntwo\r\nthree\r\nfour\r\n");
        // At the bottom, visible row 0 is "three".
        pty.mouse_select(0, 0, false, SelectKind::Line);
        assert_eq!(pty.selection_text().as_deref(), Some("three\n"));
        // Scroll one line back: the same PIXEL row now shows "two", and clicking it
        // must select that, not the absolute line it used to be.
        pty.scroll_display(1);
        pty.mouse_select(0, 0, false, SelectKind::Line);
        assert_eq!(pty.selection_text().as_deref(), Some("two\n"));
    }

    #[test]
    fn vi_find_char_moves_along_the_line() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        pty.feed(b"abcdef\r\n");
        pty.toggle_vi();
        // Park the cursor at the start of "abcdef".
        pty.vi_motion(ViMotion::Up);
        pty.vi_motion(ViMotion::First);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 0);
        // `f e` lands ON the 'e' (column 4).
        pty.vi_find_char('e', true, false);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 4);
        // `F b` searches backward to 'b' (column 1).
        pty.vi_find_char('b', false, false);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 1);
        // `t f` (till) from column 1 stops one short of 'f' → column 4.
        pty.vi_find_char('f', true, true);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 4);
        // A char that isn't on the line leaves the cursor put.
        pty.vi_find_char('z', true, false);
        assert_eq!(pty.vt.vi_mode_cursor.point.column.0, 4);
    }

    #[test]
    fn vi_cursor_scrolls_viewport_into_scrollback() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        // Print more lines than fit, so there's scrollback.
        for i in 0..20 {
            pty.feed(format!("line{i}\r\n").as_bytes());
        }
        assert_eq!(pty.vt.grid().display_offset(), 0); // viewport at the bottom
        pty.toggle_vi();
        // Moving the vi cursor up past the top must scroll the display (camera
        // follows) — the regression we're guarding.
        for _ in 0..15 {
            pty.vi_motion(ViMotion::Up);
        }
        assert!(
            pty.vt.grid().display_offset() > 0,
            "viewport should have scrolled up"
        );
        // The cursor's VIEWPORT row (absolute line + display_offset) must stay within
        // the visible band — this is what `render` uses to place the block, so a value
        // outside [0, rows) is the "cursor flew off-screen, camera didn't follow" bug.
        let vp_row = pty.vt.vi_mode_cursor.point.line.0 + pty.vt.grid().display_offset() as i32;
        assert!(
            (0..pty.rows as i32).contains(&vp_row),
            "cursor viewport row {vp_row} should be on-screen (rows={})",
            pty.rows
        );
        // gg jumps to the very top.
        pty.vi_top();
        assert!(pty.vt.grid().display_offset() > 0);
        let vp_row = pty.vt.vi_mode_cursor.point.line.0 + pty.vt.grid().display_offset() as i32;
        assert!(
            (0..pty.rows as i32).contains(&vp_row),
            "cursor row {vp_row} on-screen after gg"
        );
    }

    #[test]
    fn osc_9_9_reports_cwd_even_split_across_feed_chunks() {
        let mut pty = PtyTerm::new(80, 24, DEFAULT_SCROLLBACK);
        assert_eq!(pty.cwd(), None);
        // BEL-terminated, split mid-sequence across two PTY reads (the real
        // failure mode a per-chunk regex would miss).
        pty.feed(b"prompt> \x1b]9;9;C:\\proj");
        pty.feed(b"ects\\browser\x07more output");
        assert_eq!(pty.cwd(), Some("C:\\projects\\browser"));
        // ST-terminated (ESC \) and quoted (some prompts emit "%cd%" quoted);
        // a later report replaces the earlier one.
        pty.feed(b"\x1b]9;9;\"D:\\stuff\"\x1b\\");
        assert_eq!(pty.cwd(), Some("D:\\stuff"));
        // A WSL path via wslpath -w survives verbatim.
        pty.feed(b"\x1b]9;9;\\\\wsl.localhost\\Ubuntu\\home\\kayf\x07");
        assert_eq!(pty.cwd(), Some("\\\\wsl.localhost\\Ubuntu\\home\\kayf"));
        // Unrelated OSCs (title etc.) don't disturb the last-known cwd.
        pty.feed(b"\x1b]0;some title\x07");
        assert_eq!(pty.cwd(), Some("\\\\wsl.localhost\\Ubuntu\\home\\kayf"));
    }

    #[test]
    fn osc_7_file_uri_reports_linux_and_windows_paths() {
        let mut pty = PtyTerm::new(80, 24, DEFAULT_SCROLLBACK);
        // bash-in-WSL convention: file://<hostname><path>, percent-encoded.
        pty.feed(b"\x1b]7;file://mybox/home/kayf/my%20dir\x07");
        assert_eq!(pty.cwd(), Some("/home/kayf/my dir"));
        // A Windows-style file URI decodes to a drive path.
        pty.feed(b"\x1b]7;file:///C:/Users/kayf\x07");
        assert_eq!(pty.cwd(), Some("C:\\Users\\kayf"));
        // An empty/rootless URI is ignored, keeping the last good report.
        pty.feed(b"\x1b]7;file://\x07");
        assert_eq!(pty.cwd(), Some("C:\\Users\\kayf"));
    }

    #[test]
    fn resuming_the_shell_snaps_the_camera_back_to_the_prompt() {
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        for i in 0..20 {
            pty.feed(format!("line{i}\r\n").as_bytes());
        }
        // Ctrl+S → copy mode, scroll up into scrollback.
        pty.toggle_vi();
        for _ in 0..15 {
            pty.vi_motion(ViMotion::Up);
        }
        assert!(
            pty.vt.grid().display_offset() > 0,
            "should be parked in scrollback"
        );
        // Esc/i resumes: leaving vi mode alone does NOT reset the viewport (the
        // "camera stuck after Esc" bug) — the resume path must snap it down.
        pty.toggle_vi();
        pty.scroll_to_bottom();
        assert_eq!(
            pty.vt.grid().display_offset(),
            0,
            "camera should be back at the live line"
        );
    }

    #[test]
    fn output_keeps_a_scrolled_back_view_where_it_is() {
        // Why typing has to snap the view itself: the program's echo alone doesn't.
        let mut pty = PtyTerm::new(20, 5, DEFAULT_SCROLLBACK);
        for i in 0..20 {
            pty.feed(format!("line{i}\r\n").as_bytes());
        }
        pty.scroll_display(3);
        assert!(pty.scrolled_back());
        pty.feed(b"typed");
        assert!(pty.scrolled_back(), "output must not move the view");
        pty.scroll_to_bottom();
        assert!(!pty.scrolled_back());
    }

    /// A 10×2 terminal fed `bytes`, painted at 20 px cells into a fresh buffer with
    /// the cursor hidden, so only the text shows.
    fn paint(bytes: &[u8], opts: RenderOpts) -> Vec<u32> {
        let mut pty = PtyTerm::new(10, 2, DEFAULT_SCROLLBACK);
        pty.feed(bytes);
        pty.feed(b"\x1b[?25l");
        paint_pty(&pty, opts)
    }

    const CELL: (i32, i32) = (10, 20);

    fn paint_pty(pty: &PtyTerm, opts: RenderOpts) -> Vec<u32> {
        let (w, h) = (100usize, 40usize);
        let p = Painter::new(15.0).unwrap();
        let mut buf = vec![0u32; w * h];
        let st = TermStyle::default();
        render(
            pty, &p, &mut buf, w, h, 0, 0, CELL.0, CELL.1, h as i32, &st, opts,
        );
        buf
    }

    /// The pixels of cell `col` in row 0.
    fn cell(buf: &[u32], col: usize) -> Vec<u32> {
        let (cw, ch) = (CELL.0 as usize, CELL.1 as usize);
        (0..ch)
            .flat_map(|y| buf[y * 100 + col * cw..][..cw].to_vec())
            .collect()
    }

    fn lit(px: &[u32]) -> usize {
        px.iter().filter(|&&p| p != 0).count()
    }

    #[test]
    fn styled_text_is_drawn_differently_unless_styles_are_off() {
        let on = RenderOpts::default();
        let off = RenderOpts {
            text_styles: false,
            ..on
        };
        let plain = paint(b"l", on);
        for sgr in ["1", "3", "4", "9", "2", "4:2", "4:3", "4:4", "4:5"] {
            let styled = paint(format!("\x1b[{sgr}ml").as_bytes(), on);
            assert_ne!(styled, plain, "SGR {sgr} drew like plain text");
            let ignored = paint(format!("\x1b[{sgr}ml").as_bytes(), off);
            assert_eq!(ignored, plain, "SGR {sgr} with text styles off");
        }
        // Hidden text (SGR 8) draws nothing.
        assert_eq!(lit(&paint(b"\x1b[8ml", on)), 0);
        // An underline marks a space too; plain spaces stay blank.
        assert_eq!(lit(&cell(&paint(b" ", on), 0)), 0);
        assert!(lit(&cell(&paint(b"\x1b[4m ", on), 0)) > 0);
    }

    #[test]
    fn dim_text_is_fainter() {
        let brightest = |buf: &[u32]| {
            buf.iter()
                .map(|&p| (p >> 16 & 0xff) + (p >> 8 & 0xff) + (p & 0xff))
                .max()
                .unwrap()
        };
        let plain = paint(b"\x1b[37mM", RenderOpts::default());
        let dim = paint(b"\x1b[2;37mM", RenderOpts::default());
        assert!(brightest(&dim) < brightest(&plain));
    }

    #[test]
    fn the_cursor_takes_the_programs_shape_and_goes_hollow_unfocused() {
        let opts = RenderOpts::default();
        let blur = RenderOpts {
            focused: false,
            ..opts
        };
        let no_shapes = RenderOpts {
            cursor_shapes: false,
            ..opts
        };
        use CursorShape::*;
        assert_eq!(cursor_look(Beam, false, false, opts), Some(Beam));
        assert_eq!(cursor_look(Underline, false, false, opts), Some(Underline));
        assert_eq!(cursor_look(Beam, false, false, blur), Some(HollowBlock));
        assert_eq!(cursor_look(Beam, false, false, no_shapes), Some(Block));
        assert_eq!(cursor_look(Hidden, false, false, opts), None);
        // Copy mode's cursor is a block, whatever the program asked for.
        assert_eq!(cursor_look(Beam, true, true, blur), Some(Block));
        // A blinking cursor hides in the off half, unless it's hollow.
        let off_phase = RenderOpts {
            blink_on: false,
            ..opts
        };
        assert_eq!(cursor_look(Block, true, false, off_phase), None);
        assert_eq!(cursor_look(Block, false, false, off_phase), Some(Block));
        assert_eq!(
            cursor_look(
                Block,
                true,
                false,
                RenderOpts {
                    focused: false,
                    ..off_phase
                }
            ),
            Some(HollowBlock)
        );

        // And on screen: a bar covers less of the cell than a block.
        let mut pty = PtyTerm::new(10, 2, DEFAULT_SCROLLBACK);
        let block = lit(&cell(&paint_pty(&pty, opts), 0));
        pty.feed(b"\x1b[6 q"); // steady bar
        let bar = lit(&cell(&paint_pty(&pty, opts), 0));
        assert!(bar > 0 && bar < block, "bar {bar} block {block}");
    }

    #[test]
    fn programs_choose_whether_the_cursor_blinks() {
        let mut pty = PtyTerm::new(10, 2, DEFAULT_SCROLLBACK);
        assert!(!pty.cursor_blinking());
        pty.set_cursor_blink(true);
        assert!(pty.cursor_blinking(), "the setting's default");
        pty.feed(b"\x1b[2 q"); // steady block
        assert!(!pty.cursor_blinking());
        pty.set_cursor_blink(false);
        pty.feed(b"\x1b[5 q"); // blinking bar
        assert!(pty.cursor_blinking());
        // Copy mode never blinks.
        pty.toggle_vi();
        assert!(!pty.cursor_blinking());
    }

    #[test]
    fn colour_and_size_queries_are_answered() {
        let st = TermStyle::default();
        let mut pty = PtyTerm::new(80, 24, DEFAULT_SCROLLBACK);
        let reply = |pty: &mut PtyTerm, q: &[u8]| {
            pty.feed(q);
            String::from_utf8(pty.take_reply(&st, (9, 18))).unwrap()
        };
        let hex = |(r, g, b): Rgb| format!("rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}");
        assert_eq!(
            reply(&mut pty, b"\x1b]11;?\x1b\\"),
            format!("\x1b]11;{}\x1b\\", hex(st.bg))
        );
        assert_eq!(
            reply(&mut pty, b"\x1b]10;?\x07"),
            format!("\x1b]10;{}\x07", hex(st.fg))
        );
        assert_eq!(
            reply(&mut pty, b"\x1b]4;1;?\x07"),
            format!("\x1b]4;1;{}\x07", hex(st.ansi[1]))
        );
        // A colour the program set is the one it gets back.
        pty.feed(b"\x1b]11;rgb:12/34/56\x07");
        assert_eq!(
            reply(&mut pty, b"\x1b]11;?\x07"),
            "\x1b]11;rgb:1212/3434/5656\x07"
        );
        // Text area in pixels, and replies keep the order they were asked in.
        assert_eq!(
            reply(&mut pty, b"\x1b[14t\x1b[6n"),
            "\x1b[4;432;720t\x1b[1;1R"
        );
    }

    #[test]
    fn clipboard_copies_and_the_bell_are_reported_once() {
        let mut pty = PtyTerm::new(10, 2, DEFAULT_SCROLLBACK);
        pty.feed(b"\x1b]52;c;aGVsbG8=\x07"); // "hello"
        assert_eq!(pty.take_clipboard().as_deref(), Some("hello"));
        assert_eq!(pty.take_clipboard(), None);
        // A request to READ the clipboard is refused (OSC 52 copy-only).
        pty.feed(b"\x1b]52;c;?\x07");
        assert!(pty.take_reply(&TermStyle::default(), (9, 18)).is_empty());
        assert!(!pty.take_bell());
        pty.feed(b"\x07");
        assert!(pty.take_bell());
        assert!(!pty.take_bell());
    }

    #[test]
    fn mouse_modes_and_alternate_scroll_follow_the_program() {
        let mut pty = PtyTerm::new(10, 2, DEFAULT_SCROLLBACK);
        assert_eq!(pty.mouse_report(), None);
        assert!(
            !pty.alternate_scroll(),
            "the main screen scrolls its scrollback"
        );
        pty.feed(b"\x1b[?1049h"); // alternate screen (less, man)
        assert!(pty.alternate_scroll());
        pty.feed(b"\x1b[?1000h");
        assert_eq!(
            pty.mouse_report(),
            Some(MouseReport {
                drag: false,
                motion: false,
                encoding: MouseEncoding::X10
            })
        );
        assert!(
            !pty.alternate_scroll(),
            "a program reading the mouse gets the wheel"
        );
        pty.feed(b"\x1b[?1002h\x1b[?1006h");
        let r = pty.mouse_report().unwrap();
        assert!(r.drag && !r.motion && r.encoding == MouseEncoding::Sgr);
        pty.feed(b"\x1b[?1003h");
        assert!(pty.mouse_report().unwrap().motion);
    }

    fn urls(pty: &PtyTerm) -> Vec<String> {
        pty.visible_links().into_iter().map(|l| l.url).collect()
    }

    #[test]
    fn web_addresses_in_output_are_links_without_the_sentence_around_them() {
        let mut pty = PtyTerm::new(80, 4, DEFAULT_SCROLLBACK);
        pty.feed(
            b"docs (see https://example.com/a_(b)). or www.rust-lang.org, not file:///c/x\r\n",
        );
        assert_eq!(
            urls(&pty),
            ["https://example.com/a_(b)", "www.rust-lang.org"]
        );
        // The link under a cell: column 12 is inside the first address.
        let link = pty.link_at(12, 0).expect("a link at col 12");
        assert_eq!(link.url, "https://example.com/a_(b)");
        assert_eq!(pty.viewport_cell(link.start), Some((10, 0)));
        assert_eq!(pty.viewport_cell(link.end), Some((34, 0)));
        assert_eq!(pty.link_at(2, 0), None);
    }

    #[test]
    fn a_wrapped_address_is_one_link() {
        let mut pty = PtyTerm::new(20, 4, DEFAULT_SCROLLBACK);
        pty.feed(b"https://example.com/a/long/path ok");
        assert_eq!(urls(&pty), ["https://example.com/a/long/path"]);
        // Clicking its second line finds it too.
        assert!(pty.link_at(3, 1).is_some());
    }

    #[test]
    fn hyperlinks_programs_mark_are_links() {
        let mut pty = PtyTerm::new(40, 4, DEFAULT_SCROLLBACK);
        // OSC 8 around "click"; a web address as the visible text isn't counted twice.
        pty.feed(b"\x1b]8;;https://x.org/p\x1b\\click\x1b]8;;\x1b\\ \x1b]8;;https://y.org\x07https://y.org\x1b]8;;\x07\r\n");
        let links = pty.visible_links();
        assert_eq!(links.len(), 2, "{links:?}");
        assert_eq!(links[0].url, "https://x.org/p");
        assert_eq!(pty.viewport_cell(links[0].end), Some((4, 0)));
        assert_eq!(links[1].url, "https://y.org");
    }

    #[test]
    fn only_web_addresses_open() {
        assert_eq!(openable_url("https://a.b").as_deref(), Some("https://a.b"));
        assert_eq!(openable_url("HTTP://a.b").as_deref(), Some("HTTP://a.b"));
        assert_eq!(openable_url("www.a.b").as_deref(), Some("https://www.a.b"));
        assert_eq!(openable_url("file://host/c/x.txt"), None);
        assert_eq!(openable_url("mailto:a@b.c"), None);
    }

    #[test]
    fn trimming_keeps_brackets_that_belong_to_the_address() {
        assert_eq!(trim_url("https://a.b/x."), "https://a.b/x");
        assert_eq!(trim_url("https://a.b/x)."), "https://a.b/x");
        assert_eq!(trim_url("https://w.org/A_(b)"), "https://w.org/A_(b)");
        assert_eq!(trim_url("https://a.b/?q=1&r=2"), "https://a.b/?q=1&r=2");
    }

    /// 30 numbered lines, every third saying "Error".
    fn log_term() -> PtyTerm {
        let mut pty = PtyTerm::new(30, 5, DEFAULT_SCROLLBACK);
        for i in 0..30 {
            let word = if i % 3 == 0 { "Error" } else { "fine" };
            pty.feed(format!("{i:02} {word}\r\n").as_bytes());
        }
        pty
    }

    #[test]
    fn search_covers_the_scrollback_newest_first() {
        let mut pty = log_term();
        assert_eq!(
            pty.search_set("error"),
            10,
            "ignores case, sees all 30 lines"
        );
        assert_eq!(pty.search_position(), Some((1, 10)), "starts at the newest");
        // `n` goes further back.
        pty.search_step(true);
        assert_eq!(pty.search_position(), Some((2, 10)));
        pty.search_step(false);
        pty.search_step(false);
        assert_eq!(pty.search_position(), Some((10, 10)), "wraps to the oldest");
        // Revealing the oldest scrolls the view up to it.
        pty.search_reveal();
        assert!(pty.scrolled_back());
        // Matched literally: "e.r" isn't a pattern.
        assert_eq!(pty.search_set("e.r"), 0);
        pty.search_clear();
        assert_eq!(pty.search_position(), None);
    }

    #[test]
    fn search_from_copy_mode_starts_above_the_cursor() {
        let mut pty = log_term();
        pty.toggle_vi();
        // Up past the newest "Error" (line 27, four rows up: 27, 28, 29, prompt).
        for _ in 0..5 {
            pty.vi_motion(ViMotion::Up);
        }
        pty.search_set("error");
        assert_eq!(pty.search_position(), Some((2, 10)));
        pty.search_reveal();
        assert_eq!(pty.selection_text(), None);
        let cursor = pty.vt.vi_mode_cursor.point;
        assert_eq!(
            pty.vt
                .bounds_to_string(cursor, Point::new(cursor.line, Column(7))),
            "Error"
        );
    }

    #[test]
    fn new_output_moves_the_matches_with_it() {
        let mut pty = log_term();
        pty.search_set("error");
        pty.feed(b"30 Error\r\n31 fine\r\n");
        pty.search_refresh();
        // The new one is counted, and the current match stays on the same text.
        assert_eq!(pty.search_position(), Some((2, 11)));
    }

    #[test]
    fn matches_and_the_hovered_link_are_drawn() {
        let mut pty = PtyTerm::new(10, 2, DEFAULT_SCROLLBACK);
        pty.feed(b"ab ab\x1b[?25l");
        let plain = paint_pty(&pty, RenderOpts::default());
        pty.search_set("ab");
        let found = paint_pty(&pty, RenderOpts::default());
        let has = |buf: &[u32], c: Rgb| {
            let mut px = [0u32];
            draw::fill_rect(&mut px, 1, 1, 0, 0, 1, 1, c);
            buf.contains(&px[0])
        };
        assert!(!has(&plain, draw::FIND) && !has(&plain, draw::FIND_CUR));
        assert!(has(&found, draw::FIND) && has(&found, draw::FIND_CUR));
        pty.search_clear();
        pty.link_hover = Some((
            Point::new(Line(0), Column(0)),
            Point::new(Line(0), Column(1)),
        ));
        assert_ne!(paint_pty(&pty, RenderOpts::default()), plain, "underlined");
    }

    #[test]
    fn shell_marks_parse_in_both_dialects() {
        assert_eq!(osc_mark(b"133;A"), Some(Mark::PromptStart));
        assert_eq!(osc_mark(b"633;B"), Some(Mark::InputStart));
        assert_eq!(osc_mark(b"133;C"), Some(Mark::OutputStart));
        assert_eq!(osc_mark(b"133;D;2"), Some(Mark::Finished(Some(2))));
        assert_eq!(osc_mark(b"133;D"), Some(Mark::Finished(None)));
        assert_eq!(osc_mark(b"133;D;0;aid=7"), Some(Mark::Finished(Some(0))));
        assert_eq!(osc_mark(b"633;E;ls"), None);
        assert_eq!(osc_mark(b"9;9;C:\\x"), None);
    }

    /// A PowerShell-style session: prompts marked A/B, Enter starts the command,
    /// the next prompt reports the exit code.
    fn session() -> PtyTerm {
        let mut pty = PtyTerm::new(40, 6, DEFAULT_SCROLLBACK);
        let prompt = |pty: &mut PtyTerm, exit: Option<i32>| {
            let done = exit.map_or(String::new(), |c| format!("\x1b]133;D;{c}\x07"));
            pty.feed(format!("{done}\x1b]133;A\x07PS C:\\> \x1b]133;B\x07").as_bytes());
        };
        prompt(&mut pty, None);
        pty.feed(b"dir");
        pty.command_entered();
        pty.feed(b"\r\na.txt\r\nb.txt\r\n");
        prompt(&mut pty, Some(0));
        pty.feed(b"bad");
        pty.command_entered();
        pty.feed(b"\r\nnot found\r\n");
        prompt(&mut pty, Some(1));
        pty
    }

    #[test]
    fn prompts_are_found_and_copy_mode_jumps_between_them() {
        let mut pty = session();
        assert!(pty.shell_integrated());
        let lines: Vec<i32> = pty.prompt_spans().iter().map(|s| s.1 .0).collect();
        assert_eq!(lines.len(), 3, "{lines:?}");
        pty.toggle_vi();
        let at = |pty: &PtyTerm| pty.vt.vi_mode_cursor.point.line.0;
        // From the live prompt, back to the second, then the first, then nowhere.
        assert!(pty.vi_prompt_jump(true));
        assert_eq!(at(&pty), lines[1]);
        assert!(pty.vi_prompt_jump(true));
        assert_eq!(at(&pty), lines[0]);
        assert!(!pty.vi_prompt_jump(true));
        assert!(pty.vi_prompt_jump(false));
        assert_eq!(at(&pty), lines[1]);
    }

    #[test]
    fn a_commands_output_is_what_follows_its_prompt() {
        let pty = session();
        let first = pty.prompt_spans()[0].1;
        let out = pty.command_output_at(first).unwrap();
        assert_eq!(out.command, "dir");
        assert_eq!(out.output, "a.txt\nb.txt");
        // From inside the output, it's still that command's.
        let out = pty.command_output_at(first + 2).unwrap();
        assert_eq!(out.command, "dir");
        let second = pty.prompt_spans()[1].1;
        assert_eq!(pty.command_output_at(second).unwrap().output, "not found");
    }

    #[test]
    fn a_shell_that_marks_its_command_has_it_copied_exactly() {
        let mut pty = PtyTerm::new(40, 6, DEFAULT_SCROLLBACK);
        pty.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\x1b]133;C\x07\r\nhi\r\n\x1b]133;D;0\x07");
        let done = pty.take_commands_done();
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].exit, Some(0));
        let out = pty.command_output_at(pty.prompt_spans()[0].1).unwrap();
        assert_eq!(
            (out.command.as_str(), out.output.as_str()),
            ("echo hi", "hi")
        );
    }

    #[test]
    fn finished_commands_are_reported_with_their_exit_codes() {
        let mut pty = session();
        let done = pty.take_commands_done();
        assert_eq!(
            done.iter().map(|d| d.exit).collect::<Vec<_>>(),
            [Some(0), Some(1)]
        );
        assert!(pty.take_commands_done().is_empty(), "once");
        // A prompt without a command typed (empty Enter) reports nothing.
        pty.feed(b"\x1b]133;D\x07\x1b]133;A\x07PS> \x1b]133;B\x07");
        assert!(pty.take_commands_done().is_empty());
    }

    #[test]
    fn a_failed_command_gets_a_mark_in_the_gutter() {
        let pty = session();
        let exits: Vec<Option<i32>> = pty.visible_prompt_exits().iter().map(|e| e.1).collect();
        assert_eq!(exits, [Some(0), Some(1), None]);
        let (w, h) = (500usize, 200usize);
        let p = Painter::new(15.0).unwrap();
        let st = TermStyle::default();
        let draw = |marks: bool| {
            let mut buf = vec![0u32; w * h];
            let opts = RenderOpts {
                shell_marks: marks,
                ..RenderOpts::default()
            };
            render(&pty, &p, &mut buf, w, h, 4, 0, 10, 20, h as i32, &st, opts);
            buf
        };
        let red = |buf: &[u32]| {
            let mut px = [0u32];
            draw::fill_rect(&mut px, 1, 1, 0, 0, 1, 1, draw::ERR);
            (0..h).filter(|y| buf[y * w + 2] == px[0]).count()
        };
        // One prompt row (20px) failed.
        assert_eq!(red(&draw(true)), 20);
        assert_eq!(red(&draw(false)), 0);
    }

    #[test]
    fn prompt_tags_survive_scrolling_and_are_not_links() {
        let mut pty = session();
        assert!(pty.visible_links().is_empty(), "tags aren't links");
        for i in 0..200 {
            pty.feed(format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(pty.prompt_spans().len(), 3);
        let first = pty.prompt_spans()[0].1;
        assert_eq!(pty.command_output_at(first).unwrap().output, "a.txt\nb.txt");
    }
}
