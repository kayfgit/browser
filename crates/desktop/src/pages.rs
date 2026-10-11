//! Engine-free internal pages: the `:error(s)` pager, the `:res` resource
//! monitor, and the `:commands` / `:version` pages, plus their line renderers.

use std::time::Instant;

use crate::tabs::{TabContent, TabNav};
use crate::{procmon, vim, App, Tab};

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
                id: crate::layout::TabId::new(),
                url: "browser://error".into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
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
                id: crate::layout::TabId::new(),
                url: "browser://res".into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// Whether the active tab is the `:res` resource monitor.
    pub(crate) fn active_is_res(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .is_some_and(|t| t.url == "browser://res")
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
        if let Some(buf) = self
            .active
            .and_then(|i| self.tabs.get_mut(i))
            .and_then(|t| t.vim_mut())
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
                    let cpu =
                        (s.cpu_100ns.saturating_sub(pc)) as f64 / (elapsed * 1e7 * ncores) * 100.0;
                    let disk = (s.io_bytes.saturating_sub(pio)) as f64 / elapsed;
                    (Some(cpu), Some(disk))
                }
                None => (None, None),
            }
        };

        // Engine processes that are copies of this executable get their engine's name,
        // or they'd all read "browser.exe": Trident's sandboxed helpers, and otherwise
        // Servo's content processes.
        let own = std::process::id();
        #[cfg(windows)]
        let trident = crate::engines::trident::helper_pids();
        #[cfg(not(windows))]
        let trident: Vec<u32> = Vec::new();
        let name_of = |p: &procmon::ProcSample| -> String {
            let copy = p.pid != own && p.name.eq_ignore_ascii_case("browser.exe");
            if copy && trident.contains(&p.pid) {
                format!("{} (trident)", p.name)
            } else if copy && cfg!(feature = "servo-engine") {
                format!("{} (servo)", p.name)
            } else {
                p.name.clone()
            }
        };
        let total_mem: u64 = sample.iter().map(|p| p.working_set).sum();
        let (mut total_cpu, mut total_disk) = (0.0f64, 0.0f64);
        let mut rows = Vec::with_capacity(sample.len());
        for p in &sample {
            let (cpu, disk) = rate(p);
            total_cpu += cpu.unwrap_or(0.0);
            total_disk += disk.unwrap_or(0.0);
            let cpu_s = cpu
                .map(|c| format!("{c:.1}%"))
                .unwrap_or_else(|| "—".into());
            let disk_s = disk.map(procmon::fmt_rate).unwrap_or_else(|| "—".into());
            rows.push(format!(
                "{:>9}  {:>6}  {:>9}  {:<22} {}",
                procmon::fmt_bytes(p.working_set),
                cpu_s,
                disk_s,
                name_of(p),
                p.pid
            ));
        }

        let cpu_total = if have_prev {
            format!("CPU {total_cpu:.1}%")
        } else {
            "CPU —".into()
        };
        let disk_total = if have_prev {
            format!("disk {}", procmon::fmt_rate(total_disk))
        } else {
            "disk —".into()
        };
        let mut lines = Vec::with_capacity(rows.len() + 5);
        lines.push(format!(
            "browser — {} processes    (live; select to freeze)",
            sample.len()
        ));
        lines.push(format!(
            "{} · {} · {}",
            procmon::fmt_bytes(total_mem),
            cpu_total,
            disk_total
        ));
        lines.push(String::new());
        lines.push(format!(
            "{:>9}  {:>6}  {:>9}  {:<22} {}",
            "MEM", "CPU", "DISK", "PROCESS", "PID"
        ));
        lines.extend(rows);

        #[cfg(all(windows, feature = "servo-engine"))]
        if let Some(views) = crate::engines::servo::runtime_view_count() {
            lines.push(String::new());
            lines.push(format!("Servo: {views} open views; shared runtime loaded."));
            lines.push(format!(
                "Servo CPU/memory/I/O are included in browser.exe (PID {}).",
                std::process::id()
            ));
            lines.push(
                "The shell and Servo share this process; separate engine totals are unavailable."
                    .into(),
            );
        }

        // Roll the sample forward for the next delta.
        self.res_prev = sample
            .iter()
            .map(|p| (p.pid, (p.cpu_100ns, p.io_bytes)))
            .collect();
        self.res_at = Instant::now();
        lines
    }

    /// `:version` — build/runtime details in an engine-free vim pager (no WebView2),
    /// so the text is navigable and yankable with the same motions as `:error`/`:res`.
    pub(crate) fn open_version_page(&mut self) {
        self.place_tab(
            Tab {
                id: crate::layout::TabId::new(),
                url: "browser://version".into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
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
        if self.visited.is_empty() {
            self.set_status("no history yet");
            return;
        }
        let lines = history_lines(&self.visited);
        self.place_tab(
            Tab {
                id: crate::layout::TabId::new(),
                url: "browser://history".into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// `:extensions` — kick off the async query of the installed browser extensions. The
    /// picker opens once the result lands ([`UserEvent::ExtensionsListed`](crate::app::UserEvent::ExtensionsListed) →
    /// [`show_extensions_page`](Self::show_extensions_page)). Extensions hang off the webview
    /// profile, so this needs a live web engine — it asks you to open a page first if none.
    pub(crate) fn open_extensions_page(&mut self) {
        self.extension_request = self
            .extension_request
            .checked_add(1)
            .expect("extension request space exhausted");
        match self.any_webview() {
            Some(view) => {
                crate::extensions::list(view, self.extension_request, self.proxy.clone());
                self.set_status("loading extensions…");
            }
            None => {
                self.set_status("open a web page first — extensions need a live storage context")
            }
        }
    }

    /// A picker owns its source view and cached items, even after another picker
    /// queries a different provider or storage context.
    pub(crate) fn show_extensions_page(
        &mut self,
        view: browser_engine::ViewId,
        items: Vec<crate::ExtInfo>,
    ) {
        let lines = ext_lines(&items);
        let current = self.active.and_then(|i| self.tabs.get_mut(i));
        if let Some(Tab {
            content:
                TabContent::Extensions {
                    view: target,
                    items: cached,
                    buffer,
                },
            ..
        }) = current
        {
            *target = view;
            *cached = items;
            let cy = buffer.cy;
            buffer.set_lines(lines);
            buffer.anchor = None;
            buffer.cy = cy.min(buffer.lines.len().saturating_sub(1));
            buffer.cx = 0;
        } else {
            let mut tab = Tab::blank();
            tab.url = "browser://extensions".into();
            tab.content = TabContent::Extensions {
                view,
                items,
                buffer: vim::TextBuffer::new(lines),
            };
            // Keep the source pane alive even when this command comes from a split.
            self.place_tab_escaping_split(tab, true);
            self.window.set_focus();
        }
        self.window.request_redraw();
        self.clear_status();
    }

    /// `:commands` / `:help [topic]` — every keybind and command, in a native read tab
    /// (wrapped, coloured, searchable). A topic scrolls to its line.
    pub(crate) fn open_commands_page(&mut self, anchor: Option<&str>) {
        let mut doc = crate::markdown::to_document(&commands_markdown(), "browser://commands");
        doc.title = "Commands & keybindings".into();
        self.show_read_document(doc, false, true);
        let jump = anchor.and_then(help_jump_text);
        if let Some(nr) = self
            .active
            .and_then(|i| self.tabs.get_mut(i))
            .and_then(|t| t.native_mut())
        {
            nr.jump = jump;
        }
    }

    /// `:alias` (no args) — list the defined command aliases in a read-only vim tab,
    /// `:name → expansion` per line (selectable/yankable like the other pagers).
    pub(crate) fn open_alias_page(&mut self) {
        if self.config.aliases.is_empty() {
            self.set_status("no aliases — :alias <name> <command> to add one");
            return;
        }
        let lines = alias_lines(&self.config.aliases);
        self.place_tab(
            Tab {
                id: crate::layout::TabId::new(),
                url: "browser://aliases".into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// `:profiles` (and bare `:profile`) — the saved-profile picker in a vim tab:
    /// `default` and `scratch` first, then every saved profile, with the active one
    /// marked. Enter switches to the row, `d` deletes it. Refreshed in place when it's
    /// already the active tab, so switching from the picker updates the marker.
    pub(crate) fn open_profiles_page(&mut self) {
        let lines = crate::profiles::profile_lines(
            &crate::session::list_profiles(),
            self.profile_label(),
            self.config.scratch_return.as_deref(),
        );
        if self.active_url() == Some("browser://profiles") {
            let cy = self
                .active
                .and_then(|i| self.tabs.get(i))
                .and_then(|t| t.vim())
                .map_or(0, |b| b.cy);
            if let Some(buf) = self
                .active
                .and_then(|i| self.tabs.get_mut(i))
                .and_then(|t| t.vim_mut())
            {
                buf.set_lines(lines);
                buf.anchor = None;
                buf.cy = cy.min(buf.lines.len().saturating_sub(1));
                buf.cx = 0;
            }
            self.window.request_redraw();
            return;
        }
        self.place_tab(
            Tab {
                id: crate::layout::TabId::new(),
                url: "browser://profiles".into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }
}

/// Maximum number of past errors kept in the session log (oldest dropped first).
pub(crate) const ERROR_LOG_CAP: usize = 200;

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
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = secs % 86_400;
    format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
}

/// Local wall-clock as `YYYY-MM-DD HH:MM`, for stamping a saved `:ai` chat's
/// creation time (shown in the `:aihist` picker).
#[cfg(windows)]
pub(crate) fn now_stamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute
    )
}

#[cfg(not(windows))]
pub(crate) fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    let (days, tod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's days-from-civil, inverted: civil date from days since epoch (UTC).
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as i64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        y,
        m,
        d,
        tod / 3600,
        (tod % 3600) / 60
    )
}

/// The `:commands` command table as (anchor key, signature, description) — the
/// anchor key doubles as what `:help <topic>` matches first (any word of the
/// signature matches too, so `:help t` finds ":tabopen · :t").
const CMD_ROWS: &[(&str, &str, &str)] = &[
    ("open", ":open <url|query> · :o", "open in THIS tab (non-URL → search engine); -t = new tab, -n = private, combinable (-tn)"),
    ("tabopen", ":tabopen · :t", "open in a new tab (same as :open -t); -n = private"),
    ("reopen", ":reopen", "reopen the last closed tab (also u / Ctrl+Shift+T)"),
    ("undo", ":undo · :redo", "undo / redo the last layout change — closing a tab or pane, a split, moving, swapping or breaking out a pane, flipping or resizing a split, moving a tab (also U / R); an undone close reopens the page where it was"),
    ("research", ":research <url|query> · :rs", "lighter browse: JS on, images kept, media/embeds stripped (-t = new tab, -n = private)"),
    ("edit", ":edit · :e", "edit the current URL (re-opens in the tab's own mode)"),
    ("yank", ":y · :yank", "copy the current URL to the clipboard (also yy; yf copies a link you pick by hint)"),
    ("read", ":read <url|query>", "engine-free reader (no WebView2) in this tab; -t = new tab; non-URL → search"),
    ("search", ":search [name|template]", "show/set the search engine — a name (ddg/google/wiki…) or a %s URL"),
    ("ai", ":ai [question]", "AI tab (Groq): i to ask; Normal mode is a vim buffer (v/y select, / find); H/L step through past chats (persisted)"),
    ("aihist", ":aihist · :aihistory", "saved-chat picker in a vim tab: each row shows created time · size · name (first prompt); Enter opens that chat, d deletes the row/selection"),
    ("model", ":model [id]", "show/set the :ai model (Tab cycles the model list); persisted"),
    ("te", ":te", "native terminal (Ctrl+V pastes · drag to select and copy, double/triple-click takes a word/line · Ctrl+S → vim copy-mode: hjkl/w/b/f-find navigate, v/y yank, i resumes) · :w/:wq remembers the cwd and restores it — cmd/nushell automatic; pwsh and WSL need a one-line prompt hook (see the user guide, docs/user-guide.md, or ask :ai)"),
    ("terun", ":te <command>", "run a local command, result in the command bar"),
    ("shell", ":shell <program>", "set the terminal shell (e.g. :shell nu, :shell cmd; default: PowerShell)"),
    ("theme", ":theme [key value]", "appearance: no args opens config.toml in an editor (closing it applies); a key + value sets one field (bar_bg, bar_fg, accent, bg, bar_height_pct, term_font, term_font_px, term_scheme, term_bg, term_fg — 'default' resets); :theme install <scheme> downloads a terminal scheme; :theme reload / show"),
    ("engine", ":engine [provider] · :engine default <provider>", "inspect or select the web engine; changes reopen the URL"),
    ("engines", ":engines", "list installed providers and supported view options"),
    ("extensions", ":extensions", "browser-extension picker in a vim tab (Enter toggles one on/off)"),
    ("js", ":js", "toggle JavaScript (reloads this tab; applies to new tabs)"),
    ("nojs", ":nojs <url>", "open a single page with JavaScript disabled"),
    ("adblock", ":ads · :adblock [on|off]", "no args: toggle · on (default): uBlock Origin Lite blocks ads, trackers and YouTube ads, and the built-in guard stops forced redirects and popunders · off: none · the pages on screen reload so the change takes hold"),
    ("default", ":default [remove]", "register browser with Windows so it can be your default browser, then open Settings → Default apps to pick it (Windows doesn't let programs set themselves). Links from other apps open as a new tab in the running window. remove unregisters it"),
    ("downloads", ":downloads · :dl [dir <path>]", "every download asks first (y/Enter saves, n/Esc cancels; programs and installers get a warning) and shows its progress in the status bar · :downloads lists this session's downloads: Enter opens, e shows in folder, d cancels / forgets · dir shows or changes where they're saved (default: your Downloads folder)"),
    ("mute", ":mute · :audio", "toggle muting all page audio/video (live, all tabs)"),
    ("css", ":css", "toggle page styling off/on (live, all tabs)"),
    ("scrollbar", ":scrollbar · :sb", "toggle hiding the pages' scrollbars (live, all tabs)"),
    ("video", ":video [url]", "no url: toggle stripping video/players off pages (live, all tabs); with a url: play it in mpv/vlc"),
    ("close", ":close · :bd", "close the current tab"),
    ("split", ":vsplit · :split · :sp", "split into tmux-style panes (Ctrl+W h/j/k/l to move between them). Note :sp takes no argument — :sp <name> is :saveprofile"),
    ("reload", ":reload · :r", "reload"),
    ("tabnext", ":tabnext · :tn · :tabprev · :tp", "switch tabs"),
    ("back", ":back · :forward", "history navigation"),
    ("fullscreen", ":f · :fullscreen", "toggle fullscreen (hides the bars; `:` brings them back). YouTube's fullscreen button does this too"),
    ("resize", ":resize · :move", "window-control modes (then hjkl, Esc)"),
    ("error", ":error · :err", "latest error in a read-only vim tab (v/y to select & copy)"),
    ("errors", ":errors · :errs", "every error this session (newest first), same vim tab"),
    ("resources", ":res · :resources", "live memory/CPU/disk across the whole browser tree (freezes while you select)"),
    ("freeze", ":freeze · :unfreeze", "stop the web engine to free its memory, keeping every tab; :unfreeze reloads them"),
    ("save", ":save [name] · :favorite · :bookmark", "keep the current page for later, optionally under a short name (re-saving the same page renames it instead of duplicating)"),
    ("saved", ":saved · :favorites", "the saved pages in a vim tab (Enter opens, ⇧Enter new tab, d deletes the line/selection); kept in their own file, so profiles and :restore never touch them"),
    ("unsave", ":unsave <name|url>", "remove a saved page (same as d on its row in :saved)"),
    ("history", ":history · :hist", "visited URLs in a vim tab (Enter opens, ⇧Enter new tab, v/y select, d deletes the line/selection); :history clear wipes it"),
    ("clear", ":clear <what> [period]", "erase data: history/cookies/cache/all, optionally a window (15m/1h/24h/7d); cookies/cache need a page open"),
    ("alias", ":alias [name] [cmd] · :unalias", "list / set / remove command aliases (e.g. :alias gh open github.com → :gh)"),
    ("restore", ":restore", "reset all customization to defaults — also Ctrl+Alt+Shift+R, which works in any mode"),
    ("help", ":commands · :help [topic]", "this page; a topic jumps to its section (e.g. :help theme, :help selection)"),
    ("version", ":version", "version and build information"),
    ("news", ":news · :changelog [-t]", "what changed in each release, newest first (f follows a commit link)"),
    ("saveprofile", ":saveprofile <name> · :sp <name>", "snapshot the open tabs, splits, window and UI state as a named profile — the ONLY thing that writes a profile (:w saves the session, never a profile)"),
    ("profile", ":profile <name> · :p", "load a saved profile (no args: the picker — Enter switches, d deletes); :profile default returns to the live session. What you had open is written to the session first, so it's never lost"),
    ("profiles", ":profiles · :profs", "list the saved profiles in a vim tab (same picker as bare :profile)"),
    ("delprofile", ":delprofile <name> · :dp", "delete a saved profile (the tabs stay open)"),
    ("scratch", ":scratch · :scr · :sc", "clean slate: park everything you have open and start empty; :scratch again brings the parked layout back"),
    ("inspect", ":inspect · :devtools", "open the developer tools for this page (also F12, Ctrl+Shift+I, or right-click → Inspect)"),
    ("source", ":source · :viewsource · :src", "view this page's source in a new tab (also Ctrl+Shift+U, or right-click → View page source)"),
    ("update", ":update · :update install · :update auto|notify|off", "check for a newer version and show what's in it; install installs it now and restarts the browser with your tabs. auto (the default): updates download in the background and install when you quit; notify: only announce them; off: no checks"),
    ("write", ":w · :write", "save the current session (open tabs + UI state) to disk"),
    ("quit", ":quit · :leave · :l", "quit WITHOUT saving (the last :w'd session is kept; :w first to save). :q, :wq and :x don't quit, so a vim habit can't close the browser"),
];

/// Help sections: element id, TOC label, and the extra `:help` aliases that reach
/// them (beyond words already caught by a command row or action name).
const HELP_SECTIONS: &[(&str, &str, &[&str])] = &[
    (
        "sec-normal",
        "Normal mode",
        &[
            "normal",
            "keys",
            "keybinds",
            "keybindings",
            "bindings",
            "keyboard",
            "caret",
            "selection",
            "hints",
            "panes",
        ],
    ),
    (
        "sec-cmdline",
        "Command-line editing",
        &["cmdline", "commandline", "editing", "bar", "commandbar"],
    ),
    (
        "sec-modes",
        "Other modes",
        &["modes", "mode", "passthrough", "hint", "insert"],
    ),
    (
        "sec-pager",
        "Vim pager",
        &["pager", "vim", "vimpager", "visual", "motions"],
    ),
    ("sec-commands", "Commands", &["commands", "command"]),
    ("sec-actions", "AI actions", &["actions", "action"]),
    ("sec-bangs", "Bangs", &["bangs", "bang"]),
    (
        "sec-maths",
        "Quick maths",
        &["maths", "math", "calc", "calculator"],
    ),
];

/// Every `:help <topic>` value worth Tab-cycling in the command bar: command
/// anchors first (the most common jump), then action names and each section's
/// primary alias. All of them resolve through [`help_anchor`].
pub(crate) fn help_topics() -> Vec<String> {
    let mut v: Vec<String> = CMD_ROWS
        .iter()
        .map(|(id, _, _)| (*id).to_string())
        .collect();
    for a in crate::actions::ACTIONS {
        if !v.iter().any(|x| x == a.name) {
            v.push(a.name.to_string());
        }
    }
    for (_, _, names) in HELP_SECTIONS {
        if let Some(n) = names.first() {
            if !v.iter().any(|x| x == n) {
                v.push((*n).to_string());
            }
        }
    }
    v
}

/// Resolve a `:help <topic>` to the element id it should jump to: a command row
/// first (by anchor key, then any word of its signature), then an AI action name,
/// then a section name/alias. `None` means "no such topic" (the caller shows the
/// whole page and says so).
pub(crate) fn help_anchor(topic: &str) -> Option<String> {
    let t = topic.trim().trim_start_matches(':').to_ascii_lowercase();
    if t.is_empty() {
        return None;
    }
    for (id, sig, _) in CMD_ROWS {
        let word_hit = sig
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|w| !w.is_empty() && w.eq_ignore_ascii_case(&t));
        if *id == t || word_hit {
            return Some(format!("cmd-{id}"));
        }
    }
    if crate::actions::ACTIONS.iter().any(|a| a.name == t) {
        return Some(format!("act-{t}"));
    }
    HELP_SECTIONS
        .iter()
        .find(|(_, _, names)| names.contains(&t.as_str()))
        .map(|(id, _, _)| id.to_string())
}

/// Escape `text` for Markdown, leaving `backtick` code spans working: the command
/// descriptions use them, while `<path>` or `*` would otherwise be read as HTML or
/// emphasis.
fn md(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '*' | '_' | '<' | '>' | '[' | ']' | '#' | '|') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A Markdown list of `key — what it does` rows, the key as code.
fn md_rows(rows: &[(&str, &str)]) -> String {
    rows.iter()
        .map(|(k, d)| format!("- `{k}` — {}\n", md(d)))
        .collect()
}

/// The `:commands` page as Markdown, shown in the native read view (wrapped, coloured,
/// `/` to search, `v` to select, `f` to follow a link). `:help <topic>` scrolls to a
/// line of it — see [`help_jump_text`].
pub(crate) fn commands_markdown() -> String {
    let normal = md_rows(&[
        (":", "open the command bar"),
        ("o / O", "open a page in THIS tab / in a new tab (prefills “open ” / “open -t ”)"),
        ("j / k", "scroll down / up"),
        ("h / l", "scroll left / right (web pages)"),
        ("Ctrl+D / Ctrl+U", "scroll half a page down / up"),
        ("gg / G", "jump to top / bottom"),
        ("/", "find in page — type to search live; works on web, read & error tabs"),
        ("n / N", "next / previous match (while a search is active); Esc clears"),
        ("i", "insert mode (passthrough on a terminal tab)"),
        ("f / F", "hint mode — label every link, type the label to follow (F: open in a new tab)"),
        ("yf", "hint mode, but the label COPIES that link's address instead of going there (cyan labels)"),
        ("s", "hint scrollable boxes; pick a label, then hjkl, gg/G, Ctrl+D/U or PgUp/PgDn scroll that box; v selects text inside it; Esc exits"),
        ("yy", "copy this page's address to the clipboard (same as :y)"),
        ("v / V", "selection mode on read & web tabs — hjkl/w/b move, v/V select, y yank, Esc exits"),
        ("x", "close the current tab"),
        ("u / Ctrl+Shift+T", "reopen the last closed tab"),
        ("U / R", "undo / redo the last layout change (close, split, pane move, resize, tab move)"),
        ("r", "reload the page"),
        ("H / L", "history back / forward"),
        ("n / p", "next / previous tab"),
        ("1 – 9", "jump straight to tab N"),
        ("Shift+1 – 9", "jump to tab 10+N (the 11th – 19th)"),
        ("< / >", "move the current tab left / right"),
        ("Ctrl+W then h/j/k/l", "move focus between split panes"),
        ("Ctrl+W then H/J/K/L", "resize the focused pane in that direction"),
        ("Ctrl+W then s / v", "split the pane stacked / side-by-side (also :split / :vsplit)"),
        ("Ctrl+W then c", "close the focused pane"),
        ("Ctrl+V", "passthrough mode (every key to the page)"),
        ("+ / - / )", "zoom the page content in / out / reset (web tabs)"),
        ("Ctrl +/-/0", "zoom the browser UI and terminals in / out / reset"),
    ]);
    let cmdline = md_rows(&[
        ("Enter", "run the command"),
        ("Esc / Ctrl+C", "cancel (Ctrl+C copies first if text is selected)"),
        ("Left / Right", "move the caret a character"),
        ("Ctrl+Left / Right", "move the caret a word"),
        ("Home / End", "jump to start / end of line"),
        ("Tab / Ctrl+Right", "accept the autocomplete suggestion (verb, or :open URL from history); on a fixed-choice argument (:model, :adblock, :clear, :theme, :search, :help, …) Tab / Shift+Tab cycle the choices"),
        ("Shift+ movement", "extend the selection (with arrows, Ctrl+arrows, Home/End)"),
        ("Ctrl+A", "select the whole line"),
        ("Ctrl+C / Ctrl+X / Ctrl+V", "copy / cut / paste"),
        ("Backspace / Delete", "delete back / forward (or the selection)"),
        ("Ctrl+W · Ctrl/Alt+Backspace", "delete the previous word"),
        ("Ctrl+Delete", "delete the next word"),
        ("Ctrl+U", "delete to the start of the line"),
    ]);
    let modes = md_rows(&[
        ("Insert", "i (or clicking / hinting a text field) types into a page field; Esc (which also closes the field's popup), clicking away or navigating leaves"),
        ("Passthrough", "Ctrl+V sends every key to the page and survives clicks and navigation; Ctrl+S or Shift+Esc leaves. On a terminal i enters it; Esc goes to the shell and Ctrl+S leaves"),
        ("Hint", "type a label to follow it (type it UPPERCASE to open in a new tab); entered with yf only links are labelled and the label copies the address; Esc cancels"),
        ("Resize / Move", "hjkl to size / reposition the window; Esc finishes"),
        ("Questions", "a download or a site's permission request asks in the bar: y / Enter yes, n / Esc no"),
    ]);
    let pager = md_rows(&[
        ("h j k l · arrows", "move the cursor"),
        ("w / b / e", "next / previous / end of word"),
        ("0 / ^ / $", "start / first non-blank / end of line"),
        (
            "f / t  (F / T)",
            "jump to / before a char forward (back); ; , repeat",
        ),
        ("gg / G", "top / bottom; Ctrl+D / Ctrl+U half-page"),
        ("v / V", "charwise / linewise visual select"),
        (
            "y",
            "yank: the selection, or with a motion (yy, yw, y$, yf), yt;)",
        ),
        (
            "yiw · yi( · ya\"",
            "yank inner/around a text object (word, (), {}, [], <>, quotes)",
        ),
    ]);
    let commands: String = CMD_ROWS
        .iter()
        .map(|(_, sig, d)| format!("- `{sig}` — {}\n", md(d)))
        .collect();
    // Actions come straight from the one registry, so help and the AI never drift.
    let actions: String = crate::actions::ACTIONS
        .iter()
        .map(|a| format!("- `{}` — {}\n", action_signature(a), md(a.summary)))
        .collect();
    format!(
        "Not customizable yet — these are the built-in bindings. `:help <topic>` jumps \
         straight to a command, action or section, e.g. `:help theme`, `:help selection`, \
         `:help bangs`. `/` searches this page.\n\n\
         ## Normal mode\n\n{normal}\n\
         ## Command-line editing\n\n{cmdline}\n\
         ## Other modes\n\n{modes}\n\
         ## Vim pager\n\n\
         The engine-free pages (`:errors`, `:res`, `:history`, `:version`, …) and `v` on a \
         read page.\n\n{pager}\n\
         ## Commands\n\n{commands}\n\
         ## AI actions\n\n\
         Operations the `:ai` assistant can perform on request — e.g. “open github and gmail \
         side by side”, “wipe my cookies”, “make ‘gh’ open github”. Several map to the \
         commands above; `:restore` (or Ctrl+Alt+Shift+R) resets all customization.\n\n\
         {actions}\n\
         ## Bangs\n\n\
         A `!key` token in any open/search target jumps to that site's search (no query → \
         the site's home). Trailing form works too: `dragon scimitar !osrs`. The bangs are \
         the 13,000+ of [Kagi's list](https://github.com/kagisearch/bangs) (the ones Helium \
         uses): `:bangs` lists them all (`/` searches the list), `:bangs <word>` filters by \
         key or name, and `:bang <key>` says what one does. `:bang <key> <url>` adds your \
         own, with `%s` where the search goes (`:bang rs https://runescape.wiki/?search=%s`); \
         it wins over Kagi's for the same key. `:unbang <key>` removes one (yours, or \
         switches off Kagi's) and `:resetbangs [key]` brings it back, or every bang with no \
         key.\n\n\
         ## Quick maths\n\n\
         Type an arithmetic expression in the command bar (`+ - * / % ^`, parentheses) to \
         see the result live, e.g. `:20*8` → `= 160`. Press Enter to replace the line with \
         the result so you can copy it or keep calculating (`160+10`).\n"
    )
}

/// An AI action's signature: its name, then each parameter — `<required>` or
/// `[optional]`, a fixed set of values written `a|b|c`.
fn action_signature(a: &crate::actions::ActionSpec) -> String {
    let mut sig = a.name.to_string();
    for p in a.params {
        let slot = if p.values.is_empty() {
            p.name.to_string()
        } else {
            p.values.join("|")
        };
        if p.required {
            sig.push_str(&format!(" <{slot}>"));
        } else {
            sig.push_str(&format!(" [{slot}]"));
        }
    }
    sig
}

/// The text of the `:commands` line a [`help_anchor`] id points at, for scrolling there.
pub(crate) fn help_jump_text(anchor: &str) -> Option<String> {
    if let Some(id) = anchor.strip_prefix("cmd-") {
        return CMD_ROWS
            .iter()
            .find(|(row, _, _)| *row == id)
            .map(|(_, sig, _)| sig.chars().take(24).collect());
    }
    if let Some(name) = anchor.strip_prefix("act-") {
        return crate::actions::ACTIONS
            .iter()
            .find(|a| a.name == name)
            .map(|a| action_signature(a).chars().take(24).collect());
    }
    HELP_SECTIONS
        .iter()
        .find(|(id, _, _)| *id == anchor)
        .map(|(_, label, _)| (*label).to_string())
}

/// The `:extensions` picker body: a header plus one row per installed extension —
/// `[on ]`/`[off]` state, name, then the extension id (kept as the last token so Enter can
/// parse it back out to toggle that exact extension). See `open_extensions_page`.
pub(crate) fn ext_lines(exts: &[crate::ExtInfo]) -> Vec<String> {
    let mut lines = Vec::with_capacity(exts.len() + 3);
    lines.push(format!(
        "extensions — {} installed    (Enter: toggle on/off · :adblock on|off for the whole stack)",
        exts.len()
    ));
    lines.push(String::new());
    if exts.is_empty() {
        lines.push("(none installed)".into());
    }
    for e in exts {
        let mark = if e.enabled { "[on ]" } else { "[off]" };
        let name = if e.name.trim().is_empty() {
            "(unnamed extension)"
        } else {
            e.name.trim()
        };
        lines.push(format!("{mark}  {name}    {}", e.id));
    }
    lines
}

/// Plain-text lines for the `:history` vim pager: a header plus the visited URLs,
/// most-recent first, one per line (full URLs so they stay selectable/openable).
pub(crate) fn history_lines(visited: &crate::visited::Visited) -> Vec<String> {
    let mut lines = Vec::with_capacity(visited.len() + 2);
    lines.push(format!(
        "history — {} entries    (Enter: open · Shift+Enter: new tab · d: delete · v: select · :clear history to wipe)",
        visited.len()
    ));
    lines.push(String::new());
    lines.extend(visited.urls().map(str::to_string));
    lines
}

/// Plain-text lines for the `:aihist` vim pager: a header then one row per saved
/// chat NEWEST-FIRST — `created   size   name` — so a row's display position maps to
/// chat index `len-1-position` (see [`App::open_ai_history_entry`]). The name is the
/// chat's first prompt; an un-stamped (migrated) chat shows `—` for its time.
pub(crate) fn ai_history_lines(chats: &[crate::ai::AiChat]) -> Vec<String> {
    let n = chats.len();
    let mut lines = Vec::with_capacity(n + 2);
    lines.push(format!(
        "ai chats — {n} saved    (Enter: open · d: delete · v: select · y: yank · H/L step chats inside :ai)"
    ));
    lines.push(String::new());
    for chat in chats.iter().rev() {
        let when = if chat.created.is_empty() {
            "—".to_string()
        } else {
            chat.created.clone()
        };
        let size = fmt_chat_size(chat.size_bytes());
        lines.push(format!(
            "{when:<16}  {size:>8}  {}",
            truncate_name(&chat.name(), 80)
        ));
    }
    lines
}

/// The number of header lines `ai_history_lines` emits before the chat rows.
pub(crate) const AIHIST_HEADER: usize = 2;

/// Map a buffer row span `[lo, hi]` on the NEWEST-FIRST `:aihist` page to the
/// contiguous `[start, end]` range of `ai_chats` indices it covers (`n` = chat count).
/// Display position `p = row - HEADER` is chat index `n-1-p`, so the span inverts.
/// Rows above the first chat are clamped in; `None` if the span hits no chat rows.
pub(crate) fn aihist_rows_to_chat_range(n: usize, lo: usize, hi: usize) -> Option<(usize, usize)> {
    if n == 0 || hi < AIHIST_HEADER {
        return None;
    }
    let lo_row = lo.max(AIHIST_HEADER);
    let hi_row = hi.min(AIHIST_HEADER + n - 1);
    if lo_row > hi_row {
        return None;
    }
    Some((
        n - 1 - (hi_row - AIHIST_HEADER),
        n - 1 - (lo_row - AIHIST_HEADER),
    ))
}

/// Compact chat size: KB up to 1 MB, then MB — so a few-KB chat doesn't read `0.0 MB`.
fn fmt_chat_size(bytes: u64) -> String {
    let kb = bytes as f64 / 1024.0;
    if kb >= 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{kb:.1} KB")
    }
}

/// Truncate a name to `max` characters, appending `…` when shortened. Shared by the
/// `:aihist` and `:saved` pickers, whose name columns have a fixed width.
pub(crate) fn truncate_name(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// Plain-text lines for the `:alias` vim pager: a header plus `:name → expansion`
/// rows (sorted, since the source is a BTreeMap).
pub(crate) fn alias_lines(aliases: &std::collections::BTreeMap<String, String>) -> Vec<String> {
    let mut lines = Vec::with_capacity(aliases.len() + 2);
    lines.push(format!(
        "aliases — {} defined    (:unalias <name> to remove)",
        aliases.len()
    ));
    lines.push(String::new());
    lines.extend(aliases.iter().map(|(k, v)| {
        if crate::actions::shadows_builtin(k) {
            format!(":{k} → {v}    (ignored: :{k} is a built-in command)")
        } else {
            format!(":{k} → {v}")
        }
    }));
    lines
}

/// Plain-text lines for the `:version` pager: build/runtime details about this
/// browser, navigable and yankable with vim motions.
pub(crate) fn version_lines() -> Vec<String> {
    let kv = [
        ("Name", "browser"),
        ("Version", env!("CARGO_PKG_VERSION")),
        ("Description", env!("CARGO_PKG_DESCRIPTION")),
        ("Authors", env!("CARGO_PKG_AUTHORS")),
        (
            "Engine",
            "WebView2 (Chromium) via wry 0.55 — loaded on demand",
        ),
        ("Windowing", "tao 0.35 + softbuffer/fontdue native chrome"),
        (
            "Terminal",
            "native alacritty_terminal VT engine + a browser-pty-host companion (ConPTY)",
        ),
        ("Platform", std::env::consts::OS),
        ("Architecture", std::env::consts::ARCH),
    ];
    let mut lines = vec![
        format!("browser {}", env!("CARGO_PKG_VERSION")),
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

    #[test]
    fn help_anchor_resolves_commands_actions_and_sections() {
        // A command row wins first: by its anchor key or any word of the signature.
        assert_eq!(help_anchor("theme").as_deref(), Some("cmd-theme"));
        assert_eq!(help_anchor(":open").as_deref(), Some("cmd-open"));
        // ":tabopen · :t"
        assert_eq!(help_anchor("t").as_deref(), Some("cmd-tabopen"));
        // Actions not shadowed by a command resolve to their card.
        assert_eq!(
            help_anchor("install_scheme").as_deref(),
            Some("act-install_scheme")
        );
        // Section names and aliases.
        assert_eq!(help_anchor("caret").as_deref(), Some("sec-normal"));
        assert_eq!(help_anchor("selection").as_deref(), Some("sec-normal"));
        assert_eq!(help_anchor("bangs").as_deref(), Some("sec-bangs"));
        assert_eq!(help_anchor("math").as_deref(), Some("sec-maths"));
        // Unknown topics are None (the caller shows the full page and says so).
        assert_eq!(help_anchor("zzzz"), None);
        assert_eq!(help_anchor(""), None);
    }

    #[test]
    fn every_tab_cycle_help_topic_resolves() {
        // The `:help` Tab-cycle candidates must all be real jump targets, or the
        // cycle would offer a topic that falls back to the full page.
        for topic in help_topics() {
            assert!(
                help_anchor(&topic).is_some(),
                "help topic '{topic}' doesn't resolve"
            );
        }
    }

    #[test]
    fn every_help_topic_has_a_line_to_jump_to() {
        let page = crate::markdown::to_document(&commands_markdown(), "browser://commands");
        let text: String = page
            .blocks
            .iter()
            .map(|b| format!("{b:?}"))
            .collect::<Vec<_>>()
            .join("\n");
        for topic in help_topics() {
            let anchor = help_anchor(&topic).unwrap();
            let jump =
                help_jump_text(&anchor).unwrap_or_else(|| panic!("no jump text for {anchor}"));
            assert!(
                text.contains(&jump),
                "'{topic}' → '{jump}' isn't on the page"
            );
        }
        // Markdown-looking text in a description stays text.
        assert!(
            text.contains("dir <path>"),
            "a <path> must not vanish as HTML"
        );
    }

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
    fn ai_history_lines_are_newest_first_with_header() {
        use crate::ai::{AiChat, AiMessage, AiRole};
        let mk = |created: &str, prompt: &str| AiChat {
            created: created.into(),
            messages: vec![AiMessage {
                role: AiRole::You,
                text: prompt.into(),
            }],
        };
        let chats = vec![
            mk("2026-06-20 10:00", "first"),
            mk("2026-06-21 11:00", "second"),
        ];
        let lines = ai_history_lines(&chats);
        assert!(lines[0].starts_with("ai chats — 2 saved"));
        assert_eq!(lines[1], "");
        // Newest-first: the row at display position 0 (chat index len-1) is "second".
        assert!(lines[2].contains("second") && lines[2].contains("2026-06-21 11:00"));
        assert!(lines[3].contains("first"));
        // An empty created stamp renders as "—".
        let migrated = ai_history_lines(&[mk("", "old chat")]);
        assert!(migrated[2].trim_start().starts_with('—'));
    }

    #[test]
    fn aihist_row_span_inverts_to_chat_indices_newest_first() {
        // 3 chats: rows 2,3,4 show chats 2,1,0 (newest-first).
        assert_eq!(aihist_rows_to_chat_range(3, 2, 2), Some((2, 2))); // newest row → chat 2
        assert_eq!(aihist_rows_to_chat_range(3, 4, 4), Some((0, 0))); // oldest row → chat 0
        assert_eq!(aihist_rows_to_chat_range(3, 2, 3), Some((1, 2))); // two newest rows
        assert_eq!(aihist_rows_to_chat_range(3, 2, 4), Some((0, 2))); // all rows

        // A header-only or out-of-range span deletes nothing.
        assert_eq!(aihist_rows_to_chat_range(3, 0, 1), None);
        assert_eq!(aihist_rows_to_chat_range(0, 2, 2), None);
        // A span starting in the header clamps to the chat rows.
        assert_eq!(aihist_rows_to_chat_range(3, 0, 2), Some((2, 2)));
    }

    #[test]
    fn history_lines_keep_order_with_a_count_header() {
        let h = crate::visited::Visited::from_saved(
            vec![
                "https://a.test/".to_string(),
                "https://b.test/x".to_string(),
            ],
            vec![2, 1],
        );
        let lines = history_lines(&h);
        assert!(lines[0].starts_with("history — 2 entries"));
        assert_eq!(lines[1], ""); // blank under the header
        assert_eq!(
            &lines[2..],
            &[
                "https://a.test/".to_string(),
                "https://b.test/x".to_string()
            ]
        );
    }

    #[test]
    fn all_errors_are_oldest_first_with_command_and_time() {
        let errs = vec![
            entry("00:00:01", Some(":open a"), "e1"),
            entry("00:00:09", Some(":bad"), "e2"),
        ];
        let lines = error_lines(&errs, true);
        assert_eq!(lines[0], "[00:00:01] :open a — error 1");
        assert_eq!(lines[1], "e1");
        assert_eq!(lines[2], ""); // blank separator
        assert_eq!(lines[3], "[00:00:09] :bad — error 2");
        assert_eq!(lines[4], "e2");
    }
}
