//! Native terminal tabs: the pty-host process link ([`TermSession`] + the
//! kill-on-close job object), the [`App`] methods that open/feed/drive a
//! terminal, and the PTY key/mouse encoders.

use std::io::{Read as _, Write as _};
use std::time::Duration;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;

use tao::event::KeyEvent;
use tao::keyboard::{Key, KeyCode};

use crate::panes::PaneRect;
use crate::pty_term;
use crate::tabs::{TabContent, TabNav};
use crate::{clipboard_get, clipboard_set, App, ModeKind, Tab, UserEvent, TERM_PAD};

/// How close together two presses must be to count as a double/triple click when
/// selecting with the mouse. Windows' own default is 500 ms.
pub(crate) const MULTI_CLICK: Duration = Duration::from_millis(500);

/// A terminal tab's link to its companion `browser-pty-host` process. The ConPTY
/// lives entirely in that process; here we only hold a normal pipe + the process,
/// none of which can deadlock our exit.
pub(crate) struct TermSession {
    pub(crate) id: u64,
    pub(crate) child: Child,
    pub(crate) stdin: ChildStdin,
    /// Kill-on-close job containing the pty-host (and its conhost + shell), so
    /// closing it reaps the whole tree. 0 if jobs are unavailable.
    pub(crate) job: isize,
    pub(crate) reader: Option<JoinHandle<()>>,
    /// The native VT engine + grid this terminal renders from (no WebView2).
    pub(crate) pty: pty_term::PtyTerm,
}

impl TermSession {
    /// Send a framed message to the pty-host: `[kind:u8][len:u32 LE][payload]`.
    pub(crate) fn send(&mut self, kind: u8, payload: &[u8]) {
        let mut header = [0u8; 5];
        header[0] = kind;
        header[1..5].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        let _ = self.stdin.write_all(&header);
        let _ = self.stdin.write_all(payload);
        let _ = self.stdin.flush();
    }

    /// The shell's working directory, best-effort, for `:w`/close recording. The
    /// shell-integration report (OSC 9;9 / OSC 7) wins when one has arrived — it's
    /// exact and the ONLY view inside WSL. Otherwise fall back to reading the live
    /// process cwd of the innermost process under the pty-host, which covers
    /// nushell and cmd (they sync their physical cwd on `cd`) with no setup at
    /// all. (pwsh syncs neither, so without its prompt hook it yields the launch
    /// dir — same place a fresh shell would open anyway.)
    pub(crate) fn cwd(&self) -> Option<String> {
        if let Some(dir) = self.pty.cwd() {
            return Some(dir.to_string());
        }
        crate::proc_cwd::shell_cwd(self.child.id())
    }

    /// Tear down: closing the job force-kills the pty-host + its conhost + shell;
    /// the reader then EOFs on the (normal) pipe. None of this can hang our process.
    pub(crate) fn shutdown(mut self) {
        #[cfg(windows)]
        if self.job != 0 {
            job::close(self.job);
        }
        drop(self.stdin); // EOF the pty-host's stdin as well
        let _ = self.child.wait();
        if let Some(handle) = self.reader.take() {
            let _ = handle.join();
        }
    }
}

/// Windows job-object helpers: confine the pty-host to a kill-on-close job so the
/// OS reaps it (and its descendants) when we close the handle or the browser dies.
#[cfg(windows)]
mod job {
    use core::ffi::c_void;
    use std::mem::size_of;
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// Create a kill-on-close job and assign `process_handle` to it. Returns the
    /// job handle (as isize) to keep open; 0 on failure.
    pub fn create_for(process_handle: isize) -> isize {
        unsafe {
            let Ok(job) = CreateJobObjectW(None, windows::core::PCWSTR::null()) else {
                return 0;
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let _ = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const c_void,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if AssignProcessToJobObject(job, HANDLE(process_handle as *mut c_void)).is_err() {
                let _ = CloseHandle(job);
                return 0;
            }
            job.0 as isize
        }
    }

    pub fn close(job: isize) {
        unsafe {
            let _ = CloseHandle(HANDLE(job as *mut c_void));
        }
    }
}

impl App {
    /// Run a local shell command in the background. Result arrives as TermDone.
    /// Strictly shell-initiated — never reachable from page content.
    pub(crate) fn run_term(&mut self, cmd: &str) {
        self.set_status(format!("$ {cmd}"));
        let proxy = self.proxy.clone();
        let cmd = cmd.to_string();
        std::thread::spawn(move || {
            let (output, code) = exec_command(&cmd);
            let _ = proxy.send_event(UserEvent::TermDone { cmd, output, code });
        });
        self.window.request_redraw();
    }

    pub(crate) fn active_is_term(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .map(|t| t.term().is_some())
            .unwrap_or(false)
    }

    /// The painter terminal grids render with: the dedicated one when `:theme` set a
    /// custom terminal font/size, else the shared UI painter. Every terminal cell
    /// metric must come from THIS painter so the grid, PTY size, and mouse hit-tests
    /// agree.
    pub(crate) fn term_paint(&self) -> &crate::draw::Painter {
        self.term_painter.as_ref().unwrap_or(&self.painter)
    }

    /// Monospace cell size (width, height) in px at the current zoom.
    pub(crate) fn term_cell(&self) -> (i32, i32) {
        let p = self.term_paint();
        (p.measure("M").max(1) as i32, p.line_height().max(1) as i32)
    }

