use browser_engine::{EngineView, RectPx, StorageMode, ViewRequirements, build_checked};
use browser_servo_lab::page_policy::DESCRIPTOR;
use euclid::Scale;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use servo::{RenderingContext, ServoBuilder, WebView, WebViewBuilder};
use std::{
    cell::{Cell, RefCell},
    error::Error,
    rc::Rc,
    time::{Duration, Instant},
};
use tao::{
    dpi::PhysicalSize,
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy},
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Window, WindowBuilder},
};

#[path = "bridge.rs"]
mod bridge;
#[path = "../../../crates/desktop/src/engines/servo/input.rs"]
mod input;
#[path = "../../../crates/desktop/src/engines/servo/native.rs"]
mod native;
#[path = "page.rs"]
mod page;
#[path = "page_smoke.rs"]
mod page_smoke;
#[path = "shell.rs"]
mod shell;
#[path = "smoke.rs"]
mod smoke;
const FIXTURE: &str = include_str!("../fixture.html");
const INIT: &str = "window.labInjected = true;";

#[derive(Clone, Debug)]
enum Message {
    Wake,
    ServoFrame,
    ServoCheck(bool),
    ServoScreenshot,
    WebView2Check(bool),
    Crashed(String),
    Cursor(servo::Cursor),
    ContextMenu,
    InputProbe(Result<String, String>),
    PageProbe(Result<String, String>),
    BridgeReply {
        epoch: u64,
        result: Result<String, String>,
    },
    BridgeProbe(Result<String, String>),
    ProbeFocusKey(tao::event::KeyEvent),
    BridgeFault {
        epoch: u64,
        error: String,
    },
}
#[derive(Clone)]
struct Waker(EventLoopProxy<Message>);
impl servo::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        let _ = self.0.send_event(Message::Wake);
    }
}

// Own neither Servo nor its WebView: avoid a runtime -> view -> delegate cycle.
struct Delegate {
    proxy: EventLoopProxy<Message>,
    smoke: bool,
    checked: Cell<bool>,
    menu: Rc<RefCell<Option<servo::ContextMenu>>>,
    navigation: Rc<bridge::Navigation>,
}
impl servo::WebViewDelegate for Delegate {
    fn notify_cursor_changed(&self, _: WebView, cursor: servo::Cursor) {
        let _ = self.proxy.send_event(Message::Cursor(cursor));
    }
    fn show_embedder_control(&self, _: WebView, control: servo::EmbedderControl) {
        if let servo::EmbedderControl::ContextMenu(menu) = control {
            *self.menu.borrow_mut() = Some(menu);
            let _ = self.proxy.send_event(Message::ContextMenu);
        }
    }
    fn hide_embedder_control(&self, _: WebView, id: servo::EmbedderControlId) {
        let mut menu = self.menu.borrow_mut();
        if menu.as_ref().is_some_and(|menu| menu.id() == id) {
            menu.take();
        }
    }
    fn notify_new_frame_ready(&self, _: WebView) {
        let _ = self.proxy.send_event(Message::ServoFrame);
    }
    fn notify_load_status_changed(&self, view: WebView, status: servo::LoadStatus) {
        self.navigation.status(status);
        if self.smoke && status == servo::LoadStatus::Complete && !self.checked.replace(true) {
            let proxy = self.proxy.clone();
            view.evaluate_javascript(
                "document.querySelector('.spinner').style.animation = 'none'; window.labReady === true && window.labScriptAtStart === true",
                move |result| {
                    let _ = proxy.send_event(Message::ServoCheck(matches!(
                        result,
                        Ok(servo::JSValue::Boolean(true))
                    )));
                },
            );
        }
    }
    fn notify_crashed(&self, _: WebView, reason: String, _: Option<String>) {
        let _ = self.proxy.send_event(Message::Crashed(reason));
    }
}

