//! The `:` command dispatcher: verb parsing + execution, target resolution
//! (bangs / queries / URLs), and the `:search` template resolver.

use crate::panes::SplitDir;
use crate::{clipboard_set, commands_document, parse_tab_flag, program_exists, App, ModeKind};

/// Command verbs offered by command-bar autocomplete (`:ver`→`:version`). Longest-
/// useful canonical spellings; ordered so the first prefix match is the best one.
pub(crate) const COMMANDS: &[&str] = &[
    "open", "tabopen", "edit", "yank", "read", "research", "reload", "resize", "res", "resources", "reopen", "ai",
    "error", "errors", "te", "term", "shell", "search", "js", "nojs", "ads", "adblock", "downloads",
    "popups", "pops", "mute", "audio", "css", "model", "history", "clear", "next", "tabnext", "tabprev",
    "prev", "back", "forward", "fullscreen", "move", "commands", "help", "version", "close",
    "vsplit", "split", "write", "wq", "quit",
];

impl App {
    pub(crate) fn run_command(&mut self, line: &str) {
        let line = line.trim();
        // Attribute any failure raised below to this command in the error log.
        self.current_command = if line.is_empty() { None } else { Some(format!(":{line}")) };
        let (verb, rest) = match line.split_once(char::is_whitespace) {
            Some((v, r)) => (v, r.trim()),
            None => (line, ""),
        };
        match verb {
            // `:open`/`:o` open in THIS tab (replacing it); a leading `-t` opens a new
            // tab instead. `:tabopen`/`:t` always open a new tab (that's their meaning).
            "open" | "o" => {
                let (new_tab, rest) = parse_tab_flag(rest);
                self.open_tab(rest, self.nojs, new_tab);
            }
            "tabopen" | "t" => self.open_tab(rest, self.nojs, true),
            // Edit the current URL: drop into the command bar pre-filled with
            // `<verb> <current url>` so you can tweak and re-open it — `<verb>` is the
            // mode the tab was opened in (open/research/read/nojs) so e.g. editing a
            // `:research` tab re-opens with `:research`, not `:open`. Reads the LIVE
            // document URL (so in-page/SPA navigation gives the real address).
            "edit" | "e" => match self.current_url() {
                Some(url) => {
                    let verb = self.active_reopen_verb();
                    self.enter_command(&format!("{verb} {url}"));
                }
                None => self.set_status("no page to edit"),
            },
            // Yank (copy) the current URL to the system clipboard.
            "y" | "yank" => match self.current_url() {
                Some(url) => {
                    clipboard_set(&url);
                    self.set_status(format!("yanked {url}"));
                }
                None => self.set_status("no url to yank"),
            },
            "read" => {
                let (new_tab, rest) = parse_tab_flag(rest);
                if rest.is_empty() {
                    self.set_status("usage: :read [-t] <url>");
                } else {
                    // `replace` (open in this tab) is the default; `-t` opens a new tab.
                    self.start_read(rest, !new_tab);
                }
            }
            // Inspect this session's errors in an engine-free, scrollable tab:
            // `:error` shows the most recent one, `:errors` shows them all.
            "error" | "err" => self.open_error_page(false),
            "errors" | "errs" => self.open_error_page(true),
            // Like :open (URL or → search engine) but lighter: JS on, images kept,
            // heavy media/embeds stripped. For "how do I…" / "best way to…" lookups.
            "research" | "rs" => {
                let (new_tab, rest) = parse_tab_flag(rest);
                self.open_research(rest, new_tab);
            }
            "te" | "term" => {
                if rest.is_empty() {
                    self.open_terminal();
                } else {
                    self.run_term(rest);
                }
            }
            // `:ai` opens a native, engine-free AI tab (Groq). `:ai <q>` opens it
            // with the question (and answer) ready; bare `:ai` drops into the field
            // for a quick paste-key-then-ask flow.
            "ai" => self.open_ai_tab(rest),
            // Show or set the Groq model the `:ai` tab uses (persisted).
            "model" => {
                if rest.is_empty() {
                    self.set_status(format!(
                        "model = {}  (try: {} — or any Groq id via :model <id>)",
                        self.ai_model,
                        crate::ai::MODELS.join(", ")
                    ));
                } else {
                    self.set_ai_model(rest);
                }
            }
            "shell" => {
                if rest.is_empty() {
                    self.set_status(format!("shell = {}", self.term_command.join(" ")));
                } else {
                    let parts: Vec<String> = rest.split_whitespace().map(String::from).collect();
                    // Verify the program exists before applying, so a typo doesn't
                    // silently break `:te` (which would fail to spawn the shell).
                    if program_exists(&parts[0]) {
                        self.term_command = parts;
                        self.set_status(format!("shell set to: {}", self.term_command.join(" ")));
                    } else {
                        self.set_error(format!("shell not found on PATH: {}", parts[0]));
                    }
                }
            }
            // Customize the search engine used when `:open <query>` isn't a URL.
            // Accepts a short engine NAME (`:search ddg`, `:search google`, `:search
            // wiki` — looked up in the bang table) OR a full `%s` URL template.
            "search" => {
                if rest.is_empty() {
                    self.set_status(format!("search = {}", self.search_template));
                } else if let Some(t) = search_template_for(rest) {
                    self.search_template = t;
                    self.set_status(format!("search engine set to: {}", self.search_template));
                } else {
                    self.set_error(format!(
                        "unknown engine '{rest}' — use a name (ddg/google/wiki/…) or a %s URL template"
                    ));
                }
            }
            // Toggle JavaScript live (reloads the current tab; applies to new tabs).
            "js" => self.toggle_js(),
            // `:nojs <url>` still opens a one-off JavaScript-disabled tab.
            "nojs" => {
                if rest.is_empty() {
                    self.toggle_js();
                } else {
                    let (new_tab, rest) = parse_tab_flag(rest);
                    self.open_tab(rest, true, new_tab);
                }
            }
            // Reopen the most recently closed tab (also `u` / Ctrl+Shift+T).
            "reopen" | "undo" => self.reopen_closed(),
            // Toggle the uBlock-style content blocker. Applies live to every open web
            // tab (no reload) and is the default for new tabs; persisted in the session.
            // `:ads` is now the single blocker switch: ads, trackers, forced/auto
            // redirects, popunders, and drive-by executable downloads. `:pops`/`:popups`
            // are kept as aliases so old habits don't error.
            "ads" | "adblock" | "pops" | "popups" => self.toggle_adblock(),
            // Allow/deny executable-installer downloads (off by default — blocks
            // drive-by `.exe`/`.msi` installs).
            "downloads" | "dl" => self.toggle_downloads(),
            // Live page-feature toggles (apply instantly to every open web tab).
            "mute" | "audio" => {
                self.mute = !self.mute;
                self.broadcast_toggle("mute", self.mute);
                self.set_status(if self.mute { "audio muted" } else { "audio on" });
                self.window.request_redraw();
            }
            "css" => {
                self.no_css = !self.no_css;
                self.broadcast_toggle("css", self.no_css);
                self.set_status(if self.no_css { "CSS off" } else { "CSS on" });
                self.window.request_redraw();
            }
            "close" | "tabclose" | "bd" => self.close_active(),
            // tmux-style panes: split the focused pane (side-by-side / stacked) into a
            // new blank pane. Navigate between them with Ctrl+W h/j/k/l.
            "vsplit" | "vs" | "vsp" => self.split_pane(SplitDir::Row),
            "split" | "sp" | "hsplit" => self.split_pane(SplitDir::Col),
            // Session is saved explicitly (vim-style): `:w` writes the current tabs +
            // UI state, `:q` quits without saving, `:wq`/`:x` writes then quits.
            "write" | "w" => {
                self.save_session();
                self.set_status("session written");
            }
            "wq" | "x" => {
                self.save_session();
                self.quit = true;
            }
            "quit" | "q" | "q!" => self.quit = true,
            "reload" | "r" => self.reload_active(),
            "next" | "tabnext" | "tn" => self.switch_tab(1),
            "prev" | "tabprev" | "tp" => self.switch_tab(-1),
            "back" => self.history(false),
            "forward" => self.history(true),
            "f" | "fullscreen" => self.toggle_fullscreen(),
            "resize" => {
                self.mode = ModeKind::Resize;
                self.clear_status();
            }
            "move" => {
                self.mode = ModeKind::Move;
                self.clear_status();
            }
            // `:history` opens the visited list as a vim buffer; `:history clear`
            // wipes it (a shortcut for `:clear history`).
            "history" | "hist" => {
                if rest.trim() == "clear" {
                    self.run_action("clear", "history");
                } else {
                    self.open_history_page();
                }
            }
            // `:clear <history|cookies|cache|all>` — the privacy/data actions. This is
            // a single verb (not one per kind) so the command surface stays small; the
            // same actions are what the AI assistant will drive. See `actions.rs`.
            "clear" => self.run_action("clear", rest),
            "commands" | "help" => self.open_local_page("commands", commands_document()),
            "version" => self.open_version_page(),
            // Total the browser's real footprint across its whole process tree
            // (browser.exe + WebView2 engine procs + pty-hosts), which Task Manager
            // scatters under a separate "WebView2 Manager" group.
            "res" | "resources" => self.open_resource_page(),
            "" => {}
            // A bare bang (`:!yt cats`) opens the whole line as a bang target (in
            // this tab, like `:open`).
            other if other.starts_with('!') => self.open_tab(line, self.nojs, false),
            other => self.set_error(format!("unknown command: {other}")),
        }
        self.current_command = None;
    }