    /// The terminal grid size (cols, rows) that fits the focused pane.
    pub(crate) fn term_grid_size(&self) -> (usize, usize) {
        self.term_grid_for_rect(self.focused_pane_rect())
    }

    /// The terminal grid size (cols, rows) that fits a given pane rect.
    pub(crate) fn term_grid_for_rect(&self, r: PaneRect) -> (usize, usize) {
        let (cw, ch) = self.term_cell();
        let cols = (((r.w - 2 * TERM_PAD) / cw).max(1)) as usize;
        let rows = ((r.h / ch).max(1)) as usize;
        (cols, rows)
    }

    /// Open a native terminal tab (no WebView2). The ConPTY + shell run in the
    /// `browser-pty-host` companion (so they can't deadlock our exit); its raw
    /// output is parsed by an in-process `alacritty_terminal` engine and painted by
    /// our own renderer. Enters Passthrough (shell keeps keyboard focus and forwards
    /// every key to the PTY; Ctrl+S returns to Normal).
    pub(crate) fn open_terminal(&mut self) {
        self.open_terminal_at(None);
    }

    /// [`open_terminal`](Self::open_terminal) restored to a saved working directory
    /// (from session restore / `U` reopen; `None`/unknown opens as usual). A Windows
    /// path starts the shell there directly; a WSL path (`\\wsl$\…`,
    /// `\\wsl.localhost\…`, or a bare Linux `/path` from OSC 7) re-enters WSL —
    /// the nesting you had (shell → wsl) is recreated rather than replacing the
    /// shell with wsl.exe, so `exit` still drops back to the shell instead of
    /// closing the tab.
    ///
    /// The restore command is INVISIBLE for the known shells (nu/pwsh/cmd): it
    /// rides in as a startup argument (`nu -e`, `pwsh -NoExit -Command`,
    /// `cmd /K`), which runs AFTER the shell's own startup config — so it both
    /// leaves no typed line on screen (tmux-style) and survives a config that
    /// chdirs on startup (a `cd ~` in config.nu silently overrides the
    /// process-level `--cwd` start directory). Unknown shells fall back to
    /// typing the command into the PTY, which ConPTY buffers until the prompt.
    pub(crate) fn open_terminal_at(&mut self, cwd: Option<&str>) {
        let mut shell = if self.term_command.is_empty() {
            vec!["cmd".to_string()]
        } else {
            self.term_command.clone()
        };
        let (spawn_dir, wsl) = match cwd {
            Some(dir) => match wsl_target(dir) {
                Some(t) => (None, Some(t)),
                None => (Some(dir.to_string()), None),
            },
            None => (None, None),
        };
        // The restore command, in a syntax every target shell accepts: SINGLE
        // quotes (nushell treats backslashes inside double quotes as escapes, so
        // `cd "C:\Windows"` is a parse error there; single quotes are literal in
        // nu, pwsh and bash alike). A path containing one is left to `--cwd` only.
        let script = match (&wsl, &spawn_dir) {
            (Some((Some(d), path)), _) if !path.contains('\'') => {
                Some(format!("wsl -d '{d}' --cd '{path}'"))
            }
            (Some((None, path)), _) if !path.contains('\'') => Some(format!("wsl --cd '{path}'")),
            (None, Some(dir)) if !dir.contains('\'') => Some(format!("cd '{dir}'")),
            _ => None,
        };
        let kind = shell_kind(&shell[0]);
        let mut typed: Option<String> = None;
        if let Some(script) = script {
            match kind {
                // nu: run the command, then stay interactive.
                ShellKind::Nu => shell.extend(["-e".into(), script]),
                ShellKind::Pwsh => {
                    shell.extend(["-NoExit".into(), "-Command".into(), script]);
                }
                // cmd: only the WSL re-entry needs a command (`--cwd` already
                // handles plain dirs, and cmd's `cd` can't cross drives anyway);
                // /K runs it and keeps the shell open. cmd wants double quotes.
                ShellKind::Cmd => {
                    if let Some((distro, path)) = &wsl {
                        let line = match distro {
                            Some(d) => format!("wsl -d \"{d}\" --cd \"{path}\""),
                            None => format!("wsl --cd \"{path}\""),
                        };
                        shell.extend(["/K".into(), line]);
                    }
                }
                ShellKind::Other => typed = Some(format!("{script}\r")),
            }
        }
        let Some(id) = self.open_terminal_cmd_at(shell, spawn_dir.as_deref()) else { return };
        if let Some(line) = typed {
            if let Some(s) = self.term_session_mut(id) {
                s.send(0, line.as_bytes());
            }
        }
    }

    /// [`open_terminal`](Self::open_terminal) with an explicit argv to run in place
    /// of the configured shell (e.g. the `:theme` config editor). Returns the new
    /// terminal's id (`None` if the pty-host couldn't start) so a caller can react
    /// to that specific terminal closing.
    pub(crate) fn open_terminal_cmd(&mut self, shell: Vec<String>) -> Option<u64> {
        self.open_terminal_cmd_at(shell, None)
    }

