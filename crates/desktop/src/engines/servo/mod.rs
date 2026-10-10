//! The Servo provider. One runtime serves independently owned views; page content runs
//! in Servo's content processes (see `run_content_process` and `watchdog`).
mod bridge;
mod bridge_protocol;
mod input;
mod key_ownership;
mod native;
mod page;
pub(crate) mod smoke;
mod watchdog;

use super::{PageEventProxy, WebView2Options};
use crate::{App, ModeKind, UserEvent};
use browser_engine::{EngineResult, EngineView, History, RectPx, Source, ViewId, ViewIdentity};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use servo::{RenderingContext, Servo, ServoBuilder, WebView, WebViewBuilder};
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::{Rc, Weak},
    time::Instant,
};
use tao::{
    event::{ElementState, Event as TaoEvent, WindowEvent},
    event_loop::{EventLoopProxy, EventLoopWindowTarget},
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Window, WindowBuilder},
};

// Construction is synchronous, scoped to startup or event dispatch on this UI
// thread. No target reference escapes the scope or needs a fabricated lifetime.
scoped_tls::scoped_thread_local!(static TARGET: EventLoopWindowTarget<UserEvent>);
pub(super) fn with_target<R>(
    target: &EventLoopWindowTarget<UserEvent>,
    f: impl FnOnce() -> R,
) -> R {
    TARGET.set(target, f)
}
thread_local! {
    // Servo has process-global initialization (verified on 0.5); retain it until app teardown.
    static RUNTIME: RefCell<Option<Rc<Servo>>> = const { RefCell::new(None) };
    static VIEWS: RefCell<HashMap<ViewId, Weak<State>>> = RefCell::new(HashMap::new());
    static KEYS: RefCell<key_ownership::KeyOwnership<tao::keyboard::KeyCode>> = RefCell::new(Default::default());
    static CONSUMED_TEXT: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub(crate) enum Event {
    Wake,
    SmokeReply(Result<String, String>),
    Frame(ViewId),
    Cursor(ViewId, servo::Cursor),
    Menu(ViewId),
    Reply {
        view: ViewId,
        epoch: u64,
        result: Result<String, String>,
    },
    Fault {
        view: ViewId,
        epoch: u64,
        error: String,
    },
    /// A content process exited abnormally; some pages may be dead (see `watchdog`).
    ContentCrashed {
        code: u32,
    },
    /// The answer to a liveness check sent after a content process died.
    Alive {
        view: ViewId,
        alive: bool,
    },
}
fn post(proxy: &EventLoopProxy<UserEvent>, event: Event) {
    let _ = proxy.send_event(UserEvent::Servo(event));
}
#[derive(Clone)]
struct Waker(EventLoopProxy<UserEvent>);
impl servo::EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn servo::EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        post(&self.0, Event::Wake);
    }
}

struct State {
    page: page::ServoPage,
    window: Rc<Window>,
    proxy: EventLoopProxy<UserEvent>,
    scoped: PageEventProxy,
    navigation: Rc<bridge::Navigation>,
    bridge: RefCell<bridge::Bridge>,
    input: RefCell<input::Input>,
    menu: Rc<RefCell<Option<servo::ContextMenu>>>,
    cursor: Cell<servo::Cursor>,
    visible: Cell<bool>,
    painted: Cell<bool>,
    frame_pending: Rc<Cell<bool>>,
    /// While set, a liveness check is outstanding; no answer by then means dead.
    alive_by: Cell<Option<Instant>>,
    crashed: Cell<bool>,
}
impl State {
    /// Report this page dead, once: its tab becomes a crash placeholder.
    fn crashed(&self, reason: String) {
        self.alive_by.set(None);
        if !self.crashed.replace(true) {
            let _ = self.scoped.send_event(UserEvent::EngineCrashed(reason));
        }
    }
}
struct View(Rc<State>);
impl EngineView for View {
    fn identity(&self) -> &ViewIdentity {
        self.0.page.identity()
    }
    fn load_url(&self, url: &str) -> EngineResult {
        self.0.page.load_url(url)
    }
    fn reload(&self) -> EngineResult {
        self.0.page.reload()
    }
    fn url(&self) -> EngineResult<String> {
        self.0.page.url()
    }
    fn set_bounds(&self, r: RectPx) -> EngineResult {
        self.0.page.set_bounds(r)
    }
    fn set_visible(&self, visible: bool) -> EngineResult {
        self.0.page.set_visible(visible)?;
        self.0.visible.set(visible);
        Ok(())
    }
    fn zoom(&self, factor: f64) -> EngineResult {
        self.0.page.zoom(factor)
    }
    fn focus(&self) -> EngineResult {
        self.0.page.focus()
    }
    fn focus_parent(&self) -> EngineResult {
        self.0.page.focus_parent()
    }
    fn evaluate_script(&self, script: &str) -> EngineResult {
        self.0.page.evaluate_script(script)
    }
    fn trusted_click(&self, x: f64, y: f64) -> EngineResult {
        self.0.page.trusted_click(x, y)
    }
    fn history(&self) -> Option<&dyn History> {
        self.0.page.history()
    }
}

