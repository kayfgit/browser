//! Opt-in in-process Servo provider. One runtime serves independently owned views.
mod bridge;
mod bridge_protocol;
mod input;
mod key_ownership;
mod native;
mod page;
pub(crate) mod smoke;

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
    fn notify_fullscreen_state_changed(&self, _: WebView, fullscreen: bool) {
        let _ = self
            .scoped
            .send_event(UserEvent::PageFullscreen(fullscreen));
    }
    fn notify_crashed(&self, _: WebView, reason: String, _: Option<String>) {
        self.page.end_load();
        post(
            &self.proxy,
            Event::Fault {
                view: self.id,
                epoch: self.navigation.epoch.get(),
                error: format!("Servo page crashed: {reason}"),
            },
        );
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
        let dir = storage_dir()?;
        std::fs::create_dir_all(&dir)?;
        let runtime = Rc::new(
            ServoBuilder::default()
                .opts(servo::Opts {
                    config_dir: Some(dir),
                    ..Default::default()
                })
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
    let init = format!("if(window === window.top) {{\n{}\nwindow.__mode='normal';\n{}\n{}\n{}\nwindow.__adblockDefault={ab};\n{}\nwindow.__featureDefaults={{mute:{m},css:{c},video:{v},scrollbar:{sb}}};\n{}\n{}\n}}", include_str!("bridge.js"), crate::BRIDGE_JS, crate::FIND_JS, crate::CARET_JS, crate::ADBLOCK_JS, crate::FEATURES_JS, opts.extra_init);
    scripts.add_script(Rc::new(init.into()));
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
    });
    VIEWS.with(|views| {
        views.borrow_mut().insert(id, Rc::downgrade(&state));
    });
    smoke::log("Servo build: complete");
    Ok((Box::new(View(state)), status))
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
    let runtime = RUNTIME.with(|slot| slot.borrow_mut().take());
    drop(runtime);
}
