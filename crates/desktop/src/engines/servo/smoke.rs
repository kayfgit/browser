//! Opt-in native integration check, using isolated profiles and a loopback fixture.
use super::*;
use std::{
    io::{Read, Write},
    net::TcpListener,
    time::Duration,
};
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN, WM_KEYUP},
};
thread_local! { static SMOKE: RefCell<Option<Smoke>> = const { RefCell::new(None) }; }
struct Smoke {
    step: u8,
    due: Instant,
    deadline: Instant,
    proof: Option<Result<String, String>>,
    url: String,
    retired: Option<ViewId>,
}

pub(crate) fn start(app: &mut App) -> anyhow::Result<()> {
    if std::env::var_os("BROWSER_SERVO_SMOKE_LOG").is_none() {
        return Ok(());
    }
    if std::env::var_os("BROWSER_SERVO_DATA_DIR").is_none()
        || std::env::var_os("BROWSER_WEBVIEW2_DATA_DIR").is_none()
    {
        anyhow::bail!("Servo smoke requires both isolated data-directory overrides");
    }
    // Start-Process's hidden startup flag can suppress the first ShowWindow.
    // Exercise real native painting, including the compositor's present path.
    app.window.set_visible(false);
    app.window.set_visible(true);
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let split = std::env::var("BROWSER_SERVO_SMOKE_SCENARIO").unwrap_or_default();
    let url = if split == "Example" {
        "https://example.com/".to_owned()
    } else {
        format!("http://{}/", listener.local_addr()?)
    };
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = "<!doctype html><meta charset=utf-8><title>Main engine integration</title><input id=field><button id=button onclick=\"this.textContent='clicked'\">Test</button><a href='/two'>Next page</a><p>Mixed engine fixture</p>";
            let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        }
    });
    // These settings stay in memory; no config/session write occurs in this test.
    app.config.engine = Some("webview2".into());
    if split.is_empty() || split == "Default" {
        app.adblock.set(crate::AdblockMode::Off);
    }
    app.open_web_provider(&url, false, false, true, false, "webview2");
    SMOKE.with(|s| {
        *s.borrow_mut() = Some(Smoke {
            step: if split == "Split" || split == "Example" {
                20
            } else {
                0
            },
            due: Instant::now(),
            deadline: Instant::now() + Duration::from_secs(30),
            proof: None,
            url,
            retired: None,
        })
    });
    log("START main-browser Servo qualification");
    Ok(())
}
pub(crate) fn reply(result: Result<String, String>) {
    SMOKE.with(|s| {
        if let Some(s) = s.borrow_mut().as_mut() {
            s.proof = Some(result);
        }
    });
}
pub(crate) fn tick(app: &mut App) -> bool {
    if app.quit {
        return false;
    }
    SMOKE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(smoke) = slot.as_mut() else {
            return false;
        };
        if let Err(error) = smoke.tick(app) {
            log(&format!("FAIL stage {}: {error}", smoke.step));
            app.quit = true;
        }
        true
    })
}
pub(super) fn log(text: &str) {
    if let Some(path) = std::env::var_os("BROWSER_SERVO_SMOKE_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(f, "{text}");
        }
    }
}
impl Smoke {
    fn next(&mut self, step: u8) {
        log(&format!("stage {} complete", self.step));
        self.step = step;
        self.proof = None;
        self.due = Instant::now() + Duration::from_millis(400);
        self.deadline = Instant::now() + Duration::from_secs(30);
    }
    fn proved(&mut self) -> Result<bool, String> {
        match self.proof.take() {
            Some(Ok(s)) if s == "true" => Ok(true),
            Some(other) => Err(format!("DOM assertion: {other:?}")),
            None => Ok(false),
        }
    }
    fn tick(&mut self, app: &mut App) -> Result<(), String> {
        if Instant::now() > self.deadline {
            return Err(format!("Timed out; shell status: {}", app.status.text()));
        }
        if Instant::now() < self.due {
            return Ok(());
        }
        let step = self.step;
        match step {
            20 if settled(app, "webview2") => {
                app.split_pane(crate::panes::SplitDir::Row);
                app.open_web_provider(&self.url, false, false, false, false, "webview2");
                self.next(21);
            }
            21 if settled(app, "webview2") => {
                if !app.is_split() {
                    return Err("Missing initial WebView2 split".into());
                }
                app.enter_command("engine servo");
                key(&app.window, 0x0d, 0x1c, false)?;
                self.next(22);
            }
            22 if settled(app, "servo") => {
                check_geometry(app)?;
                key(&app.window, 0x0d, 0x1c, true)?;
                evaluate(
                    app,
                    "String(document.body.innerText.length > 0 && innerWidth > 0)",
                )?;
                self.next(23);
            }
            23 if self.proved()? => {
                if !app.is_split() || views().len() != 1 {
                    return Err("Cold switch did not preserve the mixed split".into());
                }
                // Chrome-only updates must remain independent of Servo painting.
                app.enter_command("open ");
                for _ in 0..20 {
                    app.command.push('a');
                    app.command_cursor = app.command.len();
                    app.draw().map_err(|e| e.to_string())?;
                }
                app.cancel_command();
                check_geometry(app)?;
                if !app
                    .sample_res_lines()
                    .iter()
                    .any(|line| line.contains("Servo CPU/memory/I/O"))
                {
                    return Err("Resource monitor omitted Servo accounting".into());
                }
                app.enter_command("engine webview2");
                key(&app.window, 0x0d, 0x1c, false)?;
                self.next(24);
            }
            24 if settled(app, "webview2") => {
                key(&app.window, 0x0d, 0x1c, true)?;
                if !app.is_split() || !views().is_empty() {
                    return Err("Command-bar switch back did not retain the WebView2 split".into());
                }
                log("PASS visible cold Servo switch: WebView2 split, native command-bar switches, first frame, page content and switch back");
                app.quit = true;
                self.next(25);
            }
            0 if settled(app, "webview2") => {
                app.switch_engine("servo");
                require_provider(app, "servo")?;
                self.next(1);
            }
            1 if settled(app, "servo") => {
                let id = app.active_webview().ok_or("Missing view")?.identity().id;
                if app
                    .build_provider_view("servo", Source::Url(self.url.clone()), false, "", true)
                    .is_ok()
                    || app
                        .build_provider_view(
                            "servo",
                            Source::Url(self.url.clone()),
                            true,
                            "",
                            false,
                        )
                        .is_ok()
                    || app.active_webview().ok_or("Lost view")?.identity().id != id
                {
                    return Err("Unsupported request changed the live pane".into());
                }
                app.mode = ModeKind::Normal;
                app.set_page_mode("normal");
                app.reclaim_shell_focus();
                key(&app.window, 0x46, 0x21, false)?;
                self.next(2);
            }
            2 => {
                key(&app.window, 0x46, 0x21, true)?;
                evaluate(app, "String(window.__hintMap?.a?.el?.id === 'field')")?;
                self.next(3);
            }
            3 if self.proved()? => {
                key(&app.window, 0x41, 0x1e, false)?;
                self.next(4);
            }
            4 => {
                let page = active(app)?;
                key(&page.window, 0x41, 0x1e, true)?;
                if app.mode != ModeKind::Insert {
                    return Err("Hint did not enter Insert".into());
                }
                evaluate(app,"String(document.activeElement.id === 'field' && document.querySelector('input').value === '')")?;
                self.next(5);
            }
            5 if self.proved()? => {
                key(&active(app)?.window, 0x42, 0x30, false)?;
                self.next(6);
            }
            6 => {
                key(&active(app)?.window, 0x42, 0x30, true)?;
                evaluate(app, "String(document.querySelector('input').value === 'b')")?;
                self.next(7);
            }
            7 if self.proved()? => {
                app.exit_to_normal();
                app.split_pane(crate::panes::SplitDir::Row);
                app.open_web_provider(
                    &format!("{}two", self.url),
                    false,
                    false,
                    false,
                    false,
                    "servo",
                );
                require_provider(app, "servo")?;
                self.next(8);
            }
            8 if settled(app, "servo") => {
                if views().len() != 2
                    || RUNTIME.with(|r| r.borrow().as_ref().map(Rc::strong_count)) != Some(3)
                {
                    return Err("Two panes did not share one runtime".into());
                }
                self.retired = Some(active(app)?.page.identity().id);
                app.switch_engine("webview2");
                require_provider(app, "webview2")?;
                self.next(9);
            }
            9 if settled(app, "webview2") => {
                if !app.is_split() || views().len() != 1 {
                    return Err("Mixed split did not retain the other Servo view".into());
                }
                app.exit_to_normal();
                let id = self.retired.ok_or("Missing retired ID")?;
                let _ = app.proxy.send_event(UserEvent::Engine {
                    view: id,
                    event: Box::new(UserEvent::HintEdit),
                });
                post(
                    &app.proxy,
                    Event::Reply {
                        view: id,
                        epoch: 1,
                        result: Err("stale reply must be ignored".into()),
                    },
                );
                self.next(10);
            }
            10 => {
                if app.mode != ModeKind::Normal || views().len() != 1 {
                    return Err("Retired callback affected the active pane".into());
                }
                app.close_active();
                require_provider(app, "servo")?;
                app.switch_engine("webview2");
                require_provider(app, "webview2")?;
                self.next(11);
            }
            11 if settled(app, "webview2") => {
                if !views().is_empty()
                    || RUNTIME.with(|r| r.borrow().as_ref().map(Rc::strong_count)) != Some(1)
                {
                    return Err("Closed Servo views retained runtime references".into());
                }
                app.switch_engine("servo");
                require_provider(app, "servo")?;
                self.next(12);
            }
            12 if settled(app, "servo") => {
                app.close_active();
                if !views().is_empty()
                    || RUNTIME.with(|r| r.borrow().as_ref().map(Rc::strong_count)) != Some(1)
                {
                    return Err("Reopened Servo view did not release its resources".into());
                }
                log("PASS main browser: engine switches, hints/input, shared runtime, mixed split, retired callbacks, unsupported modes and last-view close/reopen");
                app.quit = true;
                self.next(13);
            }
            _ => {}
        }
        Ok(())
    }
}
fn require_provider(app: &App, provider: &str) -> Result<(), String> {
    if app
        .active_webview()
        .is_some_and(|v| v.identity().provider == provider)
    {
        Ok(())
    } else {
        Err(format!("Expected {provider}: {}", app.status.text()))
    }
}
fn check_geometry(app: &App) -> Result<(), String> {
    use windows::Win32::{Foundation::POINT, Graphics::Gdi::ClientToScreen};
    let page = active(app)?;
    let mut child = POINT::default();
    let mut parent = POINT::default();
    unsafe {
        if !ClientToScreen(native::hwnd(&page.window), &mut child).as_bool()
            || !ClientToScreen(native::hwnd(&app.window), &mut parent).as_bool()
        {
            return Err("Could not measure native child bounds".into());
        }
    }
    let rect = app.focused_pane_rect();
    let inset = if app.is_split() {
        crate::panes::FOCUS_BORDER
    } else {
        0
    };
    let size = page.window.inner_size();
    let actual = (
        child.x - parent.x,
        child.y - parent.y,
        size.width as i32,
        size.height as i32,
    );
    let expected = (
        rect.x + inset,
        rect.y + inset,
        (rect.w - 2 * inset).max(1),
        (rect.h - 2 * inset).max(1),
    );
    if actual != expected {
        return Err(format!(
            "Servo client bounds {actual:?} differ from pane {expected:?}"
        ));
    }
    log("PASS geometry: Servo client exactly fills its inset pane");
    Ok(())
}
fn settled(app: &App, provider: &str) -> bool {
    app.active.and_then(|i| app.tabs.get(i)).is_some_and(|tab| {
        tab.provider() == Some(provider)
            && tab.page_state().is_some_and(|p| p.loading_for().is_none())
            && (provider != "servo"
                || tab
                    .webview()
                    .and_then(|v| view(v.identity().id))
                    .is_some_and(|v| v.painted.get()))
    })
}
fn active(app: &App) -> Result<Rc<State>, String> {
    view(app.active_webview().ok_or("No active view")?.identity().id)
        .ok_or("No active Servo view".into())
}
fn evaluate(app: &App, script: &str) -> Result<(), String> {
    let proxy = app.proxy.clone();
    active(app)?
        .page
        .raw()
        .evaluate_javascript(script, move |result| {
            let result = match result {
                Ok(servo::JSValue::String(s)) => Ok(s),
                other => Err(format!("{other:?}")),
            };
            post(&proxy, Event::SmokeReply(result));
        });
    Ok(())
}
fn key(window: &Window, vk: usize, scan: usize, release: bool) -> Result<(), String> {
    let bits = 1 | (scan << 16) | if release { (1 << 30) | (1 << 31) } else { 0 };
    unsafe {
        PostMessageW(
            Some(native::hwnd(window)),
            if release { WM_KEYUP } else { WM_KEYDOWN },
            WPARAM(vk),
            LPARAM(bits as isize),
        )
    }
    .map_err(|e| e.to_string())
}
