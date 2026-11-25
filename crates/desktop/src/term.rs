//! Native terminal tabs: the pty-host process link ([`TermSession`] + the
//! kill-on-close job object), the [`App`] methods that open/feed/drive a
//! terminal, and the PTY key/mouse encoders.

use std::io::{Read as _, Write as _};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tao::event::KeyEvent;
use tao::keyboard::{Key, KeyCode};

use crate::panes::PaneRect;
use crate::pty_term;
use crate::tabs::{TabContent, TabNav};
use crate::{clipboard_get, clipboard_set, App, ModeKind, Tab, UserEvent, TERM_PAD};

/// How close together two presses must be to count as a double/triple click when
/// selecting with the mouse. Windows' own default is 500 ms.
pub(crate) const MULTI_CLICK: Duration = Duration::from_millis(500);

/// Coalesce terminal PTY resizes: a grid-size change only applies once the target size
/// has been stable this long, and every further change restarts the clock — so a whole
/// zoom sequence or window drag costs exactly one PTY resize. Longer bridges slower
/// zoom taps; shorter refits the grid sooner.
pub(crate) const TERM_RESIZE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Half a cursor blink: shown this long, then hidden this long. Windows' default
/// caret blink rate.
pub(crate) const BLINK_HALF: Duration = Duration::from_millis(530);

/// A cursor stops blinking (and stays shown) this long after the last keypress,
/// output or focus change, as in Alacritty: an idle terminal shouldn't wake the
/// browser twice a second forever.
pub(crate) const BLINK_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a blink cycle that restarted `elapsed` ago stands: whether the cursor shows,
/// and when that next changes (`None` once it has stopped blinking).
pub(crate) fn blink_phase(elapsed: Duration) -> (bool, Option<Duration>) {
    if elapsed >= BLINK_TIMEOUT {
        return (true, None);
    }
    let half = BLINK_HALF.as_millis();
    let ms = elapsed.as_millis();
    let on = (ms / half).is_multiple_of(2);
    let next = Duration::from_millis((half - ms % half) as u64);
    // The last change before the timeout lands it shown.
    (on, Some(next.min(BLINK_TIMEOUT - elapsed)))
}

/// Add a wheel movement (`dy`, in lines) to `acc` and take out the whole `step`s
/// it now holds (signed: positive = up). The remainder waits for the next event,
/// so a precision touchpad's fractions add up instead of each rounding to a whole
/// step; turning the other way drops what was left of the old direction.
pub(crate) fn take_wheel_steps(acc: &mut f64, dy: f64, step: f64) -> i32 {
    if *acc != 0.0 && acc.signum() != dy.signum() {
        *acc = 0.0;
    }
    *acc += dy;
    let steps = (*acc / step).trunc();
    *acc -= steps * step;
    steps as i32
}

/// Windows' "lines to scroll per notch" setting (3 unless changed; a page setting
/// counts as 3, as tao treats it).
fn wheel_lines() -> f64 {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SystemParametersInfoW, SPI_GETWHEELSCROLLLINES, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
        };
        let mut lines = 3u32;
        // SAFETY: SPI_GETWHEELSCROLLLINES writes one UINT into `lines`.
        let _ = unsafe {
            SystemParametersInfoW(
                SPI_GETWHEELSCROLLLINES,
                0,
                Some((&mut lines as *mut u32).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
        };
        // WHEEL_PAGESCROLL (u32::MAX) means "a page per notch".
        if lines == 0 || lines == u32::MAX {
            return 3.0;
        }
        lines as f64
    }
    #[cfg(not(windows))]
    3.0
}

/// How long a terminal flashes when a program rings the bell.
pub(crate) const BELL_FLASH: Duration = Duration::from_millis(150);

/// A terminal grid size a pane wants: `(tab, cols, rows)`.
pub(crate) type GridWant = (usize, usize, usize);

/// Holds back terminal resizes until the wanted sizes stop changing for
/// [`TERM_RESIZE_DEBOUNCE`].
#[derive(Default)]
pub(crate) struct ResizeDebounce {
    want: Vec<GridWant>,
    since: Option<Instant>,
}

impl ResizeDebounce {
    /// Feed the sizes the visible terminals want right now. Returns them once they've
    /// been the same for [`TERM_RESIZE_DEBOUNCE`]; a change restarts the wait, and an
    /// empty list drops anything pending.
    pub(crate) fn settle(&mut self, wants: Vec<GridWant>, now: Instant) -> Option<Vec<GridWant>> {
        if wants.is_empty() {
            *self = Self::default();
            return None;
        }
        if wants != self.want {
            self.want = wants;
            self.since = Some(now);
            return None;
        }
        if self
            .since
            .is_none_or(|at| now.duration_since(at) < TERM_RESIZE_DEBOUNCE)
        {
            return None;
        }
        *self = Self::default();
        Some(wants)
    }