struct Delegate {
    id: ViewId,
    proxy: EventLoopProxy<UserEvent>,
    scoped: PageEventProxy,
    page: crate::tabs::PageState,
    navigation: Rc<bridge::Navigation>,
    menu: Rc<RefCell<Option<servo::ContextMenu>>>,
    frame_pending: Rc<Cell<bool>>,
}
impl servo::WebViewDelegate for Delegate {
    fn notify_new_frame_ready(&self, _: WebView) {
        // Servo repeats this notification on every pump until the frame is
        // painted. Keep one outstanding request through presentation: posting
        // another event on every pump starves Windows' lower-priority WM_PAINT.
        if !self.frame_pending.replace(true) {
            post(&self.proxy, Event::Frame(self.id));
        }
    }
    fn notify_cursor_changed(&self, _: WebView, cursor: servo::Cursor) {
        post(&self.proxy, Event::Cursor(self.id, cursor));
    }
    fn notify_load_status_changed(&self, _: WebView, status: servo::LoadStatus) {
        self.navigation.status(status);
        if matches!(status, servo::LoadStatus::HeadParsed) {
            if let Some(page) = view(self.id).filter(|p| !p.visible.get()) {
                page.page
                    .raw()
                    .evaluate_javascript(page::visibility_script(true), |_| {});
            }
        }
        match status {
            servo::LoadStatus::Started => {
                self.page.begin_load();
                let _ = self.scoped.send_event(UserEvent::SyncAdblock);
            }
            servo::LoadStatus::Complete => {
                self.page.end_load();
                let _ = self.scoped.send_event(UserEvent::FocusShell);
            }
            _ => {}
        }
        let _ = self.scoped.send_event(UserEvent::Redraw);
    }
    fn notify_url_changed(&self, _: WebView, _: url::Url) {
        let _ = self
            .scoped
            .send_event(UserEvent::UrlChanged { record: true });
    }
    fn notify_page_title_changed(&self, _: WebView, _: Option<String>) {
        let _ = self.scoped.send_event(UserEvent::Redraw);
    }
    fn show_console_message(&self, _: WebView, level: servo::ConsoleLogLevel, message: String) {
        // Diagnostics only: BROWSER_SERVO_CONSOLE_LOG names a file for page console output.
        if let Some(path) = std::env::var_os("BROWSER_SERVO_CONSOLE_LOG") {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "[{level:?}] {message}");
            }
        }
    }
    fn notify_fullscreen_state_changed(&self, _: WebView, fullscreen: bool) {
        let _ = self
            .scoped
            .send_event(UserEvent::PageFullscreen(fullscreen));
    }
    fn notify_crashed(&self, _: WebView, reason: String, _: Option<String>) {
        self.page.end_load();
        if let Some(page) = view(self.id) {
            page.crashed(format!("the page crashed: {reason}"));
        }
    }
    fn show_embedder_control(&self, _: WebView, control: servo::EmbedderControl) {
        if let servo::EmbedderControl::ContextMenu(menu) = control {
            *self.menu.borrow_mut() = Some(menu);
            post(&self.proxy, Event::Menu(self.id));
        }
    }
    fn hide_embedder_control(&self, _: WebView, id: servo::EmbedderControlId) {
        let mut menu = self.menu.borrow_mut();
        if menu.as_ref().is_some_and(|m| m.id() == id) {
            menu.take();
        }
    }
}