    /// Turn a command-bar target into a URL the way `:open` does: a bare query
    /// (spaces, or no scheme/dot like `rustlang`) — or anything that won't parse as
    /// a URL — goes to the configured search engine; a real address opens directly.
    pub(crate) fn resolve_target(&self, target: &str) -> String {
        // DuckDuckGo-style bangs (`!yt cats`, `!osrs dragon`) take priority: they
        // redirect to a specific site's search regardless of the default engine.
        if let Some(url) = browser_core::expand_bang(target) {
            return url;
        }
        if browser_core::looks_like_query(target) {
            browser_core::search_url(&self.search_template, target)
        } else {
            browser_core::normalize_url(target)
                .unwrap_or_else(|| browser_core::search_url(&self.search_template, target))
        }
    }
}

/// Resolve a `:search` argument into a search-URL template: a bare engine name
/// (looked up in the bang table — `ddg`, `google`, `wiki`, `yt`, …) maps to its
/// template; anything containing `%s` or `://` is taken as a literal template.
/// `None` for an unrecognized name with no `%s`.
pub(crate) fn search_template_for(arg: &str) -> Option<String> {
    let a = arg.trim();
    if a.contains("%s") || a.contains("://") {
        return Some(a.to_string());
    }
    browser_core::bang_search_template(a).map(|t| t.to_string())
}