    /// [`open_terminal_cmd`](Self::open_terminal_cmd) with an optional starting
    /// directory, forwarded to the pty-host as `--cwd` (invalid dirs are ignored
    /// there, falling back to the default).
    pub(crate) fn open_terminal_cmd_at(
        &mut self,
        shell: Vec<String>,
        cwd: Option<&str>,
    ) -> Option<u64> {
        let Some(host) = pty_host_path() else {
            self.set_error("could not locate browser-pty-host");
            return None;
        };

        let (cols, rows) = self.term_grid_size();
        let mut command = Command::new(&host);
        command.arg(cols.to_string()).arg(rows.to_string());
        if let Some(dir) = cwd {
            command.arg("--cwd").arg(dir);
        }
        // cmd reads its prompt from the PROMPT env var, so cwd reporting can be
        // injected with no user setup: prefix the prompt with `OSC 9;9;<cwd> ST`
        // ($E = ESC, $P = cwd — the Windows Terminal shell-integration pattern).
        // Other shells (pwsh, bash/WSL) need their own one-line prompt hook; see
        // `:help te`.
        if shell_kind(&shell[0]) == ShellKind::Cmd {
            let prompt = std::env::var("PROMPT").unwrap_or_else(|_| "$P$G".to_string());
            command.env("PROMPT", format!("$E]9;9;$P$E\\{prompt}"));
        }
        command
            .args(&shell)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // NOTE: do NOT pass CREATE_NO_WINDOW here. A console-less host can fail to
        // back its ConPTY (the shell starts but no output flows). The console popup
        // is suppressed by building browser-pty-host as a GUI-subsystem binary.
        let mut child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                self.set_error(format!("failed to start pty-host: {e}"));
                return None;
            }
        };

        // Confine the pty-host (and its conhost + shell) to a kill-on-close job so
        // closing the handle — or the browser dying — reaps the whole tree.
        #[cfg(windows)]
        let job = {
            use std::os::windows::io::AsRawHandle;
            job::create_for(child.as_raw_handle() as isize)
        };
        #[cfg(not(windows))]
        let job = 0isize;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let id = self.next_term_id;
        self.next_term_id += 1;