// Field order keeps native surfaces alive through view destruction and Servo shutdown.
struct Lab {
    page: page::ServoPage,
    webview2: wry::WebView,
    servo_window: Rc<Window>,
    _webview2_context: wry::WebContext,
    parent: Rc<Window>,
}
impl Lab {
    fn resize(&self) -> Result<(), Box<dyn Error>> {
        let size = self.parent.inner_size();
        let half = (size.width / 2).max(1);
        let height = size.height.max(1);
        self.page.set_bounds(RectPx {
            x: 0,
            y: 0,
            w: half,
            h: height,
        })?;
        self.webview2.set_bounds(wry::Rect {
            position: wry::dpi::PhysicalPosition::new(half as i32, 0).into(),
            size: wry::dpi::PhysicalSize::new(size.width.saturating_sub(half).max(1), height)
                .into(),
        })?;
        Ok(())
    }
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let smoke = args.iter().any(|arg| arg == "--smoke");
    if args.len() > 1
        || args
            .first()
            .is_some_and(|arg| arg.starts_with('-') && arg != "--smoke")
    {
        return Err("Usage: browser-servo-lab [--smoke | https://example.com]".into());
    }
    let url = if let Some(arg) = args.first().filter(|_| !smoke) {
        let url = url::Url::parse(arg)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err("Use an HTTP(S) URL".into());
        }
        Some(url)
    } else {
        None
    };
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .map_err(|_| "Could not install TLS provider")?;
    let mut event_loop = EventLoopBuilder::<Message>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let parent = Rc::new(
        WindowBuilder::new()
            .with_title("Servo 0.5 (left) | WebView2 (right) — experimental lab")
            .with_inner_size(PhysicalSize::new(1200, 800))
            .build(&event_loop)?,
    );
    let servo_window = Rc::new(
        WindowBuilder::new()
            .with_parent_window(parent.hwnd())
            .with_decorations(false)
            .with_inner_size(PhysicalSize::new(600, 800))
            .build(&event_loop)?,
    );
    let context = Rc::new(
        servo::WindowRenderingContext::new(
            servo_window.display_handle()?,
            servo_window.window_handle()?,
            dpi::PhysicalSize::new(600, 800),
        )
        .map_err(|error| format!("Servo rendering context: {error:?}"))?,
    );
    context
        .make_current()
        .map_err(|error| format!("Make current: {error:?}"))?;

    // Fresh, distinct profiles per run; never use the daily browser's data.
    let data = std::env::current_exe()?
        .parent()
        .ok_or("Missing executable directory")?
        .join("lab-data")
        .join(format!(
            "{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_nanos()
        ));
    std::fs::create_dir_all(data.join("servo"))?;
    std::fs::create_dir_all(data.join("webview2"))?;
    eprintln!("Lab profiles: {}", data.display());
    let servo = Rc::new(
        ServoBuilder::default()
            .opts(servo::Opts {
                config_dir: Some(data.join("servo")),
                ..Default::default()
            })
            .event_loop_waker(Box::new(Waker(proxy.clone())))
            .build(),
    );
    let scripts = Rc::new(servo::UserContentManager::new(&servo));
    scripts.add_script(Rc::new(
        format!(
            "{INIT}\nif (window === window.top) {{\n{}\n{}\n}}",
            bridge::PRELUDE,
            bridge::SHELL
        )
        .into(),
    ));
    let fixture_url = url::Url::parse(&format!(
        "data:text/html;charset=utf-8,{}",
        url::form_urlencoded::byte_serialize(FIXTURE.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    ))?;
    let menu = Rc::new(RefCell::new(None));
    let navigation = Rc::new(bridge::Navigation::default());
    let page = build_checked(
        &DESCRIPTOR,
        ViewRequirements {
            storage: StorageMode::Persistent,
            disable_javascript: false,
            // Only this lab's top-level bridge is qualified, not production page messaging.
            shell_bridge: false,
        },
        |identity| {
            let servo_view = WebViewBuilder::new(&servo, context.clone())
                .url(url.clone().unwrap_or(fixture_url))
                .hidpi_scale_factor(Scale::new(servo_window.scale_factor() as f32))
                .user_content_manager(scripts)
                .delegate(Rc::new(Delegate {
                    proxy: proxy.clone(),
                    smoke,
                    checked: Cell::new(false),
                    menu: menu.clone(),
                    navigation: navigation.clone(),
                }))
                .build();
            Ok(page::ServoPage::new(
                identity,
                servo_view,
                servo.clone(),
                context.clone(),
                servo_window.clone(),
                parent.clone(),
            ))
        },
    )?;
    // The adapter now owns the runtime and rendering-context lifetime.
    drop(servo);
    drop(context);
    let mut webview2_context = wry::WebContext::new(Some(data.join("webview2")));
    let ipc_proxy = proxy.clone();
    let builder = wry::WebViewBuilder::new_with_web_context(&mut webview2_context)
        .with_initialization_script(INIT)
        .with_ipc_handler(move |request| {
            if smoke {
                let _ = ipc_proxy.send_event(Message::WebView2Check(request.body() == "ready"));
            }
        });
    let webview2 = if let Some(url) = url {
        builder.with_url(url.as_str())
    } else {
        builder.with_html(FIXTURE)
    }
    .build_as_child(parent.as_ref())?;
    let mut lab = Some(Lab {
        page,
        webview2,
        servo_window,
        _webview2_context: webview2_context,
        parent,
    });
    lab.as_ref().unwrap().resize()?;

    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut servo_ok, mut webview2_ok, mut painted, mut captured) = (false, false, false, false);
    let screenshot = std::env::current_exe()?.with_file_name("servo-smoke.png");
    let mut failure = None;
    let mut input = input::Input::default();
    let mut cursor = servo::Cursor::Default;
    let mut input_smoke = smoke::InputSmoke::default();
    let mut page_smoke = page_smoke::PageSmoke::default();
    let mut bridge = bridge::Bridge::new(navigation.clone());
    let mut shell = shell::Shell::new(navigation, smoke);
    use tao::platform::run_return::EventLoopExtRunReturn;
    event_loop.run_return(|event, _, control| {
        *control = if smoke {
            ControlFlow::WaitUntil(deadline.min(Instant::now() + Duration::from_millis(50)))
        } else {
            ControlFlow::WaitUntil(bridge.wakeup())
        };
        let Some(state) = lab.as_ref() else {
            *control = ControlFlow::Exit;
            return;
        };
        match event {
            Event::UserEvent(Message::PageProbe(result)) => {
                if let Err(error) = page_smoke.reply(result) {
                    failure = Some(error);
                }
            }
            Event::UserEvent(Message::ProbeFocusKey(event)) => {
                // Replay Tao's focus-generated keydown using the actual hint key
                // captured by the smoke test, without injecting global OS input.
                if !shell.keyboard(state, &event, true) {
                    input.keyboard(state.page.raw(), &event);
                }
            }
            Event::UserEvent(Message::BridgeFault { epoch, error }) if epoch == bridge.epoch() => {
                if smoke {
                    failure = Some(error);
                } else {
                    state.parent.set_title(&error);
                    eprintln!("{error}");
                }
            }
            Event::UserEvent(Message::BridgeReply { epoch, result }) => {
                match bridge.reply(epoch, result) {
                    Ok(messages) => {
                        for message in messages {
                            shell.message(state, &message);
                        }
                    }
                    Err(error) => {
                        if smoke {
                            failure = Some(error);
                        } else {
                            state.parent.set_title(&format!("{error}; reload to retry"));
                            eprintln!("{error}");
                        }
                    }
                }
            }
            Event::UserEvent(Message::BridgeProbe(result)) => {
                if let Err(error) = shell.probe_reply(result) {
                    failure = Some(error);
                }
            }
            Event::UserEvent(Message::Cursor(value)) => {
                cursor = value;
                native::refresh_cursor(&state.servo_window, cursor);
            }
            Event::MainEventsCleared => native::refresh_cursor(&state.servo_window, cursor),
            Event::UserEvent(Message::ContextMenu) => {
                // Leave Servo's delegate call before entering a native modal loop.
                let request = menu.borrow_mut().take();
                if let Some(request) = request {
                    if smoke {
                        input_smoke.context_menu();
                    }
                    if let Err(error) =
                        native::show_context_menu(&state.servo_window, request, smoke)
                    {
                        if smoke {
                            failure = Some(format!("Context menu: {error}"));
                        } else {
                            eprintln!("Context menu: {error}");
                        }
                    }
                }
            }
            Event::UserEvent(Message::InputProbe(result)) => {
                if let Err(error) = input_smoke.result(state, result) {
                    failure = Some(error);
                }
            }
            Event::UserEvent(Message::Wake) => state.page.pump(),
            Event::UserEvent(Message::ServoFrame) => state.servo_window.request_redraw(),
            Event::UserEvent(Message::ServoCheck(ok)) => {
                servo_ok = ok;
                painted = false;
                state.servo_window.request_redraw();
                if !ok {
                    failure = Some("Servo document script/evaluation check failed".into());
                } else {
                    let proxy = proxy.clone();
                    let screenshot = screenshot.clone();
                    state.page.raw().take_screenshot(None, move |result| {
                        let result = result
                            .map_err(|error| format!("Screenshot: {error:?}"))
                            .and_then(|image| {
                                image.save(&screenshot).map_err(|error| error.to_string())
                            });
                        let message = match result {
                            Ok(()) => Message::ServoScreenshot,
                            Err(error) => Message::Crashed(error),
                        };
                        let _ = proxy.send_event(message);
                    });
                }
            }
            Event::UserEvent(Message::ServoScreenshot) => captured = true,
            Event::UserEvent(Message::WebView2Check(ok)) => {
                webview2_ok = ok;
                if !ok {
                    failure = Some("WebView2 document script check failed".into());
                }
            }
            Event::UserEvent(Message::Crashed(reason)) => failure = Some(reason),
            Event::RedrawRequested(id) if id == state.servo_window.id() => {
                if let Err(error) = state.page.paint() {
                    failure = Some(error);
                } else {
                    painted = true;
                }
            }
            Event::WindowEvent {
                window_id, event, ..
            } => {
                if matches!(event, WindowEvent::CloseRequested) {
                    if smoke {
                        failure = Some("Smoke test closed before completion".into());
                    }
                    *control = ControlFlow::Exit;
                } else if shell.key(state, window_id, &event) {
                    // Keys owned by the lab shell never enter the page's input path.
                } else if window_id == state.parent.id() {
                    if matches!(event, WindowEvent::Resized(_)) {
                        if let Err(error) = state.resize() {
                            failure = Some(error.to_string());
                        }
                    }
                } else if window_id == state.servo_window.id() {
                    input.handle(state.page.raw(), &state.servo_window, event);
                }
            }
            _ => {}
        }
        if smoke && Instant::now() >= deadline {
            failure = Some(format!(
                "Smoke test timed out (input done={}, shell={})",
                input_smoke.done(),
                shell.probe_status()
            ));
        }
        if smoke && servo_ok && webview2_ok && painted && captured && failure.is_none() {
            if let Err(error) = input_smoke.tick(state, &proxy) {
                failure = Some(error);
            }
        }
        bridge.tick(
            state.page.raw(),
            &proxy,
            native::focused(&state.servo_window) || native::focused(&state.parent),
        );
        if smoke && input_smoke.done() && failure.is_none() {
            if let Err(error) = shell.probe(state, &proxy) {
                failure = Some(error);
            }
        }
        if smoke && shell.probe_done() && failure.is_none() {
            if let Err(error) = page_smoke.tick(state, &proxy) {
                failure = Some(error);
            }
        }
        if failure.is_some()
            || (smoke && input_smoke.done() && shell.probe_done() && page_smoke.done())
        {
            *control = ControlFlow::Exit;
        }
        if !matches!(*control, ControlFlow::Exit) {
            // Reply handling changes the next poll time. Recompute after dispatch
            // so a completed read doesn't retain the in-flight 250 ms wakeup.
            *control = ControlFlow::WaitUntil(if smoke {
                bridge
                    .wakeup()
                    .min(deadline)
                    .min(Instant::now() + Duration::from_millis(50))
            } else {
                bridge.wakeup()
            });
        }
        if matches!(*control, ControlFlow::Exit) {
            // Release the views and join Servo's shutdown while native windows still exist.
            menu.borrow_mut().take();
            drop(lab.take());
        }
    });
    if let Some(error) = failure {
        return Err(error.into());
    }
    if smoke {
        println!(
            "PASS: both engines loaded and tore down; rendering, input, menus, shell/hints, strict CSP, focus isolation and EngineView navigation/history/zoom/bounds passed."
        );
        println!("Servo frame: {}", screenshot.display());
    }
    Ok(())
}
