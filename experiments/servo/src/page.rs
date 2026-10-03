//! Experimental implementation of the real shell contract. Native event plumbing
//! and the polling transport remain adapter internals until provider registration.
use super::native;
use browser_engine::{EngineResult, EngineView, History, RectPx, ViewIdentity};
use servo::{RenderingContext, Servo, WebView};
use std::rc::Rc;
use tao::{
    dpi::{PhysicalPosition, PhysicalSize},
    window::Window,
};

pub struct ServoPage {
    identity: ViewIdentity,
    // Drop the view before its runtime, and the runtime before its GL surface.
    view: WebView,
    runtime: Rc<Servo>,
    context: Rc<servo::WindowRenderingContext>,
    window: Rc<Window>,
    parent: Rc<Window>,
}

impl ServoPage {
    pub fn new(
        identity: ViewIdentity,
        view: WebView,
        runtime: Rc<Servo>,
        context: Rc<servo::WindowRenderingContext>,
        window: Rc<Window>,
        parent: Rc<Window>,
    ) -> Self {
        Self {
            identity,
            view,
            runtime,
            context,
            window,
            parent,
        }
    }

    // Used only by the lab's native input, transport and qualification probes.
    pub(super) fn raw(&self) -> &WebView {
        &self.view
    }
    pub fn pump(&self) {
        self.runtime.spin_event_loop();
    }
    pub fn paint(&self) -> EngineResult {
        self.context
            .make_current()
            .map_err(|e| format!("Make current: {e:?}"))?;
        self.view.paint();
        self.context.present();
        Ok(())
    }
}

impl EngineView for ServoPage {
    fn identity(&self) -> &ViewIdentity {
        &self.identity
    }
    fn load_url(&self, url: &str) -> EngineResult {
        let url = url::Url::parse(url).map_err(|e| format!("Invalid Servo URL: {e}"))?;
        self.view.load(url);
        Ok(())
    }
    fn reload(&self) -> EngineResult {
        self.view.reload();
        Ok(())
    }
    fn url(&self) -> EngineResult<String> {
        self.view
            .url()
            .map(|url| url.into())
            .ok_or_else(|| "Servo has no current URL".into())
    }
    fn set_bounds(&self, rect: RectPx) -> EngineResult {
        // Servo requires at least one physical pixel; visibility is independent.
        let size = PhysicalSize::new(rect.w.max(1), rect.h.max(1));
        self.window
            .set_outer_position(PhysicalPosition::new(rect.x, rect.y));
        self.window.set_inner_size(size);
        self.view
            .resize(dpi::PhysicalSize::new(size.width, size.height));
        Ok(())
    }
    fn set_visible(&self, visible: bool) -> EngineResult {
        if visible {
            self.view.show();
            self.window.set_visible(true);
            self.window.request_redraw();
        } else {
            if native::focused(&self.window) {
                self.focus_parent()?;
            }
            self.view.hide();
            self.window.set_visible(false);
        }
        Ok(())
    }
    fn zoom(&self, factor: f64) -> EngineResult {
        // Reject values Servo would silently clamp, and never send NaN to layout.
        browser_servo_lab::page_policy::validate_zoom(factor)?;
        self.view.set_page_zoom(factor as f32);
        Ok(())
    }
    fn focus(&self) -> EngineResult {
        if !native::focus(&self.window) {
            return Err("Could not focus Servo surface".into());
        }
        self.view.focus();
        Ok(())
    }
    fn focus_parent(&self) -> EngineResult {
        if !native::focus(&self.parent) {
            return Err("Could not focus shell window".into());
        }
        // The native Focused(false) event reaches the input adapter. Sending a
        // second blur here can race the shell's subsequent Insert transition.
        Ok(())
    }
    fn evaluate_script(&self, script: &str) -> EngineResult {
        self.view.evaluate_javascript(script, |_| {});
        Ok(())
    }
    fn trusted_click(&self, x: f64, y: f64) -> EngineResult {
        super::input::click(&self.view, x, y)
    }
    fn history(&self) -> Option<&dyn History> {
        Some(self)
    }
}

impl History for ServoPage {
    fn can_go(&self, forward: bool) -> bool {
        if forward {
            self.view.can_go_forward()
        } else {
            self.view.can_go_back()
        }
    }
    fn go(&self, forward: bool) -> EngineResult {
        if !self.can_go(forward) {
            return Err("No Servo history entry in that direction".into());
        }
        if forward {
            self.view.go_forward(1);
        } else {
            self.view.go_back(1);
        }
        Ok(())
    }
}