        // Pump the pty-host's stdout (raw PTY bytes) to the UI thread → VT parser.
        let proxy = self.proxy.clone();
        let reader_handle = std::thread::spawn(move || {
            let mut stdout = stdout;
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = proxy.send_event(UserEvent::TermClosed { id });
                        break;
                    }
                    Ok(n) => {
                        let data = buf[..n].to_vec();
                        if proxy.send_event(UserEvent::TermOutput { id, data }).is_err() {
                            break;
                        }
                    }
                }
            }
        });

        // New tab normally; under a split it fills the focused pane (place_tab).
        self.place_tab(
            Tab {
                url: format!("term: {}", shell[0]),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
                content: TabContent::Term(TermSession {
                    id,
                    child,
                    stdin,
                    job,
                    reader: Some(reader_handle),
                    pty: pty_term::PtyTerm::new(cols, rows, self.term_scrollback),
                }),
            },
            true,
        );
        self.mode = ModeKind::Passthrough; // terminal input mode; shell keeps focus
        self.window.set_focus();
        self.set_status("terminal — Ctrl+S returns to the shell");
        self.window.request_redraw();
        Some(id)
    }

    pub(crate) fn term_session_mut(&mut self, id: u64) -> Option<&mut TermSession> {
        self.tabs.iter_mut().find_map(|t| t.term_mut().filter(|s| s.id == id))
    }

    /// Feed raw PTY output bytes to a terminal's VT engine and repaint it. Any reply
    /// the engine produced (e.g. the `ESC[6n` cursor-position answer the shell waits
    /// for) is written straight back to the PTY.
    pub(crate) fn feed_terminal(&mut self, id: u64, data: &[u8]) {
        if let Some(s) = self.term_session_mut(id) {
            s.pty.feed(data);
            let reply = s.pty.take_reply();
            if !reply.is_empty() {
                s.send(0, &reply);
            }
            self.window.request_redraw();
        }
    }

    /// Match the active terminal's grid to the current window/zoom, resizing the PTY
    /// if the cell count changed.
    /// Match every visible terminal pane's grid to its pane rect, resizing the PTY
    /// when the cell count changed. With no split this is just the active terminal.
    ///
    /// Resize bursts (repeated Ctrl+-/+ zoom steps, live window drags) are COALESCED
    /// into a single PTY resize: nothing applies until the TARGET grid size has been
    /// stable for [`TERM_RESIZE_DEBOUNCE`](crate::app::TERM_RESIZE_DEBOUNCE) — every
    /// size change restarts the clock. Each PTY resize makes ConPTY reflow and re-emit
    /// the viewport (tearing the grid when frames land after we resized again, and
    /// making the shell reprint its prompt into scrollback), so a whole zoom sequence
    /// must cost exactly one resize, however slowly the steps are tapped.
    pub(crate) fn sync_active_term_size(&mut self) {
        let (panes, _) = self.pane_layout();
        let mut wants: Vec<(usize, usize, usize)> = Vec::new();
        for (tab, rect) in panes {
            let (cols, rows) = self.term_grid_for_rect(rect);
            if let Some(s) = self.tabs.get(tab).and_then(|t| t.term()) {
                if s.pty.cols != cols || s.pty.rows != rows {
                    wants.push((tab, cols, rows));
                }
            }
        }
        if wants.is_empty() {
            self.term_resize_want.clear();
            self.term_resize_want_at = None;
            return;
        }
        let now = std::time::Instant::now();
        if wants != self.term_resize_want {
            // The target size moved again: (re)start the settle window. The event
            // loop wakes us at its deadline to apply the final size.
            self.term_resize_want = wants;
            self.term_resize_want_at = Some(now);
            return;
        }
        if self
            .term_resize_want_at
            .is_none_or(|at| now.duration_since(at) < crate::app::TERM_RESIZE_DEBOUNCE)
        {
            return;
        }
        self.term_resize_want = Vec::new();
        self.term_resize_want_at = None;
        for (tab, cols, rows) in wants {
            let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) else {
                continue;
            };
            if s.pty.resize(cols, rows) {
                let mut p = [0u8; 4];
                p[0..2].copy_from_slice(&(cols as u16).to_le_bytes());
                p[2..4].copy_from_slice(&(rows as u16).to_le_bytes());
                s.send(1, &p);
            }
        }
    }

    /// Encode a key for the active terminal and write it to the PTY.
    pub(crate) fn key_term(&mut self, key: &KeyEvent) {
        // Swallow the stray `Tab` that Alt+Tab delivers right as the window regains
        // focus — otherwise it gets typed into the shell.
        if matches!(key.logical_key, Key::Tab)
            && self.last_focus_gain.elapsed() < Duration::from_millis(150)
        {
            return;
        }
        let app_cursor = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.term())
            .is_some_and(|s| s.pty.app_cursor());
        let ctrl = self.modifiers.control_key();
        let alt = self.modifiers.alt_key();
        let shift = self.modifiers.shift_key();
        if let Some(bytes) = encode_term_key(&key.logical_key, ctrl, alt, shift, app_cursor) {
            if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut())
            {
                // Typing dismisses a mouse selection's highlight, as terminals do —
                // it was copied on release, and leaving it lit over shifting output
                // would be stale. Copy mode's own `v`/`V` selection is untouched here
                // (keys only reach the PTY in passthrough).
                s.pty.clear_selection();
                s.send(0, &bytes);
            }
        }
    }

    /// Paste the clipboard into the active terminal's PTY (Ctrl+V in a `:te` tab).
    /// Newlines are normalized to CR (what Enter sends), and the text is wrapped in
    /// bracketed-paste markers when the program asked for them, so a shell treats a
    /// multi-line paste as literal input instead of executing each line.
    pub(crate) fn term_paste(&mut self) {
        let Some(text) = clipboard_get() else { return };
        if text.is_empty() {
            return;
        }
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut())
        else {
            return;
        };
        let mut out = Vec::new();
        let bracketed = s.pty.bracketed_paste();
        if bracketed {
            out.extend_from_slice(b"\x1b[200~");
        }
        out.extend_from_slice(text.as_bytes());
        if bracketed {
            out.extend_from_slice(b"\x1b[201~");
        }
        s.send(0, &out);
    }

    /// Whether the active terminal is in copy/vi mode.
    pub(crate) fn active_term_vi(&self) -> bool {
        self.active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.term())
            .is_some_and(|s| s.pty.is_vi())
    }

    /// Normal mode on a terminal means copy/vi mode — make sure the engine agrees.
    /// A tab/pane switch (gt, Ctrl+W hjkl, a click) can land on a terminal in Normal
    /// mode without passing through [`exit_to_normal`](App::exit_to_normal), leaving
    /// vi mode off: hjkl then fall through to plain display scrolling while the block
    /// cursor sits frozen at the live cursor — the "stuck normal-mode cursor" bug.
    /// Seeds the vi cursor at the live cursor, like Ctrl+S does.
    pub(crate) fn ensure_term_vi(&mut self) {
        if self.mode != ModeKind::Normal {
            return;
        }
        if let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut()) {
            if !s.pty.is_vi() {
                s.pty.toggle_vi();
            }
        }
    }

    /// Drive Alacritty's vi/copy mode from a key. Returns `true` if consumed;
    /// unhandled keys fall through to the normal browser bindings.
    pub(crate) fn key_term_vi(&mut self, key: &KeyEvent) -> bool {
        use pty_term::ViMotion as M;
        let ctrl = self.modifiers.control_key();
        // Awaiting the target char after f/F/t/T: this key IS the target. Any
        // non-character key (e.g. Esc) just cancels the pending find.
        if let Some((forward, till)) = self.term_find_pending.take() {
            if let Key::Character(c) = &key.logical_key {
                if let Some(ch) = c.chars().next() {
                    if let Some(s) =
                        self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut())
                    {
                        s.pty.vi_find_char(ch, forward, till);
                    }
                    self.term_last_find = Some((ch, forward, till));
                }
            }
            self.window.request_redraw();
            return true;
        }
        let last_find = self.term_last_find;
        let mut yank: Option<String> = None;
        let mut exit = false;
        let mut consumed = true;
        // An f/F/t/T that needs to wait for its target char (applied after the borrow).
        let mut pending: Option<(bool, bool)> = None;
        {
            let Some(s) = self.active.and_then(|i| self.tabs.get_mut(i)).and_then(|t| t.term_mut())
            else {
                return false;
            };
            let pty = &mut s.pty;
            let half = (pty.rows as i32 / 2).max(1);
            let page = (pty.rows as i32 - 1).max(1);
            if ctrl {
                match key.physical_key {
                    KeyCode::KeyU => pty.vi_scroll(-half),
                    KeyCode::KeyD => pty.vi_scroll(half),
                    _ => consumed = false,
                }
            } else {
                match &key.logical_key {
                    Key::Escape => {
                        if !pty.clear_selection() {
                            exit = true;
                        }
                    }
                    Key::Enter => exit = true,
                    Key::ArrowLeft => pty.vi_motion(M::Left),
                    Key::ArrowRight => pty.vi_motion(M::Right),
                    Key::ArrowUp => pty.vi_motion(M::Up),
                    Key::ArrowDown => pty.vi_motion(M::Down),
                    Key::PageUp => pty.vi_scroll(-page),
                    Key::PageDown => pty.vi_scroll(page),
                    Key::Character(c) => match *c {
                        "h" => pty.vi_motion(M::Left),
                        "j" => pty.vi_motion(M::Down),
                        "k" => pty.vi_motion(M::Up),
                        "l" => pty.vi_motion(M::Right),
                        "w" => pty.vi_motion(M::WordRight),
                        "b" => pty.vi_motion(M::WordLeft),
                        "e" => pty.vi_motion(M::WordRightEnd),
                        "0" => pty.vi_motion(M::First),
                        "$" => pty.vi_motion(M::Last),
                        "^" => pty.vi_motion(M::FirstOccupied),
                        "H" => pty.vi_motion(M::High),
                        "M" => pty.vi_motion(M::Middle),
                        "L" => pty.vi_motion(M::Low),
                        "G" => pty.vi_bottom(),
                        "g" => pty.vi_top(),
                        // Vim find-char: f/F/t/T arm a pending find for the next key;
                        // `;`/`,` repeat the last find (same / opposite direction).
                        "f" => pending = Some((true, false)),
                        "F" => pending = Some((false, false)),
                        "t" => pending = Some((true, true)),
                        "T" => pending = Some((false, true)),
                        ";" => {
                            if let Some((ch, fwd, till)) = last_find {
                                pty.vi_find_char(ch, fwd, till);
                            }
                        }
                        "," => {
                            if let Some((ch, fwd, till)) = last_find {
                                pty.vi_find_char(ch, !fwd, till);
                            }
                        }
                        "v" => {
                            if !pty.clear_selection() {
                                pty.start_selection(false);
                            }
                        }
                        "V" => {
                            if !pty.clear_selection() {
                                pty.start_selection(true);
                            }
                        }
                        "y" => yank = pty.yank(),
                        "i" => exit = true,
                        _ => consumed = false,
                    },
                    _ => consumed = false,
                }
            }
        }
        if let Some(p) = pending {
            self.term_find_pending = Some(p);
        }
        if let Some(text) = yank {
            let n = text.chars().count();
            clipboard_set(&text);
            self.set_status(format!("yanked {n} chars"));
        }
        if exit {
            self.enter_passthrough(); // leaves vi mode + resumes the live shell
        }
        if consumed || exit {
            self.window.request_redraw();
        }
        consumed || exit
    }

    /// Present a finished command vim-style: the result replaces the command-bar
    /// text (collapsed to one line).
    pub(crate) fn show_term_result(&mut self, _cmd: &str, output: &str, code: Option<i32>) {
        let trimmed = output.trim();
        let msg = if trimmed.is_empty() {
            let codestr = code.map(|c| c.to_string()).unwrap_or_else(|| "?".into());
            format!("(exit {codestr})")
        } else {
            trimmed.replace(['\r', '\n'], " ")
        };
        self.set_status(msg);
        self.window.request_redraw();
    }

    /// Close the tab whose terminal has the given id (its shell exited). Behaves
    /// like `x`, but only disturbs focus/mode if that tab was the active one.
    pub(crate) fn close_term_tab(&mut self, id: u64) {
        // The `:theme` config editor exited: re-read the config and apply it (the
        // edit → save → quit loop). Checked before the tab lookup so it still fires
        // when the tab was already closed by hand (`x`) before the PTY EOF arrived.
        let was_config_edit = self.config_edit_term == Some(id);
        if was_config_edit {
            self.config_edit_term = None;
        }
        let Some(i) = self.tabs.iter().position(|t| t.term().map(|s| s.id) == Some(id))
        else {
            if was_config_edit {
                self.reload_config();
            }
            return;
        };
        let was_active = self.active == Some(i);
        if let Some(session) = self.tabs[i].take_term() {
            session.shutdown();
        }
        let focus_after = self.drop_tab(i);
        self.active = if was_active {
            focus_after
        } else {
            // Keep the same focused tab, just adjusted for the index shift.
            self.active.map(|a| if a > i { a - 1 } else { a })
        };
        if was_active {
            self.mode = ModeKind::Normal;
            self.window.set_focus();
        }
        self.refresh_visibility();
        if was_config_edit {
            self.reload_config();
        }
    }

    /// Map a window pixel to the (col, row) cell of the terminal pane drawn at `rect`,
    /// plus whether the pointer sits in the cell's right half. Coordinates are the
    /// VIEWPORT's (row 0 = the pane's top line) and clamped to the grid, so dragging
    /// past an edge selects to it. Mirrors the renderer's origin exactly: the grid
    /// starts at `rect.x + TERM_PAD`, `rect.y` (see `chrome.rs`).
    fn term_cell_at(&self, tab: usize, rect: PaneRect, x: f64, y: f64) -> Option<(usize, usize, bool)> {
        let s = self.tabs.get(tab)?.term()?;
        let (cw, ch) = self.term_cell();
        let dx = (x as i32 - rect.x - TERM_PAD).max(0);
        let dy = (y as i32 - rect.y).max(0);
        let col = (dx / cw).clamp(0, s.pty.cols.saturating_sub(1) as i32) as usize;
        let row = (dy / ch).clamp(0, s.pty.rows.saturating_sub(1) as i32) as usize;
        Some((col, row, dx % cw >= cw / 2))
    }

    /// Left press inside a terminal pane: begin a mouse selection there. Consecutive
    /// presses on the same spot widen it (2 = word, 3 = line), like every other
    /// terminal. Returns `false` when the press isn't ours to take — not a terminal,
    /// or the program itself is reading the mouse (vim `mouse=a`, less, tmux) and the
    /// user isn't holding Shift to override it, the xterm convention.
    pub(crate) fn term_select_start(&mut self, tab: usize, rect: PaneRect, x: f64, y: f64) -> bool {
        let Some((col, row, right)) = self.term_cell_at(tab, rect, x, y) else {
            return false;
        };
        let shift = self.modifiers.shift_key();
        if self.tabs.get(tab).and_then(|t| t.term()).is_some_and(|s| s.pty.mouse_mode()) && !shift {
            return false;
        }
        // A press within the streak window AND on the same cell continues the streak;
        // anything else starts a fresh one. Cell-based (not pixel-exact) so a hand
        // that drifts a pixel between clicks still counts as a double-click.
        let now = std::time::Instant::now();
        let streak = match self.term_clicks {
            Some((at, px, py, n))
                if now.duration_since(at) < MULTI_CLICK
                    && self
                        .term_cell_at(tab, rect, px, py)
                        .is_some_and(|(c, r, _)| (c, r) == (col, row)) =>
            {
                n % 3 + 1
            }
            _ => 1,
        };
        self.term_clicks = Some((now, x, y, streak));
        let kind = match streak {
            2 => pty_term::SelectKind::Word,
            3 => pty_term::SelectKind::Line,
            _ => pty_term::SelectKind::Char,
        };
        if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
            s.pty.mouse_select(col, row, right, kind);
        }
        self.term_drag = Some((tab, rect));
        self.window.request_redraw();
        true
    }

    /// Pointer moved with the button down: extend the selection to the cell under it.
    pub(crate) fn term_select_drag(&mut self, x: f64, y: f64) {
        let Some((tab, rect)) = self.term_drag else { return };
        let Some((col, row, right)) = self.term_cell_at(tab, rect, x, y) else {
            return;
        };
        if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
            s.pty.mouse_select_to(col, row, right);
        }
        self.window.request_redraw();
    }

    /// Button released: end the drag and copy what was selected, the way terminals do
    /// (the selection stays highlighted — `y` in copy mode still works too). A press
    /// that selected nothing just clears, leaving the clipboard alone.
    pub(crate) fn term_select_end(&mut self) {
        let Some((tab, _)) = self.term_drag.take() else { return };
        let text = self.tabs.get(tab).and_then(|t| t.term()).and_then(|s| s.pty.selection_text());
        match text {
            Some(text) => {
                let chars = text.chars().count();
                clipboard_set(&text);
                self.set_status(format!("copied {chars} chars"));
            }
            None => {
                if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
                    s.pty.clear_selection();
                }
            }
        }
        self.window.request_redraw();
    }

    /// Forward a wheel notch to a terminal program that turned on mouse reporting,
    /// as a mouse wheel-button event at the cell under the cursor (relative to the
    /// pane's rect). Returns `false` (so the caller scrolls our scrollback instead)
    /// when no program wants mice.
    pub(crate) fn term_mouse_wheel(&mut self, tab: usize, rect: PaneRect, dy_lines: f64) -> bool {
        let (cw, ch) = self.term_cell();
        let (px, py) = (self.cursor_pos.0 as i32, self.cursor_pos.1 as i32);
        let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) else {
            return false;
        };
        if !s.pty.mouse_mode() {
            return false;
        }
        let (cols, rows) = (s.pty.cols as i32, s.pty.rows as i32);
        let col = (((px - rect.x - TERM_PAD) / cw) + 1).clamp(1, cols.max(1));
        let row = (((py - rect.y) / ch) + 1).clamp(1, rows.max(1));
        // xterm wheel buttons: 64 = up, 65 = down.
        let button = if dy_lines > 0.0 { 64 } else { 65 };
        let sgr = s.pty.sgr_mouse();
        let notches = (dy_lines.abs().round() as i32).max(1);
        let mut out = Vec::new();
        for _ in 0..notches {
            out.extend_from_slice(&encode_mouse_wheel(sgr, button, col, row));
        }
        s.send(0, &out);
        self.window.request_redraw();
        true
    }
}

