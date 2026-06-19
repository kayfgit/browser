//! Engine-free internal pages: the `:error(s)` pager, the `:res` resource
//! monitor, and the `:commands` / `:version` pages, plus their line renderers.

use std::time::Instant;

use crate::tabs::TabContent;
use crate::{procmon, vim, App, Source, Tab};

/// One recorded failure: when it happened, the command that triggered it (if
/// known), and the message. Rendered by `:error` / `:errors`.
pub(crate) struct ErrorEntry {
    /// Local wall-clock time, `HH:MM:SS`.
    pub(crate) time: String,
    /// The command line that raised it (e.g. `:open foo`), if any.
    pub(crate) command: Option<String>,
    pub(crate) message: String,
}

impl App {
    /// Open the `:error` / `:errors` page: render the session error log in an
    /// engine-free, read-only **vim-style** tab so the full text (which may be long,
    /// like the WebView2 HRESULT messages) is readable, navigable, and — crucially —
    /// selectable/yankable without retyping. `all = false` shows just the most recent
    /// error; `all = true` shows every error this session, newest first.
    pub(crate) fn open_error_page(&mut self, all: bool) {
        if self.errors.is_empty() {
            self.set_status("no errors this session");
            return;
        }
        let lines = error_lines(&self.errors, all);
        self.place_tab(
            Tab {
                url: "browser://error".into(),
                nojs: false,
                read: false,
                research: false,
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// `:res` — the browser's whole-tree resource usage (browser.exe + WebView2
    /// engine procs + pty-hosts): memory, CPU%, and disk I/O per process plus a
    /// grand total, in an engine-free vim-style tab. Task Manager won't give this in
    /// one place (it scatters WebView2 under its own group). Auto-refreshes ~1×/sec
    /// and freezes automatically while you're selecting text (so you can copy figures
    /// with vim motions without it shifting).
    pub(crate) fn open_resource_page(&mut self) {
        // No previous sample yet, so the first frame shows memory immediately and
        // CPU/disk fill in on the next refresh.
        self.res_prev.clear();
        self.res_at = Instant::now();
        let lines = self.sample_res_lines();
        if lines.is_empty() {
            self.set_status("resource info unavailable");
            return;
        }
        self.place_tab(
            Tab {
                url: "browser://res".into(),
                nojs: false,
                read: false,
                research: false,
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// Whether the active tab is the `:res` resource monitor.
    pub(crate) fn active_is_res(&self) -> bool {
        self.active.and_then(|i| self.tabs.get(i)).is_some_and(|t| t.url == "browser://res")
    }

    /// If the active tab is the resource monitor, re-sample and update its buffer
    /// **in place** — keeping the cursor, selection, and scroll — so a live refresh
    /// never disturbs navigation/copy. Freezes (skips the update) while a visual
    /// selection is active, so the highlighted text can't shift mid-copy; the
    /// refresh resumes once the selection is cleared (after yank/Esc). Called on the
    /// ~1s tick and on pause/resume.
    pub(crate) fn refresh_res(&mut self) {
        if !self.active_is_res() {
            return;
        }
        let selecting = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim())
            .is_some_and(|b| b.has_selection());
        if selecting {
            return;
        }
        let lines = self.sample_res_lines();
        if let Some(buf) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.vim_mut())
        {
            buf.set_lines(lines);
        }
    }

    /// Take a fresh process-tree sample, fold in CPU%/disk-rate deltas against the
    /// previous sample, format the breakdown, and update `res_prev`/`res_at`.
    pub(crate) fn sample_res_lines(&mut self) -> Vec<String> {
        let sample = procmon::tree_sample();
        if sample.is_empty() {
            return Vec::new();
        }
        let elapsed = self.res_at.elapsed().as_secs_f64();
        let ncores = procmon::cpu_count() as f64;
        let have_prev = !self.res_prev.is_empty() && elapsed > 0.05;

        // Per-process CPU% and disk B/s from the cumulative-counter deltas.
        let rate = |s: &procmon::ProcSample| -> (Option<f64>, Option<f64>) {
            if !have_prev {
                return (None, None);
            }
            match self.res_prev.get(&s.pid) {
                Some(&(pc, pio)) => {
                    let cpu = (s.cpu_100ns.saturating_sub(pc)) as f64
                        / (elapsed * 1e7 * ncores)
                        * 100.0;
                    let disk = (s.io_bytes.saturating_sub(pio)) as f64 / elapsed;
                    (Some(cpu), Some(disk))
                }
                None => (None, None),
            }
        };

        let total_mem: u64 = sample.iter().map(|p| p.working_set).sum();
        let (mut total_cpu, mut total_disk) = (0.0f64, 0.0f64);
        let mut rows = Vec::with_capacity(sample.len());
        for p in &sample {
            let (cpu, disk) = rate(p);
            total_cpu += cpu.unwrap_or(0.0);
            total_disk += disk.unwrap_or(0.0);
            let cpu_s = cpu.map(|c| format!("{c:.1}%")).unwrap_or_else(|| "—".into());
            let disk_s = disk.map(procmon::fmt_rate).unwrap_or_else(|| "—".into());
            rows.push(format!(
                "{:>9}  {:>6}  {:>9}  {:<22} {}",
                procmon::fmt_bytes(p.working_set),
                cpu_s,
                disk_s,
                p.name,
                p.pid
            ));
        }

        let cpu_total = if have_prev { format!("CPU {total_cpu:.1}%") } else { "CPU —".into() };
        let disk_total =
            if have_prev { format!("disk {}", procmon::fmt_rate(total_disk)) } else { "disk —".into() };
        let mut lines = Vec::with_capacity(rows.len() + 5);
        lines.push(format!("browser — {} processes    (live; select to freeze)", sample.len()));
        lines.push(format!("{} · {} · {}", procmon::fmt_bytes(total_mem), cpu_total, disk_total));
        lines.push(String::new());
        lines.push(format!("{:>9}  {:>6}  {:>9}  {:<22} {}", "MEM", "CPU", "DISK", "PROCESS", "PID"));
        lines.extend(rows);

        // Roll the sample forward for the next delta.
        self.res_prev = sample.iter().map(|p| (p.pid, (p.cpu_100ns, p.io_bytes))).collect();
        self.res_at = Instant::now();
        lines
    }

    /// `:version` — build/runtime details in an engine-free vim pager (no WebView2),
    /// so the text is navigable and yankable with the same motions as `:error`/`:res`.
    pub(crate) fn open_version_page(&mut self) {
        self.place_tab(
            Tab {
                url: "browser://version".into(),
                nojs: false,
                read: false,
                research: false,
                content: TabContent::Pager(vim::TextBuffer::new(version_lines())),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// `:history` — the shell's own visited list (most-recent first) in an
    /// engine-free, read-only vim tab, so it's navigable and yankable like
    /// `:error`/`:res`. The full URLs are shown (not the autocomplete short form) so
    /// they can be selected and re-opened. `:history clear` wipes it (see
    /// [`App::perform`]).
    pub(crate) fn open_history_page(&mut self) {
        if self.history.is_empty() {
            self.set_status("no history yet");
            return;
        }
        let lines = history_lines(&self.history);
        self.place_tab(
            Tab {
                url: "browser://history".into(),
                nojs: false,
                read: false,
                research: false,
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// Open an internal HTML page (e.g. `:commands`) in a new tab.
    pub(crate) fn open_local_page(&mut self, label: &str, html: String) {
        match self.build_content_webview(Source::Html(html), false, "") {
            Ok(webview) => {
                self.place_tab(
                    Tab {
                        content: TabContent::Web(webview),
                        url: format!("browser://{label}"),
                        nojs: false,
                        read: false,
                        research: false,
                    },
                    true,
                );
                self.window.set_focus();
                self.clear_status();
            }
            Err(e) => self.set_error(format!("failed to open {label}: {e:#}")),
        }
    }
}

/// Maximum number of past errors kept in the session log (oldest dropped first).
pub(crate) const ERROR_LOG_CAP: usize = 200;

/// Stylesheet for the internal `:commands` / `:version` pages.
const HELP_CSS: &str = "html{background:#1e1e1e;color:#d0d0d0}body{margin:0}\
main{max-width:820px;margin:40px auto;padding:0 22px;\
font:16px/1.6 -apple-system,Segoe UI,Roboto,sans-serif}\
h1{color:#fff;font-size:1.7em;margin:0 0 .2em}h2{color:#6cb6ff;font-size:1.1em;\
margin:1.6em 0 .4em;border-bottom:1px solid #333;padding-bottom:.2em}\
p.sub{color:#888;margin:0 0 1em}table{border-collapse:collapse;width:100%}\
td{padding:3px 10px 3px 0;vertical-align:top}td.k{white-space:nowrap;color:#e6a55e;\
font-family:Consolas,monospace;width:1%}kbd{background:#2a2a2a;border:1px solid #444;\
border-radius:4px;padding:1px 6px;font-family:Consolas,monospace;font-size:.9em;color:#f0f0f0}\
td.d{color:#cfcfcf}";

/// Minimal HTML-escaping for text interpolated into the internal pages.
pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Render rows of (key, description) into a `<table>`, escaping both columns.
pub(crate) fn help_table(rows: &[(&str, &str)]) -> String {
    let mut s = String::from("<table>");
    for (k, d) in rows {
        s.push_str(&format!(
            "<tr><td class=\"k\">{}</td><td class=\"d\">{}</td></tr>",
            html_escape(k),
            html_escape(d)
        ));
    }
    s.push_str("</table>");
    s
}

/// Build the plain-text lines shown by `:error` / `:errors` in the vim pager. Each
/// error becomes a header line (`[HH:MM:SS] :command — error N`) followed by its
/// message (split on newlines), with a blank line between entries. `all = false`
/// renders only the most recent error; `all = true` renders every logged error in
/// chronological order (oldest first, newest last). The text is intentionally flat
/// so vim motions/text-objects work cleanly over it (e.g. `yi(` to grab a
/// `HRESULT(0x…)` token).
pub(crate) fn error_lines(errors: &[ErrorEntry], all: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut emit = |n: usize, e: &ErrorEntry| {
        let cmd = e.command.as_deref().unwrap_or("(no command)");
        out.push(format!("[{}] {} — error {}", e.time, cmd, n + 1));
        for line in e.message.lines() {
            out.push(line.to_string());
        }
        out.push(String::new());
    };
    if all {
        for (n, e) in errors.iter().enumerate() {
            emit(n, e);
        }
    } else if let Some(e) = errors.last() {
        emit(errors.len() - 1, e);
    }
    // Drop the trailing blank so the buffer doesn't end on an empty line.
    if out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

/// Local wall-clock time as `HH:MM:SS`, for stamping logged errors.
#[cfg(windows)]
pub(crate) fn now_hms() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!("{:02}:{:02}:{:02}", st.wHour, st.wMinute, st.wSecond)
}

#[cfg(not(windows))]
pub(crate) fn now_hms() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// The `:commands` page: every keybind and command (not customizable yet).
pub(crate) fn commands_document() -> String {
    let normal = help_table(&[
        (":", "open the command bar"),
        ("o / O", "open a page in THIS tab / in a new tab (prefills “open ” / “open -t ”)"),
        ("j / k", "scroll down / up"),
        ("Ctrl+D / Ctrl+U", "scroll half a page down / up"),
        ("g / G", "jump to top / bottom"),
        ("/", "find in page — type to search live; works on web, read & error tabs"),
        ("n / N", "next / previous match (while a search is active); Esc clears"),
        ("i", "insert mode (passthrough on a terminal tab)"),
        ("f / F", "hint mode — label every link, type the label to follow (F: open in a new tab)"),
        ("v / V", "caret/visual select on read & web tabs — hjkl/w/b move, y yank, Esc exits"),
        ("x", "close the current tab"),
        ("u / Ctrl+Shift+T", "reopen the last closed tab"),
        ("r", "reload the page"),
        ("H / L", "history back / forward"),
        ("n / p", "next / previous tab"),
        ("1 – 9", "jump straight to tab N"),
        ("< / >", "move the current tab left / right"),
        ("Ctrl+W then h/j/k/l", "move focus between split panes"),
        ("Ctrl+W then s / v", "split the pane stacked / side-by-side (also :split / :vsplit)"),
        ("Ctrl+W then c", "close the focused pane"),
        ("Ctrl+V", "passthrough mode (every key to the page)"),
        ("Ctrl +/-/0", "zoom the whole UI in / out / reset"),
    ]);
    let cmdline = help_table(&[
        ("Enter", "run the command"),
        ("Esc / Ctrl+C", "cancel (Ctrl+C copies first if text is selected)"),
        ("Left / Right", "move the caret a character"),
        ("Ctrl+Left / Right", "move the caret a word"),
        ("Home / End", "jump to start / end of line"),
        ("Tab / Ctrl+Right", "accept the autocomplete suggestion (verb, or :open URL from history)"),
        ("Shift+ movement", "extend the selection (with arrows, Ctrl+arrows, Home/End)"),
        ("Ctrl+A", "select the whole line"),
        ("Ctrl+C / Ctrl+X / Ctrl+V", "copy / cut / paste"),
        ("Backspace / Delete", "delete back / forward (or the selection)"),
        ("Ctrl+W · Ctrl/Alt+Backspace", "delete the previous word"),
        ("Ctrl+Delete", "delete the next word"),
        ("Ctrl+U", "delete to the start of the line"),
    ]);
    let modes = help_table(&[
        ("Insert", "type into a field; Esc or click-away leaves, Ctrl+V → passthrough"),
        ("Passthrough", "every key goes to the page; Ctrl+S (or Shift+Esc) leaves"),
        ("Hint", "type a label to follow it (type it UPPERCASE to open in a new tab); Esc cancels"),
        ("Resize / Move", "hjkl to size / reposition the window; Esc finishes"),
    ]);
    let vimpager = help_table(&[
        ("h j k l · arrows", "move the cursor"),
        ("w / b / e", "next / previous / end of word"),
        ("0 / ^ / $", "start / first non-blank / end of line"),
        ("f / t  (F / T)", "jump to / before a char forward (back); ; , repeat"),
        ("gg / G", "top / bottom; Ctrl+D / Ctrl+U half-page"),
        ("v / V", "charwise / linewise visual select"),
        ("y", "yank: the selection, or with a motion (yy, yw, y$, yf), yt;)"),
        ("yiw · yi( · ya\"", "yank inner/around a text object (word, (), {}, [], <>, quotes)"),
    ]);
    let cmds = help_table(&[
        (":open <url|query> · :o", "open in THIS tab (non-URL → search engine); add -t for a new tab"),
        (":tabopen · :t", "open in a new tab (same as :open -t)"),
        (":reopen · :undo", "reopen the last closed tab (also U / Ctrl+Shift+T)"),
        (":research <url|query> · :rs", "lighter browse: JS on, images kept, media/embeds stripped (-t = new tab)"),
        (":edit · :e", "edit the current URL (re-opens in the tab's own mode)"),
        (":y · :yank", "copy the current URL to the clipboard"),
        (":read <url|query>", "engine-free reader (no WebView2) in this tab; -t = new tab; non-URL → search"),
        (":search [name|template]", "show/set the search engine — a name (ddg/google/wiki…) or a %s URL"),
        (":ai [question]", "AI tab (Groq): i to ask; Normal mode is a vim buffer (v/y select, / find); H/L step through past chats (persisted)"),
        (":model [id]", "show/set the :ai model; in the command bar, Tab cycles the model list; persisted"),
        (":te", "native terminal (Ctrl+V pastes · Ctrl+S → vim copy-mode: navigate/yank, i resumes)"),
        (":te <command>", "run a local command, result in the command bar"),
        (":shell <program>", "set the terminal shell (e.g. :shell nu, :shell bash)"),
        (":js", "toggle JavaScript (reloads this tab; applies to new tabs)"),
        (":nojs <url>", "open a single page with JavaScript disabled"),
        (":ads · :adblock", "toggle the blocker: ads, trackers, forced redirects + popunders (on by default)"),
        (":downloads · :dl", "allow executable/installer downloads (.exe/.msi…; blocked by default)"),
        (":mute · :audio", "toggle muting all page audio/video (live, all tabs)"),
        (":css", "toggle page styling off/on (live, all tabs)"),
        (":close · :bd", "close the current tab"),
        (":vsplit · :split", "split into tmux-style panes (Ctrl+W h/j/k/l to move between them)"),
        (":reload · :r", "reload"),
        (":tabnext · :tn · :tabprev · :tp", "switch tabs"),
        (":back · :forward", "history navigation"),
        (":f · :fullscreen", "toggle fullscreen (hides the bars; `:` brings them back). YouTube's fullscreen button does this too"),
        (":resize · :move", "window-control modes (then hjkl, Esc)"),
        (":error · :err", "latest error in a read-only vim tab (v/y to select & copy)"),
        (":errors · :errs", "every error this session (newest first), same vim tab"),
        (":res · :resources", "live memory/CPU/disk across the whole browser tree (freezes while you select)"),
        (":history · :hist", "visited URLs in a vim tab (v/y to select & open); :history clear wipes it"),
        (":commands · :help", "this page"),
        (":version", "version and build information"),
        (":w · :write", "save the current session (open tabs + UI state) to disk"),
        (":wq · :x", "save the session, then quit"),
        (":quit · :q", "quit WITHOUT saving (the last :w'd session is kept)"),
    ]);
    // Actions: the unified action layer (`actions.rs`) — the same described,
    // invokable operations the `:ai` assistant will drive. Rendered from the one
    // registry so help and AI never drift.
    let action_rows = crate::actions::help_rows();
    let actions =
        help_table(&action_rows.iter().map(|(k, d)| (k.as_str(), d.as_str())).collect::<Vec<_>>());
    // Bangs: build `!key → description` rows from the core table.
    let bang_rows: Vec<(String, &str)> =
        browser_core::bang_list().into_iter().map(|(k, d)| (format!("!{k} <query>"), d)).collect();
    let bangs = help_table(
        &bang_rows.iter().map(|(k, d)| (k.as_str(), *d)).collect::<Vec<_>>(),
    );
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>commands</title><style>{HELP_CSS}</style></head><body><main>\
         <h1>Commands &amp; keybindings</h1>\
         <p class=\"sub\">Not customizable yet — these are the built-in bindings.</p>\
         <h2>Normal mode</h2>{normal}\
         <h2>Command-line editing</h2>{cmdline}\
         <h2>Other modes</h2>{modes}\
         <h2>Vim pager (:error · :errors · :res · :version · read-mode v/V)</h2>{vimpager}\
         <h2>Commands</h2>{cmds}\
         <h2>Actions</h2>\
         <p class=\"sub\">The data/customization layer — one described verb each, and \
         what the <code>:ai</code> assistant will drive (\u{201C}wipe my cookies\u{201D}). \
         Cookies/cache need a page open.</p>{actions}\
         <h2>Bangs</h2>\
         <p class=\"sub\">A <code>!key</code> token in any open/search target jumps to that \
         site's search (no query → the site's home). Trailing form works too: \
         <code>dragon scimitar !osrs</code>.</p>{bangs}\
         <h2>Quick maths</h2>\
         <p class=\"sub\">Type an arithmetic expression in the command bar \
         (<code>+ - * / %  ^</code>, parentheses) to see the result live, e.g. \
         <code>:20*8</code> → <code>= 160</code>. Press Enter to replace the line with the \
         result so you can copy it or keep calculating (<code>160+10</code>).</p>\
         </main></body></html>"
    )
}

/// Plain-text lines for the `:history` vim pager: a header plus the visited URLs,
/// most-recent first, one per line (full URLs so they stay selectable/openable).
pub(crate) fn history_lines(history: &[String]) -> Vec<String> {
    let mut lines = Vec::with_capacity(history.len() + 2);
    lines.push(format!("history — {} entries    (:clear history to wipe)", history.len()));
    lines.push(String::new());
    lines.extend(history.iter().cloned());
    lines
}

/// The `:version` page: build/runtime details about this browser.
/// Plain-text lines for the `:version` pager (navigable/yankable with vim motions).
pub(crate) fn version_lines() -> Vec<String> {
    let kv = [
        ("Name", env!("CARGO_PKG_NAME")),
        ("Version", env!("CARGO_PKG_VERSION")),
        ("Description", env!("CARGO_PKG_DESCRIPTION")),
        ("Authors", env!("CARGO_PKG_AUTHORS")),
        ("Engine", "WebView2 (Chromium) via wry 0.55 — loaded on demand"),
        ("Windowing", "tao 0.35 + softbuffer/fontdue native chrome"),
        ("Terminal", "native alacritty_terminal VT engine + a browser-pty-host companion (ConPTY)"),
        ("Platform", std::env::consts::OS),
        ("Architecture", std::env::consts::ARCH),
    ];
    let mut lines = vec![
        format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")),
        String::new(),
    ];
    for (k, v) in kv {
        lines.push(format!("  {:<14}{}", format!("{k}:"), v));
    }
    lines.push(String::new());
    lines.push("A modal, mode-dispatching browser — only what's needed, when needed.".into());
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(time: &str, command: Option<&str>, message: &str) -> ErrorEntry {
        ErrorEntry {
            time: time.into(),
            command: command.map(Into::into),
            message: message.into(),
        }
    }

    #[test]
    fn latest_error_has_header_then_message_lines() {
        let errs = vec![
            entry("00:00:01", Some(":open a"), "boom"),
            entry("00:00:02", None, "bad\nthings"),
        ];
        let lines = error_lines(&errs, false);
        assert_eq!(lines[0], "[00:00:02] (no command) — error 2");
        assert_eq!(&lines[1..], &["bad".to_string(), "things".to_string()]);
    }

    #[test]
    fn history_lines_keep_order_with_a_count_header() {
        let h = vec!["https://a.test/".to_string(), "https://b.test/x".to_string()];
        let lines = history_lines(&h);
        assert!(lines[0].starts_with("history — 2 entries"));
        assert_eq!(lines[1], ""); // blank under the header
        assert_eq!(&lines[2..], &["https://a.test/".to_string(), "https://b.test/x".to_string()]);
    }

    #[test]
    fn all_errors_are_oldest_first_with_command_and_time() {
        let errs = vec![entry("00:00:01", Some(":open a"), "e1"), entry("00:00:09", Some(":bad"), "e2")];
        let lines = error_lines(&errs, true);
        assert_eq!(lines[0], "[00:00:01] :open a — error 1");
        assert_eq!(lines[1], "e1");
        assert_eq!(lines[2], ""); // blank separator
        assert_eq!(lines[3], "[00:00:09] :bad — error 2");
        assert_eq!(lines[4], "e2");
    }
}