pub(super) fn build(
    parent: &Rc<Window>,
    opts: WebView2Options<'_>,
    identity: ViewIdentity,
) -> anyhow::Result<(Box<dyn EngineView>, crate::tabs::PageState)> {
    if !TARGET.is_set() {
        anyhow::bail!("Servo view creation requires the UI event-loop scope");
    }
    let bounds = opts.bounds;
    smoke::log("Servo build: creating child window");
    let window = Rc::new(TARGET.with(|target| {
        WindowBuilder::new()
            .with_parent_window(parent.hwnd())
            .with_decorations(false)
            .with_resizable(false)
            .with_undecorated_shadow(false)
            .with_visible(false)
            .with_inner_size(tao::dpi::PhysicalSize::new(
                bounds.w.max(1),
                bounds.h.max(1),
            ))
            .build(target)
    })?);
    smoke::log("Servo build: creating graphics context");
    let context = Rc::new(
        servo::WindowRenderingContext::new(
            window.display_handle()?,
            window.window_handle()?,
            dpi::PhysicalSize::new(bounds.w.max(1), bounds.h.max(1)),
        )
        .map_err(|e| anyhow::anyhow!("Servo graphics initialization: {e:?}"))?,
    );
    context
        .make_current()
        .map_err(|e| anyhow::anyhow!("Servo GL context: {e:?}"))?;
    smoke::log("Servo build: creating runtime");
    let runtime = RUNTIME.with(|slot| -> anyhow::Result<Rc<Servo>> {
        if let Some(runtime) = slot.borrow().as_ref().cloned() {
            return Ok(runtime);
        }
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let multiprocess = std::env::var_os("BROWSER_SERVO_SINGLE_PROCESS").is_none();
        if multiprocess {
            // Before the first content process exists. Without it, hard crashes go
            // unnoticed, but pages still run isolated.
            if let Err(error) = watchdog::start(opts.proxy.clone()) {
                smoke::log(&format!("Servo crash watchdog unavailable: {error}"));
            }
        }
        let dir = storage_dir()?;
        std::fs::create_dir_all(&dir)?;
        let runtime = Rc::new(
            ServoBuilder::default()
                .opts(servo::Opts {
                    config_dir: Some(dir),
                    // Page scripts and layout run in content processes (see
                    // `run_content_process`), so a page that crashes them takes down
                    // only its own pane. BROWSER_SERVO_SINGLE_PROCESS=1 keeps
                    // everything in-process for debugging.
                    multiprocess,
                    ..Default::default()
                })
                .preferences(web_preferences())
                .event_loop_waker(Box::new(Waker(opts.proxy.clone())))
                .build(),
        );
        *slot.borrow_mut() = Some(runtime.clone());
        Ok(runtime)
    })?;
    smoke::log("Servo build: runtime ready");
    let scripts = Rc::new(servo::UserContentManager::new(&runtime));
    let (m, c, v, sb, ab) = (
        opts.mute,
        opts.no_css,
        opts.no_video,
        opts.no_scrollbar,
        opts.adblock,
    );
    let init = format!("if(window === window.top) {{\n{}\nwindow.__mode='normal';\n{}\n{}\n{}\nwindow.__adblockDefault={ab};\n{}\n{}\nwindow.__featureDefaults={{mute:{m},css:{c},video:{v},scrollbar:{sb}}};\n{}\n{}\n}}", include_str!("bridge.js"), crate::BRIDGE_JS, crate::FIND_JS, crate::CARET_JS, crate::ADBLOCK_JS, crate::NAVGUARD_JS, crate::FEATURES_JS, opts.extra_init);
    // Standard APIs Servo lacks, filled in for every document and frame.
    if std::env::var_os("BROWSER_SERVO_NO_COMPAT").is_none() {
        scripts.add_script(Rc::new(include_str!("compat.js").to_string().into()));
    }
    // Diagnostics: BROWSER_SERVO_NO_SCRIPTS=1 loads pages without any of the shell's
    // scripts (no hints or modes), to tell a site's own problems from ours.
    if std::env::var_os("BROWSER_SERVO_NO_SCRIPTS").is_none() {
        scripts.add_script(Rc::new(init.into()));
    }
    if std::env::var_os("BROWSER_SERVO_CONSOLE_LOG").is_some() {
        // Uncaught errors don't reach the console by themselves.
        scripts.add_script(Rc::new(
            "addEventListener('error', e => console.error('[uncaught] ' + e.message + ' @ ' + e.filename + ':' + e.lineno + ':' + e.colno));
addEventListener('unhandledrejection', e => console.error('[unhandled rejection] ' + (e.reason && (e.reason.stack || e.reason))));"
                .to_string()
                .into(),
        ));
    }
    let url = match opts.source {
        Source::Url(url) => url::Url::parse(&url)?,
        Source::Html(html) => url::Url::parse(&format!(
            "data:text/html;charset=utf-8,{}",
            url::form_urlencoded::byte_serialize(html.as_bytes())
                .collect::<String>()
                .replace('+', "%20")
        ))?,
    };
    let id = identity.id;
    let scoped = PageEventProxy::new(opts.proxy.clone(), id);
    let navigation = Rc::new(bridge::Navigation::default());
    let menu = Rc::new(RefCell::new(None));
    let frame_pending = Rc::new(Cell::new(false));
    let status = crate::tabs::PageState::default();
    status.begin_load();
    smoke::log("Servo build: creating page");
    let raw = WebViewBuilder::new(&runtime, context.clone())
        .url(url)
        .hidpi_scale_factor(euclid::Scale::new(window.scale_factor() as f32))
        .user_content_manager(scripts)
        .delegate(Rc::new(Delegate {
            id,
            proxy: opts.proxy.clone(),
            scoped: scoped.clone(),
            page: status.clone(),
            navigation: navigation.clone(),
            menu: menu.clone(),
            frame_pending: frame_pending.clone(),
        }))
        .build();
    smoke::log("Servo build: page ready");
    let page = page::ServoPage::new(
        identity,
        raw,
        runtime,
        context,
        window.clone(),
        parent.clone(),
    );
    page.set_bounds(bounds).map_err(anyhow::Error::msg)?;
    let state = Rc::new(State {
        page,
        window,
        proxy: opts.proxy,
        scoped,
        navigation: navigation.clone(),
        bridge: RefCell::new(bridge::Bridge::new(navigation, id)),
        input: RefCell::new(Default::default()),
        menu,
        cursor: Cell::new(servo::Cursor::Default),
        visible: Cell::new(false),
        painted: Cell::new(false),
        frame_pending,
        alive_by: Cell::new(None),
        crashed: Cell::new(false),
    });
    VIEWS.with(|views| {
        views.borrow_mut().insert(id, Rc::downgrade(&state));
    });
    smoke::log("Servo build: complete");
    Ok((Box::new(View(state)), status))
}