/// Cap captured command output so a runaway command can't balloon memory.
const TERM_OUTPUT_CAP: usize = 200_000;

/// Run `cmd` through the platform shell, returning combined stdout+stderr and the
/// exit code. Blocking — call from a background thread.
pub(crate) fn exec_command(cmd: &str) -> (String, Option<i32>) {
    #[cfg(windows)]
    let mut command = {
        let mut c = Command::new("cmd");
        c.args(["/C", cmd]);
        c
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut c = Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    match command.output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            let err = String::from_utf8_lossy(&out.stderr);
            if !err.trim().is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&err);
            }
            if s.len() > TERM_OUTPUT_CAP {
                s.truncate(TERM_OUTPUT_CAP);
                s.push_str("\n… (output truncated)");
            }
            (s, out.status.code())
        }
        Err(e) => (format!("failed to run command: {e}"), None),
    }
}

/// Whether `program` resolves to an executable: an explicit path that exists, or a
/// bare name found on `PATH` (trying `PATHEXT` extensions on Windows, so `:shell nu`
/// matches `nu.exe`). Used to reject a `:shell` typo before it breaks `:te`.
pub(crate) fn program_exists(program: &str) -> bool {
    use std::path::{Path, PathBuf};
    let exts: Vec<String> = if cfg!(windows) {
        let mut v = vec![String::new()];
        let pe = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        v.extend(pe.split(';').filter(|s| !s.is_empty()).map(|s| s.to_lowercase()));
        v
    } else {
        vec![String::new()]
    };
    let exists_with_ext = |base: &Path| -> bool {
        exts.iter().any(|ext| {
            let cand = if ext.is_empty() {
                base.to_path_buf()
            } else {
                PathBuf::from(format!("{}{}", base.display(), ext))
            };
            cand.is_file()
        })
    };
    if program.contains(['/', '\\']) {
        return exists_with_ext(Path::new(program));
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| exists_with_ext(&dir.join(program)))
    })
}

