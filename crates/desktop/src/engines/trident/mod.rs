//! The Trident provider: Internet Explorer 11's engine (MSHTML), for testing old
//! intranet, government and bank sites. Built into Windows, so nothing to download.
//!
//! Each view is a separate, sandboxed helper process (see `host` and `sandbox`) whose
//! window the browser places in the pane. MSHTML is old and heavily targeted, so it
//! never runs inside the browser itself: an exploit stays in an AppContainer with no
//! access to the user's files or the browser's profile, and a crash takes down only
//! its own pane.
mod host;
mod protocol;
mod sandbox;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::{BufRead, BufReader, Write},
    rc::{Rc, Weak},
    sync::Mutex,
};

use browser_engine::{EngineResult, EngineView, History, RectPx, Source, ViewId, ViewIdentity};
use tao::{event_loop::EventLoopProxy, platform::windows::WindowExtWindows, window::Window};
use windows::Win32::{
    Foundation::{CloseHandle, HANDLE, HWND},
    System::Threading::{TerminateProcess, WaitForSingleObject},
    UI::{
        Input::KeyboardAndMouse::SetFocus,
        WindowsAndMessaging::{
            SetParent, SetWindowLongPtrW, SetWindowPos, ShowWindow, GWL_STYLE, SWP_FRAMECHANGED,
            SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOWNA, WS_CHILD, WS_CLIPCHILDREN,
            WS_CLIPSIBLINGS,
        },
    },
};

use super::{events::decode_page_message, PageEventProxy, WebView2Options};
use crate::{App, UserEvent};
pub(crate) use host::run_if_helper;
use protocol::Command;

/// The argument that starts this executable as a Trident helper.
const HOST_FLAG: &str = "--trident-host";

/// What a helper's reader thread hands the UI thread.
pub(crate) enum Event {
    Helper(ViewId, protocol::Event),
    /// The helper's output ended: it quit or crashed.
    Exited(ViewId),
}

struct State {
    identity: ViewIdentity,
    page: crate::tabs::PageState,
    scoped: PageEventProxy,
    parent: HWND,
    stdin: Mutex<std::fs::File>,
    process: HANDLE,
    hwnd: Cell<Option<HWND>>,
    bounds: Cell<RectPx>,
    visible: Cell<bool>,
    url: RefCell<String>,
    back: Cell<bool>,
    forward: Cell<bool>,
    closing: Cell<bool>,
}

thread_local! {
    static VIEWS: RefCell<HashMap<ViewId, Weak<State>>> = RefCell::new(HashMap::new());
    /// Whether this run has shown the "testing only" warning yet.
    static WARNED: Cell<bool> = const { Cell::new(false) };
}