    /// When the pending resize settles, if one is waiting.
    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.since.map(|at| at + TERM_RESIZE_DEBOUNCE)
    }
}

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
    /// When a program last rang the bell, while its flash
    /// (`term.visual_bell`) is still showing.
    pub(crate) bell_at: Option<Instant>,
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
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
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

    /// How terminals are drawn this frame: the style and cursor settings, the blink
    /// phase, and `focused` = the window has the OS focus and keys go to terminals
    /// (each pane narrows that to itself, see `chrome::paint_pane`).
    pub(crate) fn term_render_opts(&self) -> pty_term::RenderOpts {
        pty_term::RenderOpts {
            text_styles: self.config.setting("term.text_styles"),
            cursor_shapes: self.config.setting("term.cursor_shape"),
            focused: self.window_focused,
            blink_on: blink_phase(self.term_blink_at.elapsed()).0,
        }
    }

    /// When the focused terminal's blinking cursor next turns on or off, so the event
    /// loop can wake to repaint it. `None` when nothing is blinking.
    pub(crate) fn term_blink_deadline(&self) -> Option<Instant> {
        if !self.term_cursor_blinks() {
            return None;
        }
        let (_, next) = blink_phase(self.term_blink_at.elapsed());
        Some(Instant::now() + next?)
    }

    /// Whether the focused terminal shows a blinking cursor: the window is focused,
    /// keys go to the terminal, and its cursor style blinks.
    pub(crate) fn term_cursor_blinks(&self) -> bool {
        self.window_focused
            && self.mode == ModeKind::Passthrough
            && self
                .active
                .and_then(|i| self.tabs.get(i))
                .and_then(|t| t.term())
                .is_some_and(|s| s.pty.cursor_blinking() && !s.pty.is_vi())
    }

    /// Restart the cursor blink with the cursor shown (typing, output, focus).
    pub(crate) fn restart_term_blink(&mut self) {
        self.term_blink_at = Instant::now();
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
        let Some(id) = self.open_terminal_cmd_at(shell, spawn_dir.as_deref()) else {
            return;
        };
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
                        if proxy
                            .send_event(UserEvent::TermOutput { id, data })
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        // New tab normally; under a split it fills the focused pane if blank (place_tab).
        self.place_tab(
            Tab {
                id: crate::layout::TabId::new(),
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
                    pty: {
                        let mut pty = pty_term::PtyTerm::new(cols, rows, self.term_scrollback);
                        pty.set_cursor_blink(self.config.setting("term.cursor_blink"));
                        pty
                    },
                    bell_at: None,
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
        self.tabs
            .iter_mut()
            .find_map(|t| t.term_mut().filter(|s| s.id == id))
    }

    /// Feed raw PTY output bytes to a terminal's VT engine and repaint it. Any reply
    /// the engine produced (e.g. the `ESC[6n` cursor-position answer the shell waits
    /// for) is written straight back to the PTY.
    pub(crate) fn feed_terminal(&mut self, id: u64, data: &[u8]) {
        let style = self.term_style;
        let (cw, ch) = self.term_cell();
        let clipboard_ok = self.config.setting("term.clipboard_write");
        let bell_ok = self.config.setting("term.visual_bell");
        let active = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.term())
            .is_some_and(|s| s.id == id);
        let Some(s) = self.term_session_mut(id) else {
            return;
        };
        s.pty.feed(data);
        let reply = s.pty.take_reply(&style, (cw as u16, ch as u16));
        if !reply.is_empty() {
            s.send(0, &reply);
        }
        if s.pty.take_bell() && bell_ok {
            s.bell_at = Some(Instant::now());
        }
        // A program copying (nvim, tmux, also over SSH) writes the clipboard.
        if let Some(text) = s.pty.take_clipboard().filter(|_| clipboard_ok) {
            clipboard_set(&text);
        }
        // Output shows the cursor again, like typing does.
        if active {
            self.restart_term_blink();
        }
        self.window.request_redraw();
    }

    /// When the next bell flash ends (to wake and repaint without it).
    pub(crate) fn term_bell_deadline(&self) -> Option<Instant> {
        self.tabs
            .iter()
            .filter_map(|t| t.term()?.bell_at)
            .map(|at| at + BELL_FLASH)
            .min()
    }

    /// End bell flashes that have run their course, repainting without them.
    pub(crate) fn expire_term_bells(&mut self) {
        let mut ended = false;
        for s in self.tabs.iter_mut().filter_map(|t| t.term_mut()) {
            if s.bell_at.is_some_and(|at| at.elapsed() >= BELL_FLASH) {
                s.bell_at = None;
                ended = true;
            }
        }
        if ended {
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
    /// stable for [`TERM_RESIZE_DEBOUNCE`] — every
    /// size change restarts the clock. Each PTY resize makes ConPTY reflow and re-emit
    /// the viewport (tearing the grid when frames land after we resized again, and
    /// making the shell reprint its prompt into scrollback), so a whole zoom sequence
    /// must cost exactly one resize, however slowly the steps are tapped.
    pub(crate) fn sync_active_term_size(&mut self) {
        let (panes, _) = self.pane_layout();
        let mut wants: Vec<GridWant> = Vec::new();
        for (tab, rect) in panes {
            let (cols, rows) = self.term_grid_for_rect(rect);
            if let Some(s) = self.tabs.get(tab).and_then(|t| t.term()) {
                if s.pty.cols != cols || s.pty.rows != rows {
                    wants.push((tab, cols, rows));
                }
            }
        }
        // Until the target settles, the event loop wakes us at its deadline to try again.
        let Some(wants) = self.term_resize.settle(wants, Instant::now()) else {
            return;
        };
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
        let mut ctrl = self.modifiers.control_key();
        let mut alt = self.modifiers.alt_key();
        let shift = self.modifiers.shift_key();
        // A character typed with AltGr (Ctrl+Alt to Windows) is plain text.
        if is_altgr(&key.logical_key, &key.key_without_modifiers(), ctrl, alt) {
            (ctrl, alt) = (false, false);
        }
        if let Some(bytes) = encode_term_key(&key.logical_key, ctrl, alt, shift, app_cursor) {
            if let Some(s) = self
                .active
                .and_then(|i| self.tabs.get_mut(i))
                .and_then(|t| t.term_mut())
            {
                // Typing dismisses a mouse selection's highlight, as terminals do —
                // it was copied on release, and leaving it lit over shifting output
                // would be stale. Copy mode's own `v`/`V` selection is untouched here
                // (keys only reach the PTY in passthrough).
                s.pty.clear_selection();
                snap_to_live(s, &self.window);
                s.send(0, &bytes);
                self.restart_term_blink();
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
        let Some(s) = self
            .active
            .and_then(|i| self.tabs.get_mut(i))
            .and_then(|t| t.term_mut())
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
        snap_to_live(s, &self.window);
        s.send(0, &out);
        self.restart_term_blink();
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
        if let Some(s) = self
            .active
            .and_then(|i| self.tabs.get_mut(i))
            .and_then(|t| t.term_mut())
        {
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
                    if let Some(s) = self
                        .active
                        .and_then(|i| self.tabs.get_mut(i))
                        .and_then(|t| t.term_mut())
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
        // `s`: label the links on screen (after the borrow).
        let mut link_hints = false;
        {
            let Some(s) = self
                .active
                .and_then(|i| self.tabs.get_mut(i))
                .and_then(|t| t.term_mut())
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
                        // With no find to repeat, `;` is the shell's own key: hide/show
                        // the HUD (key_normal handles it once this returns unconsumed).
                        ";" => match last_find {
                            Some((ch, fwd, till)) => pty.vi_find_char(ch, fwd, till),
                            None => consumed = false,
                        },
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
                        // Links: the terminal's `f` (which is vim's find-char here).
                        // `s` is a scroll-hint key on pages, with no use in a terminal.
                        "s" => link_hints = true,
                        _ => consumed = false,
                    },
                    _ => consumed = false,
                }
            }
        }
        if let Some(p) = pending {
            self.term_find_pending = Some(p);
        }
        if link_hints {
            self.enter_term_hints();
            return true;
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
        let Some(i) = self
            .tabs
            .iter()
            .position(|t| t.term().map(|s| s.id) == Some(id))
        else {
            if was_config_edit {
                self.reload_config();
            }
            return;
        };
        if let Some(session) = self.tabs[i].take_term() {
            session.shutdown();
        }
        if self.drop_tab_keeping_focus(i) {
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
    fn term_cell_at(
        &self,
        tab: usize,
        rect: PaneRect,
        x: f64,
        y: f64,
    ) -> Option<(usize, usize, bool)> {
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
        if self.term_mouse_report(tab).is_some() && !self.modifiers.shift_key() {
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
        let Some((tab, rect)) = self.term_drag else {
            return;
        };
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
        let Some((tab, _)) = self.term_drag.take() else {
            return;
        };
        let text = self
            .tabs
            .get(tab)
            .and_then(|t| t.term())
            .and_then(|s| s.pty.selection_text());
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

    /// Ctrl held over a link in a terminal: underline it and show a hand pointer, as
    /// a hint that Ctrl+click opens it. Called as the pointer moves and as Ctrl goes
    /// down or up; cheap when Ctrl isn't held and nothing was hovered.
    pub(crate) fn term_link_hover(&mut self) {
        let (x, y) = self.cursor_pos;
        let want = if self.config.setting("term.links") && self.modifiers.control_key() {
            self.pane_at_pixel(x, y).and_then(|(tab, rect)| {
                let (col, row, _) = self.term_cell_at(tab, rect, x, y)?;
                let link = self.tabs.get(tab)?.term()?.pty.link_at(col, row)?;
                Some((tab, (link.start, link.end)))
            })
        } else {
            None
        };
        if want == self.term_link_hover {
            return;
        }
        if let Some((tab, _)) = self.term_link_hover.take() {
            if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
                s.pty.link_hover = None;
            }
        }
        if let Some((tab, range)) = want {
            if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
                s.pty.link_hover = Some(range);
            }
        }
        self.term_link_hover = want;
        self.window.set_cursor_icon(if want.is_some() {
            tao::window::CursorIcon::Hand
        } else {
            tao::window::CursorIcon::Default
        });
        self.window.request_redraw();
    }

    /// A Ctrl+click on a link in terminal `tab`: open it in a new tab. Returns whether
    /// the press was on a link (and so is used up).
    pub(crate) fn term_click_link(&mut self, tab: usize, rect: PaneRect) -> bool {
        if !self.config.setting("term.links") || !self.modifiers.control_key() {
            return false;
        }
        let (x, y) = self.cursor_pos;
        let Some((col, row, _)) = self.term_cell_at(tab, rect, x, y) else {
            return false;
        };
        let Some(link) = self
            .tabs
            .get(tab)
            .and_then(|t| t.term()?.pty.link_at(col, row))
        else {
            return false;
        };
        self.open_term_link(&link.url);
        true
    }

    /// Open a link from a terminal in a new tab. Only web addresses open: a terminal's
    /// `file:` links (`ls --hyperlink`) and other schemes just say so.
    pub(crate) fn open_term_link(&mut self, url: &str) -> bool {
        match pty_term::openable_url(url) {
            Some(url) => {
                self.clear_term_link_hover();
                self.open_tab(&url, false, true);
                true
            }
            None => {
                self.set_status(format!("only web links open from the terminal: {url}"));
                false
            }
        }
    }

    /// Re-run the `/` search in each terminal on screen whose output moved since its
    /// matches were found, once per frame rather than once per chunk of output.
    pub(crate) fn refresh_term_searches(&mut self) {
        let (panes, _) = self.pane_layout();
        for (tab, _) in panes {
            if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
                s.pty.search_refresh();
            }
        }
    }

    /// Drop the hovered-link underline and hand pointer.
    pub(crate) fn clear_term_link_hover(&mut self) {
        if let Some((tab, _)) = self.term_link_hover.take() {
            if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
                s.pty.link_hover = None;
            }
            self.window
                .set_cursor_icon(tao::window::CursorIcon::Default);
        }
    }

    /// Label every link on screen (`s` in copy mode, Ctrl+Shift+O while typing);
    /// typing a label opens that link in a new tab, Esc goes back to what you were
    /// doing. The same badges and letters as `f` on a page.
    pub(crate) fn enter_term_hints(&mut self) {
        if !self.config.setting("term.links") {
            self.set_status("terminal links are off (term.links)");
            return;
        }
        let rect = self.focused_pane_rect();
        let (cw, ch) = self.term_cell();
        let Some(pty) = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.term())
            .map(|s| &s.pty)
        else {
            return;
        };
        let links = pty.visible_links();
        let labels = crate::hints::hint_labels(links.len());
        let hints: Vec<crate::hints::NativeHint> = links
            .into_iter()
            .zip(labels)
            .filter_map(|(link, label)| {
                let (col, row) = pty.viewport_cell(link.start)?;
                Some(crate::hints::NativeHint {
                    label,
                    url: link.url,
                    x: rect.x + TERM_PAD + col as i32 * cw,
                    y: rect.y + row as i32 * ch + ch * 3 / 4,
                })
            })
            .collect();
        if hints.is_empty() {
            self.set_status("no links on screen");
            return;
        }
        self.clear_term_link_hover();
        self.native_hints = hints;
        self.hint_back_to_passthrough = self.mode == ModeKind::Passthrough;
        self.hint_act = crate::hints::HintAct::Follow;
        self.hint_input.clear();
        self.mode = ModeKind::Hint;
        self.window.request_redraw();
    }

    /// The mouse reporting terminal `tab`'s program asked for, when it should get the
    /// mouse: `term.mouse_reporting` is on and copy mode isn't (copy mode's clicks
    /// and drags select, as they do for a program that reads no mouse).
    pub(crate) fn term_mouse_report(&self, tab: usize) -> Option<pty_term::MouseReport> {
        if !self.config.setting("term.mouse_reporting") {
            return None;
        }
        let s = self.tabs.get(tab)?.term()?;
        if s.pty.is_vi() {
            return None;
        }
        s.pty.mouse_report()
    }

    /// The 1-based cell a mouse report names for a window pixel over terminal `tab`.
    fn term_report_cell(&self, tab: usize, rect: PaneRect, x: f64, y: f64) -> Option<(u32, u32)> {
        let (col, row, _) = self.term_cell_at(tab, rect, x, y)?;
        Some((col as u32 + 1, row as u32 + 1))
    }

    /// The modifier bits a mouse report carries: Shift 4, Alt 8, Ctrl 16.
    fn mouse_mods(&self) -> u8 {
        let m = &self.modifiers;
        (m.shift_key() as u8) << 2 | (m.alt_key() as u8) << 3 | (m.control_key() as u8) << 4
    }

    /// A mouse button pressed over terminal `tab`: report it to the program when it
    /// reads the mouse (Shift held keeps the press ours, to select, as in xterm).
    /// `button` is xterm's code: 0 left, 1 middle, 2 right. Returns whether the press
    /// went to the program.
    pub(crate) fn term_mouse_press(&mut self, tab: usize, rect: PaneRect, button: u8) -> bool {
        let Some(report) = self.term_mouse_report(tab) else {
            return false;
        };
        if self.modifiers.shift_key() {
            return false;
        }
        let (x, y) = self.cursor_pos;
        let Some(cell) = self.term_report_cell(tab, rect, x, y) else {
            return false;
        };
        let bytes = encode_mouse(report.encoding, button | self.mouse_mods(), cell, false);
        self.term_mouse_held = Some(TermMouseHeld {
            tab,
            rect,
            button,
            cell,
        });
        self.term_mouse_send(tab, &bytes);
        true
    }

    /// [`term_mouse_press`](Self::term_mouse_press) for whatever pane is under the
    /// pointer (the right and middle buttons, which don't focus or select).
    pub(crate) fn term_mouse_button_press(&mut self, button: u8) -> bool {
        let (x, y) = self.cursor_pos;
        match self.pane_at_pixel(x, y) {
            Some((tab, rect)) => self.term_mouse_press(tab, rect, button),
            None => false,
        }
    }

    /// A mouse button released: report it when its press went to the program. The
    /// release lands on the cell under the pointer (clamped to the pane), or on the
    /// last one reported when the pointer left the window (`at_last_cell`).
    pub(crate) fn term_mouse_release(&mut self, button: u8, at_last_cell: bool) -> bool {
        let Some(held) = self.term_mouse_held.filter(|h| h.button == button) else {
            return false;
        };
        self.term_mouse_held = None;
        let Some(report) = self.term_mouse_report(held.tab) else {
            return true;
        };
        let (x, y) = self.cursor_pos;
        let cell = if at_last_cell {
            held.cell
        } else {
            self.term_report_cell(held.tab, held.rect, x, y)
                .unwrap_or(held.cell)
        };
        let bytes = encode_mouse(report.encoding, button | self.mouse_mods(), cell, true);
        self.term_mouse_send(held.tab, &bytes);
        true
    }

    /// The pointer moved: report it when the program tracks motion — while a button
    /// it was told about is held (drag, DEC 1002), or always (DEC 1003). One report
    /// per cell crossed, not per pixel.
    pub(crate) fn term_mouse_motion(&mut self, x: f64, y: f64) {
        if let Some(held) = self.term_mouse_held {
            let Some(report) = self.term_mouse_report(held.tab) else {
                return;
            };
            if !(report.drag || report.motion) {
                return;
            }
            let Some(cell) = self.term_report_cell(held.tab, held.rect, x, y) else {
                return;
            };
            if cell == held.cell {
                return;
            }
            self.term_mouse_held = Some(TermMouseHeld { cell, ..held });
            let code = held.button | 32 | self.mouse_mods();
            let bytes = encode_mouse(report.encoding, code, cell, false);
            self.term_mouse_send(held.tab, &bytes);
            return;
        }
        let Some((tab, rect)) = self.pane_at_pixel(x, y) else {
            self.term_mouse_hover = None;
            return;
        };
        let Some(report) = self.term_mouse_report(tab).filter(|r| r.motion) else {
            self.term_mouse_hover = None;
            return;
        };
        let Some(cell) = self.term_report_cell(tab, rect, x, y) else {
            return;
        };
        if self.term_mouse_hover == Some((tab, cell)) {
            return;
        }
        self.term_mouse_hover = Some((tab, cell));
        // No button held: xterm reports motion as button 3 (none) plus the motion bit.
        let bytes = encode_mouse(report.encoding, 3 | 32 | self.mouse_mods(), cell, false);
        self.term_mouse_send(tab, &bytes);
    }

    fn term_mouse_send(&mut self, tab: usize, bytes: &[u8]) {
        if let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) {
            s.send(0, bytes);
        }
    }

    /// The wheel over terminal `tab` (`dy_lines` > 0 = rolled up, in lines: tao
    /// multiplies notches by Windows' lines-per-notch setting). A program reading the
    /// mouse gets one wheel report per notch, as xterm and Windows Terminal send, and
    /// scrolls its own amount for each (vim: 3 lines). Otherwise it moves a line per
    /// line: as arrow keys when a full-screen program without mouse support (less,
    /// man) is up, since the alternate screen has no scrollback, else our scrollback.
    pub(crate) fn term_wheel(&mut self, tab: usize, rect: PaneRect, dy_lines: f64) {
        if let Some(report) = self.term_mouse_report(tab) {
            let notches = take_wheel_steps(&mut self.term_wheel_acc, dy_lines, wheel_lines());
            if notches == 0 {
                return;
            }
            let (x, y) = self.cursor_pos;
            let Some(cell) = self.term_report_cell(tab, rect, x, y) else {
                return;
            };
            // xterm wheel buttons: 64 = up, 65 = down.
            let button = if notches > 0 { 64 } else { 65 };
            let one = encode_mouse(report.encoding, button | self.mouse_mods(), cell, false);
            self.term_mouse_send(tab, &one.repeat(notches.unsigned_abs() as usize));
            return;
        }
        let lines = take_wheel_steps(&mut self.term_wheel_acc, dy_lines, 1.0);
        if lines == 0 {
            return;
        }
        let mouse_on = self.config.setting("term.mouse_reporting");
        let Some(s) = self.tabs.get_mut(tab).and_then(|t| t.term_mut()) else {
            return;
        };
        if mouse_on && s.pty.alternate_scroll() {
            let key = if lines > 0 {
                Key::ArrowUp
            } else {
                Key::ArrowDown
            };
            if let Some(one) = encode_term_key(&key, false, false, false, s.pty.app_cursor()) {
                s.send(0, &one.repeat(lines.unsigned_abs() as usize));
            }
            return;
        }
        s.pty.scroll_display(lines);
        self.window.request_redraw();
    }
}

/// A mouse button whose press went to a terminal program, so its motion and
/// release go there too.
#[derive(Clone, Copy)]
pub(crate) struct TermMouseHeld {
    pub(crate) tab: usize,
    pub(crate) rect: PaneRect,
    /// xterm's code: 0 left, 1 middle, 2 right.
    pub(crate) button: u8,
    /// The last cell reported (1-based).
    pub(crate) cell: (u32, u32),
}

/// Input sent to a terminal returns its view to the live screen, as other terminals
/// do: you scrolled up to read, and typing means you're back at the prompt. The
/// program's own output doesn't move a scrolled-back view, so this has to.
fn snap_to_live(s: &mut TermSession, window: &tao::window::Window) {
    if s.pty.scrolled_back() {
        s.pty.scroll_to_bottom();
        window.request_redraw();
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

/// The shell `:te` starts until `:shell` picks another: PowerShell 7 (`pwsh`) when
/// it's installed, else Windows PowerShell, which every Windows install has. `-NoLogo`
/// skips the copyright banner. Elsewhere, `sh`.
pub(crate) fn default_shell() -> Vec<String> {
    if !cfg!(windows) {
        return vec!["sh".into()];
    }
    let program = if program_exists("pwsh") {
        "pwsh"
    } else {
        "powershell"
    };
    vec![program.into(), "-NoLogo".into()]
}

/// Whether `program` resolves to an executable: an explicit path that exists, or a
/// bare name found on `PATH` (trying `PATHEXT` extensions on Windows, so `:shell nu`
/// matches `nu.exe`). Used to reject a `:shell` typo before it breaks `:te`.
pub(crate) fn program_exists(program: &str) -> bool {
    use std::path::{Path, PathBuf};
    let exts: Vec<String> = if cfg!(windows) {
        let mut v = vec![String::new()];
        let pe = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        v.extend(
            pe.split(';')
                .filter(|s| !s.is_empty())
                .map(|s| s.to_lowercase()),
        );
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

/// Classify a shell `argv[0]` (possibly a full path, possibly `.exe`-suffixed).
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

/// Encode one mouse report for a terminal program. `code` is xterm's button code
/// with its extra bits already added: 0/1/2 left/middle/right, 3 no button, 64/65
/// wheel up/down, +4 Shift, +8 Alt, +16 Ctrl, +32 motion. `cell` is the 1-based
/// (col, row). A release keeps its button in SGR (`…m`) but is button 3 in the
/// older encodings, which can't say which button went up.
fn encode_mouse(
    encoding: pty_term::MouseEncoding,
    code: u8,
    (col, row): (u32, u32),
    release: bool,
) -> Vec<u8> {
    use pty_term::MouseEncoding;
    if encoding == MouseEncoding::Sgr {
        let end = if release { 'm' } else { 'M' };
        return format!("\x1b[<{code};{col};{row}{end}").into_bytes();
    }
    let code = if release { (code & !0b11) | 3 } else { code };
    let mut out = vec![0x1b, b'[', b'M', 32 + code];
    for v in [col, row] {
        let v = 32 + v;
        if encoding == MouseEncoding::Utf8 {
            // Up to 2015 fits in two UTF-8 bytes; past that it clamps.
            let ch = char::from_u32(v.min(2047)).unwrap_or(' ');
            let mut b = [0; 4];
            out.extend_from_slice(ch.encode_utf8(&mut b).as_bytes());
        } else {
            out.push(v.min(255) as u8);
        }
    }
    out
}

/// Encode a key event into the bytes a PTY expects (the inverse of what xterm.js
/// used to do). Covers printable input, Ctrl-combos → control codes, Enter/Tab/
/// Backspace/Esc, the cursor/navigation keys (honoring DECCKM app-cursor mode) and
/// F1–F12.
/// `None` for keys with no terminal meaning. Alt prefixes the sequence with ESC.
fn encode_term_key(
    key: &Key,
    ctrl: bool,
    alt: bool,
    shift: bool,
    app_cursor: bool,
) -> Option<Vec<u8>> {
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
    let ss3 = |c: u8| -> Vec<u8> {
        if modf > 1 {
            format!("\x1b[1;{modf}{}", c as char).into_bytes()
        } else {
            vec![0x1b, b'O', c]
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
        // Ctrl+Backspace is ^H, the "delete word backward" key in PowerShell, bash and
        // fish (Windows Terminal sends it too); plain Backspace is DEL.
        Key::Backspace if ctrl => vec![0x08],
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
        // F1–F4 are SS3 keys (`ESC O P`), switching to CSI when modified (`ESC[1;2P`).
        Key::F1 => ss3(b'P'),
        Key::F2 => ss3(b'Q'),
        Key::F3 => ss3(b'R'),
        Key::F4 => ss3(b'S'),
        // F5–F12 are numbered tilde keys; xterm skips 16 and 22.
        Key::F5 => tilde(15),
        Key::F6 => tilde(17),
        Key::F7 => tilde(18),
        Key::F8 => tilde(19),
        Key::F9 => tilde(20),
        Key::F10 => tilde(21),
        Key::F11 => tilde(23),
        Key::F12 => tilde(24),
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

/// Whether a key pressed with Ctrl+Alt is really AltGr typing a character: Windows
/// reports AltGr as Ctrl+Alt, and the layout then made a different character than
/// the bare key (`/` from AltGr+Q on a Brazilian layout, `@` on a German one). A
/// letter with Ctrl+Alt that the layout left alone is a real chord (Meta+Ctrl+Q).
fn is_altgr(logical: &Key, unmodified: &Key, ctrl: bool, alt: bool) -> bool {
    match (logical, unmodified) {
        (Key::Character(typed), Key::Character(bare)) if ctrl && alt => {
            typed.to_lowercase() != bare.to_lowercase()
        }
        _ => false,
    }
}

/// Control code for `Ctrl+<char>`: `Ctrl+A`→0x01 … `Ctrl+Z`→0x1a, `Ctrl+[`→ESC,
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
    fn a_resize_waits_until_the_wanted_size_stops_changing() {
        let t0 = Instant::now();
        let step = TERM_RESIZE_DEBOUNCE / 2;
        let mut d = ResizeDebounce::default();
        assert_eq!(d.settle(vec![(0, 80, 24)], t0), None);
        assert_eq!(d.deadline(), Some(t0 + TERM_RESIZE_DEBOUNCE));
        // A new size restarts the wait.
        assert_eq!(d.settle(vec![(0, 90, 24)], t0 + step), None);
        assert_eq!(d.settle(vec![(0, 90, 24)], t0 + step * 2), None);
        assert_eq!(
            d.settle(vec![(0, 90, 24)], t0 + step * 3),
            Some(vec![(0, 90, 24)])
        );
        assert_eq!(d.deadline(), None);
    }

    #[test]
    fn nothing_to_resize_drops_a_pending_one() {
        let t0 = Instant::now();
        let mut d = ResizeDebounce::default();
        d.settle(vec![(1, 80, 24)], t0);
        assert_eq!(d.settle(Vec::new(), t0), None);
        assert_eq!(d.deadline(), None);
    }

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
        assert_eq!(
            wsl_target("\\\\wsl$\\Debian"),
            Some((Some("Debian".into()), "/".into()))
        );
        // A bare Linux path (OSC 7 from inside WSL) → default distro.
        assert_eq!(wsl_target("/home/kayf"), Some((None, "/home/kayf".into())));
        // Ordinary Windows dirs are not WSL.
        assert_eq!(wsl_target("C:\\projects\\browser"), None);
        assert_eq!(wsl_target("\\\\server\\share\\dir"), None);
    }

    #[cfg(windows)]
    #[test]
    fn default_shell_is_an_installed_powershell() {
        let shell = default_shell();
        assert!(matches!(shell[0].as_str(), "pwsh" | "powershell"));
        assert!(program_exists(&shell[0]));
        assert!(matches!(shell_kind(&shell[0]), ShellKind::Pwsh));
    }

    /// Modified arrows/nav keys must use the xterm CSI forms (`ESC[1;5C` =
    /// Ctrl+Right) — plain sequences made Ctrl+arrows move one char in the shell
    /// instead of jumping words.
    #[test]
    fn modified_keys_use_xterm_csi() {
        let enc =
            |key, ctrl, alt, shift, app| encode_term_key(&key, ctrl, alt, shift, app).unwrap();
        assert_eq!(enc(Key::ArrowRight, false, false, false, false), b"\x1b[C");
        assert_eq!(
            enc(Key::ArrowRight, true, false, false, false),
            b"\x1b[1;5C"
        );
        assert_eq!(enc(Key::ArrowLeft, true, false, false, false), b"\x1b[1;5D");
        assert_eq!(
            enc(Key::ArrowRight, false, false, true, false),
            b"\x1b[1;2C"
        );
        assert_eq!(
            enc(Key::ArrowRight, false, true, false, false),
            b"\x1b[1;3C"
        );
        assert_eq!(enc(Key::ArrowRight, true, false, true, false), b"\x1b[1;6C");
        // Application-cursor mode (DECCKM) only changes the UNmodified form.
        assert_eq!(enc(Key::ArrowRight, false, false, false, true), b"\x1bOC");
        assert_eq!(enc(Key::ArrowRight, true, false, false, true), b"\x1b[1;5C");
        // Modified editing keys keep their number with the modifier appended.
        assert_eq!(enc(Key::Delete, false, false, false, false), b"\x1b[3~");
        assert_eq!(enc(Key::Delete, true, false, false, false), b"\x1b[3;5~");
        assert_eq!(enc(Key::Home, true, false, false, false), b"\x1b[1;5H");
    }

    /// F1–F12 reach the program as xterm sends them (htop quits on F10, Midnight
    /// Commander lives on them). F1–F4 are SS3 (`ESC O P`) unmodified and CSI with a
    /// modifier; F5–F12 are numbered tilde keys with the gap at 16 and 22.
    #[test]
    fn function_keys_use_xterm_sequences() {
        let enc = |key, ctrl, alt, shift| encode_term_key(&key, ctrl, alt, shift, false).unwrap();
        assert_eq!(enc(Key::F1, false, false, false), b"\x1bOP");
        assert_eq!(enc(Key::F4, false, false, false), b"\x1bOS");
        assert_eq!(enc(Key::F1, false, false, true), b"\x1b[1;2P");
        assert_eq!(enc(Key::F5, false, false, false), b"\x1b[15~");
        assert_eq!(enc(Key::F6, false, false, false), b"\x1b[17~");
        assert_eq!(enc(Key::F10, false, false, false), b"\x1b[21~");
        assert_eq!(enc(Key::F11, false, false, false), b"\x1b[23~");
        assert_eq!(enc(Key::F12, false, false, false), b"\x1b[24~");
        assert_eq!(enc(Key::F12, true, false, false), b"\x1b[24;5~");
        // Alt doesn't add a second ESC in front of a sequence that has one.
        assert_eq!(enc(Key::F2, false, true, false), b"\x1b[1;3Q");
    }

    /// The cursor shows for the first half of each blink, hides for the second, and
    /// stops blinking, shown, once things have been quiet for the timeout.
    #[test]
    fn the_blink_alternates_then_settles_shown() {
        let ms = Duration::from_millis;
        assert_eq!(blink_phase(ms(0)), (true, Some(BLINK_HALF)));
        assert_eq!(blink_phase(ms(100)), (true, Some(BLINK_HALF - ms(100))));
        assert_eq!(blink_phase(BLINK_HALF), (false, Some(BLINK_HALF)));
        assert!(blink_phase(BLINK_HALF * 2).0);
        // The last change lands exactly on the timeout...
        let near = BLINK_TIMEOUT - ms(10);
        assert_eq!(blink_phase(near).1, Some(ms(10)));
        // ...after which it stays shown and nothing more is scheduled.
        assert_eq!(blink_phase(BLINK_TIMEOUT), (true, None));
        assert_eq!(blink_phase(BLINK_TIMEOUT * 3), (true, None));
    }

    /// A notch (3 lines on Windows' default) is one wheel report and three lines;
    /// touchpad fractions add up; reversing drops the old direction's remainder.
    #[test]
    fn wheel_movement_becomes_whole_steps() {
        let mut acc = 0.0;
        assert_eq!(
            take_wheel_steps(&mut acc, 3.0, 3.0),
            1,
            "a notch is one report"
        );
        assert_eq!(take_wheel_steps(&mut acc, 3.0, 1.0), 3, "and three lines");
        assert_eq!(take_wheel_steps(&mut acc, -6.0, 3.0), -2);
        assert_eq!(acc, 0.0);
        // A touchpad's small deltas: nothing until a whole notch has built up.
        assert_eq!(take_wheel_steps(&mut acc, 1.2, 3.0), 0);
        assert_eq!(take_wheel_steps(&mut acc, 1.2, 3.0), 0);
        assert_eq!(take_wheel_steps(&mut acc, 1.2, 3.0), 1);
        assert!((acc - 0.6).abs() < 1e-9);
        // Turning around starts fresh rather than paying back the leftover.
        assert_eq!(take_wheel_steps(&mut acc, -1.0, 1.0), -1);
    }

    /// Mouse reports in the three encodings programs ask for: SGR keeps the button
    /// on release and writes plain numbers; X10 adds 32 to everything and reports
    /// every release as button 3; UTF-8 is X10 with big coordinates UTF-8 encoded.
    #[test]
    fn mouse_reports_follow_the_programs_encoding() {
        use pty_term::MouseEncoding::*;
        assert_eq!(encode_mouse(Sgr, 0, (5, 3), false), b"\x1b[<0;5;3M");
        assert_eq!(encode_mouse(Sgr, 0, (5, 3), true), b"\x1b[<0;5;3m");
        assert_eq!(encode_mouse(Sgr, 2 | 16, (1, 1), false), b"\x1b[<18;1;1M");
        assert_eq!(encode_mouse(Sgr, 32, (300, 2), false), b"\x1b[<32;300;2M");
        assert_eq!(encode_mouse(X10, 0, (5, 3), false), b"\x1b[M\x20\x25\x23");
        assert_eq!(encode_mouse(X10, 2, (5, 3), true), b"\x1b[M\x23\x25\x23");
        assert_eq!(encode_mouse(X10, 65, (1, 1), false), b"\x1b[M\x61\x21\x21");
        // X10 can't go past column 223.
        assert_eq!(encode_mouse(X10, 0, (500, 1), false), b"\x1b[M\x20\xff\x21");
        // UTF-8 writes column 200 (232) as two bytes.
        assert_eq!(
            encode_mouse(Utf8, 0, (200, 1), false),
            b"\x1b[M\x20\xc3\xa8\x21"
        );
    }

    /// AltGr is Ctrl+Alt on Windows, so a character typed with it (`/` and `?` on
    /// Brazilian keyboards, `@ { [ ] } \` on German ones) arrives with both held.
    /// It's text when the layout made a different character than the bare key;
    /// Ctrl+Alt+Q on a US layout (still `q`) stays a chord.
    #[test]
    fn altgr_characters_are_text_not_chords() {
        let ch = |s: &'static str| Key::Character(s);
        assert!(is_altgr(&ch("/"), &ch("q"), true, true));
        assert!(is_altgr(&ch("@"), &ch("q"), true, true));
        assert!(!is_altgr(&ch("q"), &ch("q"), true, true));
        assert!(
            !is_altgr(&ch("Q"), &ch("q"), true, true),
            "Shift isn't AltGr"
        );
        assert!(!is_altgr(&ch("/"), &ch("q"), true, false), "needs Alt too");
        assert!(!is_altgr(&Key::F1, &Key::F1, true, true));
    }

    /// Ctrl+Backspace sends ^H, which PowerShell, bash and fish read as "delete the
    /// word before the cursor" (as in Windows Terminal); plain Backspace stays DEL.
    #[test]
    fn ctrl_backspace_deletes_a_word() {
        let enc = |ctrl| encode_term_key(&Key::Backspace, ctrl, false, false, false).unwrap();
        assert_eq!(enc(false), b"\x7f");
        assert_eq!(enc(true), b"\x08");
    }
}