/// The shells whose restore command can ride in invisibly as a startup argument
/// (see [`App::open_terminal_at`]); `Other` falls back to typing into the PTY.
#[derive(PartialEq)]
enum ShellKind {
    Nu,
    Pwsh,
    Cmd,
    Other,
}

/// Classify a shell argv[0] (possibly a full path, possibly `.exe`-suffixed).
fn shell_kind(argv0: &str) -> ShellKind {
    let base = argv0.rsplit(['\\', '/']).next().unwrap_or(argv0);
    match base.to_ascii_lowercase().trim_end_matches(".exe") {
        "nu" | "nushell" => ShellKind::Nu,
        "pwsh" | "powershell" => ShellKind::Pwsh,
        "cmd" => ShellKind::Cmd,
        _ => ShellKind::Other,
    }
}

/// Interpret a saved terminal cwd as a WSL location, returning
/// `(distro, linux_path)` when it is one — `None` means it's an ordinary Windows
/// directory. Two forms mean WSL: the UNC view of a distro's filesystem
/// (`\\wsl$\Ubuntu\home\x` / `\\wsl.localhost\Ubuntu\home\x`, as reported by an
/// OSC 9;9 built with `wslpath -w`), and a bare absolute Linux path (`/home/x`,
/// as reported by a plain OSC 7 — no distro recoverable, so the default is used).
fn wsl_target(dir: &str) -> Option<(Option<String>, String)> {
    if dir.starts_with('/') {
        return Some((None, dir.to_string()));
    }
    let rest = ["\\\\wsl$\\", "\\\\wsl.localhost\\"]
        .iter()
        .find_map(|p| dir.strip_prefix(p))?;
    let (distro, path) = match rest.split_once('\\') {
        Some((d, p)) => (d, format!("/{}", p.replace('\\', "/"))),
        None => (rest, "/".to_string()),
    };
    (!distro.is_empty()).then(|| (Some(distro.to_string()), path))
}