impl State {
    fn send(&self, command: Command) -> EngineResult {
        let mut stdin = self.stdin.lock().map_err(|_| "Trident pipe poisoned")?;
        stdin
            .write_all(protocol::encode(&command).as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(|e| format!("Trident helper is gone: {e}"))
    }

    fn place(&self) {
        let Some(hwnd) = self.hwnd.get() else { return };
        let r = self.bounds.get();
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                r.x,
                r.y,
                r.w.max(1) as i32,
                r.h.max(1) as i32,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            let _ = ShowWindow(
                hwnd,
                if self.visible.get() {
                    SW_SHOWNA
                } else {
                    SW_HIDE
                },
            );
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        self.closing.set(true);
        let _ = self.send(Command::Quit);
        // Closing stdin also tells it to quit; if it hangs anyway, end it.
        let process = self.process.0 as isize;
        std::thread::spawn(move || unsafe {
            let process = HANDLE(process as *mut _);
            if WaitForSingleObject(process, 3000).0 != 0 {
                let _ = TerminateProcess(process, 1);
            }
            let _ = CloseHandle(process);
        });
    }
}

struct View(Rc<State>);

impl Drop for View {
    fn drop(&mut self) {
        VIEWS.with(|views| views.borrow_mut().remove(&self.0.identity.id));
    }
}

impl EngineView for View {
    fn identity(&self) -> &ViewIdentity {
        &self.0.identity
    }
    fn load_url(&self, url: &str) -> EngineResult {
        self.0.send(Command::Navigate { url: url.into() })
    }
    fn reload(&self) -> EngineResult {
        self.0.send(Command::Reload)
    }
    fn url(&self) -> EngineResult<String> {
        Ok(self.0.url.borrow().clone())
    }
    fn set_bounds(&self, rect: RectPx) -> EngineResult {
        self.0.bounds.set(rect);
        self.0.place();
        Ok(())
    }
    fn set_visible(&self, visible: bool) -> EngineResult {
        self.0.visible.set(visible);
        self.0.place();
        Ok(())
    }
    fn zoom(&self, factor: f64) -> EngineResult {
        self.0.send(Command::Zoom {
            percent: (factor * 100.0).round() as i32,
        })
    }
    fn focus(&self) -> EngineResult {
        self.0.send(Command::Focus)
    }
    fn focus_parent(&self) -> EngineResult {
        unsafe { SetFocus(Some(self.0.parent)) }
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    fn evaluate_script(&self, script: &str) -> EngineResult {
        self.0.send(Command::Eval {
            script: script.into(),
        })
    }
    fn trusted_click(&self, x: f64, y: f64) -> EngineResult {
        self.0.send(Command::Click { x, y })
    }
    fn history(&self) -> Option<&dyn History> {
        Some(self)
    }
}

impl History for View {
    fn can_go(&self, forward: bool) -> bool {
        if forward {
            self.0.forward.get()
        } else {
            self.0.back.get()
        }
    }
    fn go(&self, forward: bool) -> EngineResult {
        self.0.send(Command::Go { forward })
    }
}

/// The scripts every top-level document gets: the IE11 prelude (which also connects
/// `window.__post` to the shell), then the same shell bridge the other engines run.
fn init_script(opts: &WebView2Options<'_>) -> String {
    let (m, c, v, sb) = (opts.mute, opts.no_css, opts.no_video, opts.no_scrollbar);
    let mut init = format!(
        "{}\n{}\n{}\n{}\nwindow.__featureDefaults={{mute:{m},css:{c},video:{v},scrollbar:{sb}}};\n{}",
        include_str!("prelude.js"),
        crate::BRIDGE_JS,
        crate::FIND_JS,
        crate::CARET_JS,
        crate::FEATURES_JS,
    );
    if !opts.extra_init.is_empty() {
        init.push('\n');
        init.push_str(opts.extra_init);
    }
    // Diagnostics: BROWSER_TRIDENT_PROBE adds a script to every page (it can report
    // with `__post('dbg:…')`, which BROWSER_TRIDENT_LOG records).
    if let Ok(probe) = std::env::var("BROWSER_TRIDENT_PROBE") {
        init.push('\n');
        init.push_str(&probe);
    }
    init
}

pub(super) fn build(
    parent: &Rc<Window>,
    opts: WebView2Options<'_>,
    identity: ViewIdentity,
) -> anyhow::Result<(Box<dyn EngineView>, crate::tabs::PageState)> {
    let url = match &opts.source {
        Source::Url(url) => url.clone(),
        // Internal pages are the shell's own; Trident is for the web.
        Source::Html(_) => "about:blank".into(),
    };
    let helper = sandbox::spawn()?;
    let id = identity.id;
    let page = crate::tabs::PageState::default();
    page.begin_load();
    let state = Rc::new(State {
        identity,
        page: page.clone(),
        scoped: PageEventProxy::new(opts.proxy.clone(), id),
        parent: HWND(parent.hwnd() as *mut _),
        stdin: Mutex::new(helper.stdin),
        process: helper.process,
        hwnd: Cell::new(None),
        bounds: Cell::new(opts.bounds),
        visible: Cell::new(false),
        url: RefCell::new(url.clone()),
        back: Cell::new(false),
        forward: Cell::new(false),
        closing: Cell::new(false),
    });
    state
        .send(Command::Init {
            script: init_script(&opts),
        })
        .map_err(anyhow::Error::msg)?;
    state
        .send(Command::Navigate { url })
        .map_err(anyhow::Error::msg)?;
    read_events(helper.stdout, opts.proxy.clone(), id);
    VIEWS.with(|views| views.borrow_mut().insert(id, Rc::downgrade(&state)));
    Ok((Box::new(View(state)), page))
}

/// Forward a helper's events to the UI thread until its output ends.
fn read_events(stdout: std::fs::File, proxy: EventLoopProxy<UserEvent>, id: ViewId) {
    std::thread::spawn(move || {
        // Diagnostics: BROWSER_TRIDENT_LOG names a file for everything helpers say.
        let mut log = std::env::var_os("BROWSER_TRIDENT_LOG").and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(log) = log.as_mut() {
                let _ = writeln!(log, "[{id:?}] {line}");
            }
            if let Some(event) = protocol::decode::<protocol::Event>(&line) {
                if proxy
                    .send_event(UserEvent::Trident(Event::Helper(id, event)))
                    .is_err()
                {
                    return;
                }
            }
        }
        let _ = proxy.send_event(UserEvent::Trident(Event::Exited(id)));
    });
}

/// Act on a helper's event (UI thread).
pub(crate) fn on_event(app: &mut App, event: Event) {
    let (id, event) = match event {
        Event::Helper(id, event) => (id, Some(event)),
        Event::Exited(id) => (id, None),
    };
    let Some(state) = VIEWS.with(|views| views.borrow().get(&id).and_then(Weak::upgrade)) else {
        return;
    };
    let Some(event) = event else {
        if !state.closing.get() {
            state.page.end_load();
            let _ = state.scoped.send_event(UserEvent::EngineCrashed(
                "the Trident (Internet Explorer) helper stopped".into(),
            ));
        }
        return;
    };
    use protocol::Event as E;
    let send = |event: UserEvent| {
        let _ = state.scoped.send_event(event);
    };
    match event {
        E::Ready { hwnd } => {
            let hwnd = HWND(hwnd as isize as *mut _);
            unsafe {
                // A popup until now; make it a child of the browser's window.
                SetWindowLongPtrW(
                    hwnd,
                    GWL_STYLE,
                    (WS_CHILD | WS_CLIPCHILDREN | WS_CLIPSIBLINGS).0 as isize,
                );
                let _ = SetParent(hwnd, Some(state.parent));
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            state.hwnd.set(Some(hwnd));
            state.place();
            if !WARNED.with(|w| w.replace(true)) {
                app.set_status(
                    "Trident is IE11: for testing old sites only. No ad blocking, outdated security",
                );
            }
        }
        E::LoadStart => {
            state.page.begin_load();
            send(UserEvent::Redraw);
        }
        E::LoadEnd => {
            state.page.end_load();
            send(UserEvent::Redraw);
        }
        E::Url { url } => {
            if *state.url.borrow() != url {
                *state.url.borrow_mut() = url;
                send(UserEvent::UrlChanged { record: true });
            }
        }
        E::History { back, forward } => {
            state.back.set(back);
            state.forward.set(forward);
        }
        E::Message { body } => {
            if let Some(event) = decode_page_message(&body) {
                send(event);
            }
        }
        E::NewWindow { url } => send(UserEvent::HintOpen(url)),
        E::Notice { text } => app.set_status(text),
    }
}