/// Web platform features Servo implements but leaves off by default; its own browser
/// turns these on as "experimental web platform features". Common sites need them:
/// GitHub breaks without IntersectionObserver, and YouTube loads thumbnails with it.
/// APIs that would need a permission prompt (notifications, permissions, protocol
/// handlers, reading the clipboard) stay off: the shell has no UI to answer one.
fn web_preferences() -> servo::Preferences {
    let mut p = servo::Preferences {
        dom_intersection_observer_enabled: true,
        dom_adoptedstylesheet_enabled: true,
        dom_fontface_enabled: true,
        dom_indexeddb_enabled: true,
        dom_exec_command_enabled: true,
        dom_offscreen_canvas_enabled: true,
        dom_sanitizer_enabled: true,
        dom_storage_manager_api_enabled: true,
        layout_columns_enabled: true,
        layout_container_queries_enabled: true,
        layout_css_alpha_color_function_enabled: true,
        layout_css_attr_enabled: true,
        layout_css_ellipse_corners_enabled: true,
        layout_css_progress_function_enabled: true,
        layout_variable_fonts_enabled: true,
        ..Default::default()
    };
    // Diagnostics: BROWSER_SERVO_PREFS="name=value,..." overrides any Servo preference.
    if let Ok(spec) = std::env::var("BROWSER_SERVO_PREFS") {
        for item in spec.split(',').filter(|s| !s.trim().is_empty()) {
            let Some((name, value)) = item.split_once('=') else {
                continue;
            };
            let value = match value.trim() {
                "true" => servo::PrefValue::Bool(true),
                "false" => servo::PrefValue::Bool(false),
                v => v
                    .parse()
                    .map(servo::PrefValue::Int)
                    .unwrap_or_else(|_| servo::PrefValue::Str(v.into())),
            };
            // Servo panics on an unknown name; report it instead.
            let set = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                p.set_value(name.trim(), value)
            }));
            if set.is_err() {
                smoke::log(&format!("BROWSER_SERVO_PREFS: unknown preference {name}"));
            }
        }
    }
    p
}

/// In multi-process mode Servo relaunches this executable as
/// `browser --content-process <token>` to run page scripts and layout. Returns true
/// when this process was one of those; it has then already run to completion.
pub(crate) fn run_content_process() -> bool {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--content-process") {
        return false;
    }
    let Some(token) = args.next() else {
        return false;
    };
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    servo::run_content_process(token);
    true
}