/// Locate the companion `browser-pty-host` binary next to our own executable.
fn pty_host_path() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = if cfg!(windows) {
        "browser-pty-host.exe"
    } else {
        "browser-pty-host"
    };
    Some(exe.parent()?.join(name))
}

/// Encode a single mouse-button event for a terminal in mouse-reporting mode.
/// `button` is the xterm button code (64/65 = wheel up/down), `col`/`row` are
/// 1-based cell coordinates. Uses the SGR (1006) form when the program asked for
/// it, else the legacy X10 byte form. Wheel events are press-only (no release).
fn encode_mouse_wheel(sgr: bool, button: u8, col: i32, row: i32) -> Vec<u8> {
    if sgr {
        format!("\x1b[<{button};{col};{row}M").into_bytes()
    } else {
        // X10: each field is its value + 32, clamped to one byte.
        let cb = 32u8.saturating_add(button);
        let cx = (32 + col).clamp(32, 255) as u8;
        let cy = (32 + row).clamp(32, 255) as u8;
        vec![0x1b, b'[', b'M', cb, cx, cy]
    }
}

/// Encode a key event into the bytes a PTY expects (the inverse of what xterm.js
/// used to do). Covers printable input, Ctrl-combos → control codes, Enter/Tab/
/// Backspace/Esc, and the cursor/navigation keys (honoring DECCKM app-cursor mode).
/// `None` for keys with no terminal meaning. Alt prefixes the sequence with ESC.
fn encode_term_key(key: &Key, ctrl: bool, alt: bool, shift: bool, app_cursor: bool) -> Option<Vec<u8>> {
    // xterm modifier digit: 1 + shift(1) + alt(2) + ctrl(4). When any modifier is
    // held, arrows/nav keys use the modified CSI forms (`ESC[1;5C` = Ctrl+Right) —
    // that's what makes Ctrl+arrows jump WORDS in the shell instead of one char
    // (ConPTY turns them back into key events with the ctrl flag set).
    let modf = 1 + shift as u8 + ((alt as u8) << 1) + ((ctrl as u8) << 2);
    let cursor = |c: u8| -> Vec<u8> {
        if modf > 1 {
            format!("\x1b[1;{modf}{}", c as char).into_bytes()
        } else if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    let tilde = |n: u8| -> Vec<u8> {
        if modf > 1 {
            format!("\x1b[{n};{modf}~").into_bytes()
        } else {
            format!("\x1b[{n}~").into_bytes()
        }
    };
    let mut out: Vec<u8> = match key {
        Key::Character(s) => {
            if ctrl {
                vec![ctrl_byte(s.chars().next()?)?]
            } else {
                s.as_bytes().to_vec()
            }
        }
        Key::Enter => vec![b'\r'],
        Key::Backspace => vec![0x7f],
        // Shift+Tab is the "back-tab" CSI Z — TUIs (Claude Code's mode switch,
        // form/field navigation) need it to move backward; plain Tab otherwise.
        Key::Tab => {
            if shift {
                b"\x1b[Z".to_vec()
            } else {
                vec![b'\t']
            }
        }
        Key::Escape => vec![0x1b],
        Key::Space => vec![b' '],
        Key::ArrowUp => cursor(b'A'),
        Key::ArrowDown => cursor(b'B'),
        Key::ArrowRight => cursor(b'C'),
        Key::ArrowLeft => cursor(b'D'),
        Key::Home => cursor(b'H'),
        Key::End => cursor(b'F'),
        Key::PageUp => tilde(5),
        Key::PageDown => tilde(6),
        Key::Delete => tilde(3),
        Key::Insert => tilde(2),
        _ => return None,
    };
    // Alt = Meta: prefix with ESC (unless the sequence already starts with one).
    if alt && out.first() != Some(&0x1b) {
        let mut v = vec![0x1b];
        v.append(&mut out);
        out = v;
    }
    Some(out)
}

/// Control code for Ctrl+<char>: `Ctrl+A`→0x01 … `Ctrl+Z`→0x1a, `Ctrl+[`→ESC,
/// `Ctrl+Space`→NUL, etc. `None` for non-controllable keys.
fn ctrl_byte(c: char) -> Option<u8> {
    if !c.is_ascii() {
        return None;
    }
    let u = c.to_ascii_uppercase() as u8;
    if (0x40..=0x5f).contains(&u) {
        Some(u & 0x1f)
    } else if u == b' ' {
        Some(0)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_target_classifies_saved_cwds() {
        // UNC views of a distro filesystem → (distro, linux path).
        assert_eq!(
            wsl_target("\\\\wsl$\\Ubuntu\\home\\kayf\\proj"),
            Some((Some("Ubuntu".into()), "/home/kayf/proj".into()))
        );
        assert_eq!(
            wsl_target("\\\\wsl.localhost\\Ubuntu-22.04\\home\\x"),
            Some((Some("Ubuntu-22.04".into()), "/home/x".into()))
        );
        // Distro root with no path → "/".
        assert_eq!(wsl_target("\\\\wsl$\\Debian"), Some((Some("Debian".into()), "/".into())));
        // A bare Linux path (OSC 7 from inside WSL) → default distro.
        assert_eq!(wsl_target("/home/kayf"), Some((None, "/home/kayf".into())));
        // Ordinary Windows dirs are not WSL.
        assert_eq!(wsl_target("C:\\projects\\browser"), None);
        assert_eq!(wsl_target("\\\\server\\share\\dir"), None);
    }

    /// Modified arrows/nav keys must use the xterm CSI forms (`ESC[1;5C` =
    /// Ctrl+Right) — plain sequences made Ctrl+arrows move one char in the shell
    /// instead of jumping words.
    #[test]
    fn modified_keys_use_xterm_csi() {
        let enc = |key, ctrl, alt, shift, app| encode_term_key(&key, ctrl, alt, shift, app).unwrap();
        assert_eq!(enc(Key::ArrowRight, false, false, false, false), b"\x1b[C");
        assert_eq!(enc(Key::ArrowRight, true, false, false, false), b"\x1b[1;5C");
        assert_eq!(enc(Key::ArrowLeft, true, false, false, false), b"\x1b[1;5D");
        assert_eq!(enc(Key::ArrowRight, false, false, true, false), b"\x1b[1;2C");
        assert_eq!(enc(Key::ArrowRight, false, true, false, false), b"\x1b[1;3C");
        assert_eq!(enc(Key::ArrowRight, true, false, true, false), b"\x1b[1;6C");
        // Application-cursor mode (DECCKM) only changes the UNmodified form.
        assert_eq!(enc(Key::ArrowRight, false, false, false, true), b"\x1bOC");
        assert_eq!(enc(Key::ArrowRight, true, false, false, true), b"\x1b[1;5C");
        // Modified editing keys keep their number with the modifier appended.
        assert_eq!(enc(Key::Delete, false, false, false, false), b"\x1b[3~");
        assert_eq!(enc(Key::Delete, true, false, false, false), b"\x1b[3;5~");
        assert_eq!(enc(Key::Home, true, false, false, false), b"\x1b[1;5H");
    }
}
