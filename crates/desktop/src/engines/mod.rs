//! Adapter entry points. The shell only stores browser_engine trait objects.
mod events;
#[cfg(all(windows, feature = "servo-engine"))]
pub(crate) mod servo;
mod shell;
pub(crate) use shell::unavailable_content;
mod webview2;

pub(crate) fn with_window_target<R>(
    target: &tao::event_loop::EventLoopWindowTarget<crate::UserEvent>,
    f: impl FnOnce() -> R,
) -> R {
    #[cfg(all(windows, feature = "servo-engine"))]
    {
        servo::with_target(target, f)
    }
    #[cfg(not(all(windows, feature = "servo-engine")))]
    {
        let _ = target;
        f()
    }
}

#[cfg(test)]
mod tests;

// The Servo adapter's message queue and key routing are plain Rust; build their tests
// without the Servo feature too, so every CI run covers them.
#[cfg(all(test, not(all(windows, feature = "servo-engine"))))]
#[allow(dead_code)]
#[path = "servo/bridge_protocol.rs"]
mod servo_bridge_protocol;
#[cfg(all(test, not(all(windows, feature = "servo-engine"))))]
#[allow(dead_code)]
#[path = "servo/key_ownership.rs"]
mod servo_key_ownership;

pub(crate) use events::PageEventProxy;
pub(crate) use webview2::downloads::{answer as answer_download, cancel as cancel_download};
pub(crate) use webview2::permissions::answer as answer_permission;
pub(crate) use webview2::{keep_alive as keep_webview2_alive, BuildOptions as WebView2Options};

use browser_engine::{Capabilities, ProviderDescriptor, ViewRequirements};

pub(crate) const PROVIDERS: &[ProviderDescriptor] = &[
    ProviderDescriptor {
        id: "webview2",
        family: "blink",
        display_name: "Microsoft Edge WebView2",
        capabilities: Capabilities {
            private: true,
            disable_javascript: true,
            document_scripts: true,
            page_messages: true,
        },
    },
    #[cfg(all(windows, feature = "servo-engine"))]
    ProviderDescriptor {
        id: "servo",
        family: "servo",
        display_name: "Servo 0.6",
        capabilities: Capabilities {
            private: false,
            disable_javascript: false,
            document_scripts: true,
            page_messages: true,
        },
    },
];

pub(crate) fn resolve(
    name: &str,
    preferred: &str,
) -> browser_engine::EngineResult<&'static ProviderDescriptor> {
    browser_engine::resolve_provider(PROVIDERS, name, preferred)
}

pub(crate) fn build(
    provider: &str,
    parent: &std::rc::Rc<tao::window::Window>,
    opts: WebView2Options<'_>,
) -> anyhow::Result<(Box<dyn browser_engine::EngineView>, crate::tabs::PageState)> {
    let provider = resolve(provider, "webview2").map_err(anyhow::Error::msg)?;
    let needs = ViewRequirements {
        storage: opts.storage,
        disable_javascript: opts.disable_js,
        shell_bridge: true,
    };
    browser_engine::build_checked(provider, needs, |identity| match provider.id {
        "webview2" => webview2::build(parent, opts, identity).map_err(|e| format!("{e:#}")),
        #[cfg(all(windows, feature = "servo-engine"))]
        "servo" => servo::build(parent, opts, identity).map_err(|e| format!("{e:#}")),
        _ => Err(format!("no factory for {}", provider.id)),
    })
    .map_err(anyhow::Error::msg)
}