fn storage_dir() -> anyhow::Result<std::path::PathBuf> {
    // Explicit test override never changes the normal WebView2 profile.
    if let Some(dir) = std::env::var_os("BROWSER_SERVO_DATA_DIR") {
        return Ok(dir.into());
    }
    let dir = directories::ProjectDirs::from("", "", "browser")
        .ok_or_else(|| anyhow::anyhow!("No browser data directory"))?;
    Ok(dir.data_dir().join("engines").join("servo"))
}
fn views() -> Vec<Rc<State>> {
    VIEWS.with(|views| {
        let mut views = views.borrow_mut();
        views.retain(|_, view| view.strong_count() > 0);
        views.values().filter_map(Weak::upgrade).collect()
    })
}

pub(crate) fn runtime_view_count() -> Option<usize> {
    RUNTIME.with(|slot| slot.borrow().as_ref().map(|_| views().len()))
}
fn view(id: ViewId) -> Option<Rc<State>> {
    VIEWS.with(|views| views.borrow().get(&id).and_then(Weak::upgrade))
}

pub(crate) fn tick(app: &App) -> Option<Instant> {
    let pages = views();
    // Also process asynchronous page closure when the last surface is gone.
    let runtime = RUNTIME.with(|slot| slot.borrow().as_ref().cloned());
    if let Some(runtime) = runtime {
        runtime.spin_event_loop();
    }
    let mut due = None;
    for page in pages {
        if let Some(by) = page.alive_by.get() {
            if Instant::now() >= by {
                page.crashed("its Servo content process stopped".into());
            } else {
                due = Some(due.map_or(by, |old: Instant| old.min(by)));
            }
        }
        native::refresh_cursor(&page.window, page.cursor.get());
        let mut bridge = page.bridge.borrow_mut();
        let focused = native::focused(&page.window)
            || (native::focused(&app.window)
                && app
                    .active_webview()
                    .is_some_and(|v| v.identity().id == page.page.identity().id));
        bridge.tick(page.page.raw(), &page.proxy, focused);
        let at = bridge.wakeup();
        due = Some(due.map_or(at, |old: Instant| old.min(at)));
    }
    due
}

