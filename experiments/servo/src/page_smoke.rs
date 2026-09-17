//! Exercise host operations through `dyn EngineView`, observing the real DOM and HWND.
use super::{Lab, Message, native};
use browser_engine::{EngineView, RectPx, StorageMode};
use std::time::{Duration, Instant};
use tao::event_loop::EventLoopProxy;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Step {
    Start,
    LoadedA,
    Zoomed,
    Resized,
    Reset,
    LoadedB,
    Back,
    Forward,
    Script,
    Reloaded,
    Done,
}

pub struct PageSmoke {
    step: Step,
    pending: bool,
    proof: bool,
    due: Instant,
    deadline: Instant,
    a: String,
    b: String,
}
impl Default for PageSmoke {
    fn default() -> Self {
        Self {
            step: Step::Start,
            pending: false,
            proof: false,
            due: Instant::now(),
            deadline: Instant::now(),
            a: fixture("Adapter A"),
            b: fixture("Adapter B"),
        }
    }
}
impl PageSmoke {
    pub fn done(&self) -> bool {
        self.step == Step::Done
    }
    pub fn reply(&mut self, result: Result<String, String>) -> Result<(), String> {
        self.pending = false;
        self.proof = result? == "true";
        self.due = Instant::now() + Duration::from_millis(100);
        Ok(())
    }
    fn step(&mut self, step: Step) {
        self.step = step;
        self.proof = false;
        self.deadline = Instant::now() + Duration::from_secs(10);
        self.due = Instant::now() + Duration::from_millis(100);
    }
    pub fn tick(&mut self, lab: &Lab, proxy: &EventLoopProxy<Message>) -> Result<(), String> {
        if self.done() {
            return Ok(());
        }
        let page: &dyn EngineView = &lab.page;
        if self.step == Step::Start {
            if page.identity().provider != "servo"
                || page.identity().storage != StorageMode::Persistent
            {
                return Err("Unexpected adapter identity".into());
            }
            if page.extensions().is_some()
                || page.browsing_data().is_some()
                || page.suspension().is_some()
            {
                return Err("Adapter advertised an unqualified optional service".into());
            }
            let original = page.url()?;
            if page.load_url("not a URL").is_ok()
                || page.url()? != original
                || page.zoom(f64::NAN).is_ok()
            {
                return Err("Adapter accepted invalid navigation/zoom".into());
            }
            page.focus()?;
            if !native::focused(&lab.servo_window) {
                return Err("Adapter focus failed".into());
            }
            page.set_visible(false)?;
            if lab.servo_window.is_visible() || !native::focused(&lab.parent) {
                return Err("Hiding the adapter did not return focus to the shell".into());
            }
            lab.webview2.focus().map_err(|e| e.to_string())?;
            let other_focus = native::focused_handle();
            page.set_visible(true)?;
            if !lab.servo_window.is_visible()
                || other_focus == 0
                || native::focused_handle() != other_focus
            {
                return Err("Showing the adapter stole WebView2 focus".into());
            }
            page.load_url(&self.a)?;
            self.step(Step::LoadedA);
            return Ok(());
        }
        if Instant::now() > self.deadline {
            return Err(format!("Adapter probe {:?} timed out", self.step));
        }
        if self.pending || Instant::now() < self.due {
            return Ok(());
        }
        if self.proof {
            match self.step {
                Step::LoadedA => {
                    page.zoom(1.5)?;
                    self.step(Step::Zoomed);
                }
                Step::Zoomed => {
                    let size = lab.parent.inner_size();
                    page.set_bounds(RectPx {
                        x: 0,
                        y: 0,
                        w: size.width / 3,
                        h: size.height,
                    })?;
                    self.step(Step::Resized);
                }
                Step::Resized => {
                    lab.resize().map_err(|e| e.to_string())?;
                    page.zoom(1.0)?;
                    self.step(Step::Reset);
                }
                Step::Reset => {
                    page.load_url(&self.b)?;
                    self.step(Step::LoadedB);
                }
                Step::LoadedB => {
                    let history = page.history().ok_or("Missing history service")?;
                    if !history.can_go(false) || history.can_go(true) || history.go(true).is_ok() {
                        return Err("Incorrect history boundaries after navigation".into());
                    }
                    history.go(false)?;
                    self.step(Step::Back);
                }
                Step::Back => {
                    page.history().ok_or("Missing history service")?.go(true)?;
                    self.step(Step::Forward);
                }
                Step::Forward => {
                    page.evaluate_script("window.adapterMarker = 42")?;
                    self.step(Step::Script);
                }
                Step::Script => {
                    page.reload()?;
                    self.step(Step::Reloaded);
                }
                Step::Reloaded => {
                    eprintln!(
                        "Adapter probe: identity, optional-service rejection, visibility/focus, layout zoom/reset, bounds, navigation, back/forward, script dispatch and reload passed through EngineView"
                    );
                    self.step(Step::Done);
                }
                _ => {}
            }
            return Ok(());
        }
        let title = match self.step {
            Step::LoadedB | Step::Forward | Step::Script | Step::Reloaded => "Adapter B",
            _ => "Adapter A",
        };
        let expected_url = if title == "Adapter B" {
            &self.b
        } else {
            &self.a
        };
        if page.url()? != *expected_url
            || lab.page.raw().load_status() != servo::LoadStatus::Complete
        {
            self.due = Instant::now() + Duration::from_millis(100);
            return Ok(());
        }
        let extra = match self.step {
            Step::Zoomed | Step::Resized | Step::Reset => {
                let zoom = if self.step == Step::Reset { 1.0 } else { 1.5 };
                let ratio = lab.servo_window.scale_factor() * zoom;
                let parent_size = lab.parent.inner_size();
                let expected_width =
                    parent_size.width / if self.step == Step::Resized { 3 } else { 2 };
                let native_size = lab.servo_window.inner_size();
                let rendered_size = lab.page.raw().size();
                if native_size.width != expected_width
                    || native_size.height != parent_size.height
                    || rendered_size.width != expected_width as f32
                    || rendered_size.height != parent_size.height as f32
                {
                    self.due = Instant::now() + Duration::from_millis(100);
                    return Ok(());
                }
                let width = f64::from(expected_width) / ratio;
                format!(
                    "Math.abs(innerWidth - {width}) <= 2 && Math.abs(devicePixelRatio - {ratio}) < 0.01"
                )
            }
            Step::Script => "window.adapterMarker === 42".into(),
            Step::Reloaded => "window.adapterMarker === undefined".into(),
            _ => "true".into(),
        };
        let script = format!(
            "String(document.title === '{}' && window.labInjected === true && ({}))",
            title, extra
        );
        self.pending = true;
        let proxy = proxy.clone();
        lab.page.raw().evaluate_javascript(script, move |result| {
            let result = match result {
                Ok(servo::JSValue::String(value)) => Ok(value),
                other => Err(format!("Adapter probe evaluation: {other:?}")),
            };
            let _ = proxy.send_event(Message::PageProbe(result));
        });
        Ok(())
    }
}
fn fixture(title: &str) -> String {
    let html = format!("<!doctype html><title>{title}</title><p>EngineView qualification.</p>");
    format!(
        "data:text/html,{}",
        url::form_urlencoded::byte_serialize(html.as_bytes())
            .collect::<String>()
            .replace('+', "%20")
    )
}