pub(crate) fn intercept<'a>(
    app: &mut App,
    event: TaoEvent<'a, UserEvent>,
) -> Option<TaoEvent<'a, UserEvent>> {
    match event {
        TaoEvent::UserEvent(UserEvent::Servo(event)) => {
            match event {
                Event::Wake => {}
                Event::SmokeReply(result) => smoke::reply(result),
                Event::Frame(id) => {
                    if let Some(page) = view(id) {
                        if page.visible.get() {
                            page.window.request_redraw();
                        }
                    }
                }
                Event::Cursor(id, cursor) => {
                    if let Some(page) = view(id) {
                        page.cursor.set(cursor);
                        native::refresh_cursor(&page.window, cursor);
                    }
                }
                Event::Menu(id) => {
                    if let Some(page) = view(id) {
                        let menu = page.menu.borrow_mut().take();
                        if let Some(menu) = menu {
                            if let Err(e) = native::show_context_menu(&page.window, menu, false) {
                                app.set_error(e.to_string());
                            }
                        }
                    }
                }
                Event::Reply {
                    view: id,
                    epoch,
                    result,
                } => {
                    if let Some(page) = view(id) {
                        let messages = page.bridge.borrow_mut().reply(epoch, result);
                        match messages {
                            Ok(messages) => {
                                for message in messages {
                                    if let Some(event) =
                                        super::events::decode_page_message(&message)
                                    {
                                        let _ = page.scoped.send_event(event);
                                    }
                                }
                            }
                            Err(e) => app.set_error(format!("Servo bridge: {e}; reload to retry")),
                        }
                    }
                }
                Event::ContentCrashed { code } => {
                    smoke::log(&format!("Servo content process exited with {code:#x}"));
                    // Which pages it ran isn't reported: ask each page that isn't
                    // already being asked; only dead ones fail to answer.
                    for page in views() {
                        if page.crashed.get() || page.alive_by.get().is_some() {
                            continue;
                        }
                        page.alive_by
                            .set(Some(Instant::now() + std::time::Duration::from_secs(3)));
                        let proxy = page.proxy.clone();
                        let id = page.page.identity().id;
                        page.page.raw().evaluate_javascript("1", move |result| {
                            post(
                                &proxy,
                                Event::Alive {
                                    view: id,
                                    alive: result.is_ok(),
                                },
                            );
                        });
                    }
                }
                Event::Alive { view: id, alive } => {
                    if let Some(page) = view(id) {
                        if alive {
                            page.alive_by.set(None);
                        } else {
                            page.crashed("its Servo content process stopped".into());
                        }
                    }
                }
                Event::Fault {
                    view: id,
                    epoch,
                    error,
                } => {
                    if let Some(page) = view(id) {
                        if epoch == page.navigation.epoch.get() {
                            app.set_error(error);
                        }
                    }
                }
            }
            None
        }
        TaoEvent::RedrawRequested(id) if id != app.window.id() => {
            if let Some(page) = views().into_iter().find(|v| v.window.id() == id) {
                if page.visible.get() {
                    if !page.painted.get() {
                        smoke::log("Servo paint: first frame starting");
                    }
                    if let Err(e) = page.page.paint() {
                        app.set_error(e);
                    } else {
                        page.frame_pending.set(false);
                        if !page.painted.replace(true) {
                            smoke::log("Servo paint: first frame presented");
                        }
                    }
                }
            }
            None
        }
        original @ TaoEvent::WindowEvent { .. } => {
            let TaoEvent::WindowEvent {
                window_id,
                ref event,
                ..
            } = original
            else {
                unreachable!()
            };
            let pages = views();
            let child = pages.iter().find(|v| v.window.id() == window_id).cloned();
            if child.is_none() && window_id != app.window.id() {
                return None;
            }
            if let WindowEvent::ModifiersChanged(modifiers) = event {
                app.modifiers = *modifiers;
            }
            if matches!(event, WindowEvent::Focused(false))
                && !native::focused(&app.window)
                && !pages.iter().any(|v| native::focused(&v.window))
            {
                KEYS.with(|keys| keys.borrow_mut().clear());
                CONSUMED_TEXT.with(|text| text.borrow_mut().take());
            }
            if let WindowEvent::KeyboardInput {
                event: key,
                is_synthetic,
                ..
            } = event
            {
                let shell = !matches!(app.mode, ModeKind::Insert | ModeKind::Passthrough)
                    && !app.page_focus_yielded;
                let mut route = KEYS.with(|keys| {
                    keys.borrow_mut().route(
                        key.physical_key,
                        key.state == ElementState::Pressed,
                        *is_synthetic,
                        shell,
                    )
                });
                // Keep the browser's normal key repeat (scrolling, command editing).
                // Ownership blocks repeats only after the press hands focus to a page.
                if route == key_ownership::Route::Suppress
                    && shell
                    && !is_synthetic
                    && key.state == ElementState::Pressed
                {
                    route = key_ownership::Route::Shell;
                }
                if route != key_ownership::Route::Page
                    && key.state == ElementState::Pressed
                    && !is_synthetic
                {
                    CONSUMED_TEXT.with(|text| {
                        *text.borrow_mut() = match key.logical_key {
                            tao::keyboard::Key::Character(s) => Some(s.into()),
                            _ => None,
                        }
                    });
                }
                if route == key_ownership::Route::Suppress {
                    return None;
                }
                if child.is_some() && route == key_ownership::Route::Shell {
                    app.handle_key(key);
                    return None;
                }
            }
            if let Some(page) = child {
                if let WindowEvent::ReceivedImeText(text) = event {
                    if CONSUMED_TEXT
                        .with(|old| old.borrow_mut().take().as_deref() == Some(text.as_str()))
                    {
                        return None;
                    }
                }
                if matches!(
                    event,
                    WindowEvent::MouseInput {
                        state: ElementState::Pressed,
                        ..
                    }
                ) {
                    app.route_engine_event(page.page.identity().id, UserEvent::PaneClick);
                }
                if let TaoEvent::WindowEvent { event, .. } = original {
                    page.input
                        .borrow_mut()
                        .handle(page.page.raw(), &page.window, event);
                }
                return None;
            }
            // Preserve the original event for the ordinary shell path.
            Some(original)
        }
        event => Some(event),
    }
}

/// Called after all tab views have been dropped, while the shell window exists.
pub(crate) fn shutdown() {
    let Some(runtime) = RUNTIME.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    // Servo 0.6 waits, without a time limit, for every page to confirm it closed, and
    // a page whose content process died never does. By now the session, terminals and
    // any pending update are taken care of, so a stuck shutdown just ends the process.
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watch = done.clone();
    let _ = std::thread::Builder::new()
        .name("servo-shutdown-limit".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if !watch.load(std::sync::atomic::Ordering::Acquire) {
                smoke::log("Servo shutdown exceeded its limit; exiting");
                std::process::exit(0);
            }
        });
    drop(runtime);
    done.store(true, std::sync::atomic::Ordering::Release);
}
