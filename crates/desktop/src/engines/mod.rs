//! Adapter entry points. The shell only stores browser_engine trait objects.
mod events;
#[cfg(all(windows, feature = "servo-engine"))]
pub(crate) mod servo;
mod shell;
#[cfg(windows)]
pub(crate) mod trident;
pub(crate) use shell::unavailable_content;
mod options;
mod popup_queue;
#[cfg(windows)]
mod webview2;

pub(crate) use options::BuildOptions;
pub(crate) use popup_queue::take as take_popups;

/// The engine a new tab uses unless `config.toml` picks another: each platform's
/// system webview. A saved provider that only exists on another platform (a session
/// copied from Windows) resolves to this one, see [`resolve`].
#[cfg(windows)]
pub(crate) const DEFAULT_PROVIDER: &str = "webview2";
#[cfg(not(windows))]
pub(crate) const DEFAULT_PROVIDER: &str = "webkitgtk";

/// Providers that exist only on another platform, mapped to this one's default when
/// a session or config written there names them.
const FOREIGN_PROVIDERS: &[&str] = if cfg!(windows) {
    &["webkitgtk"]
} else {
    &["webview2"]
};

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
#[cfg(windows)]
pub(crate) use webview2::downloads::{answer as answer_download, cancel as cancel_download};
#[cfg(windows)]
pub(crate) use webview2::permissions::answer as answer_permission;

/// Answer a download the engine is holding. No engine holds one on this platform yet.
#[cfg(not(windows))]
pub(crate) fn answer_download(_id: u64, _path: Option<&std::path::Path>) {}

/// Cancel a running download. No engine runs one on this platform yet.
#[cfg(not(windows))]
pub(crate) fn cancel_download(_id: u64) {}

/// Answer a permission request the engine is holding. None exist on this platform yet.
#[cfg(not(windows))]
pub(crate) fn answer_permission(_id: u64, _allow: bool) {}

/// A hidden view that keeps the engine's processes (and the signed-in profile) alive
/// while a profile switch rebuilds every tab. `None` where the engine needs no help.
pub(crate) fn keep_engine_alive(
    parent: &tao::window::Window,
) -> anyhow::Result<Option<Box<dyn browser_engine::EngineView>>> {
    #[cfg(windows)]
    {
        webview2::keep_alive(parent).map(Some)
    }
    #[cfg(not(windows))]
    {
        let _ = parent;
        Ok(None)
    }
}

use browser_engine::{Capabilities, ProviderDescriptor, ViewRequirements};

pub(crate) const PROVIDERS: &[ProviderDescriptor] = &[
    #[cfg(windows)]
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
    #[cfg(windows)]
    ProviderDescriptor {
        id: "trident",
        family: "trident",
        display_name: "Trident (Internet Explorer 11)",
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
    let foreign = FOREIGN_PROVIDERS
        .iter()
        .any(|p| p.eq_ignore_ascii_case(name.trim()));
    let name = if foreign { DEFAULT_PROVIDER } else { name };
    browser_engine::resolve_provider(PROVIDERS, name, preferred)
}

pub(crate) fn build(
    provider: &str,
    parent: &std::rc::Rc<tao::window::Window>,
    opts: BuildOptions<'_>,
) -> anyhow::Result<(Box<dyn browser_engine::EngineView>, crate::tabs::PageState)> {
    let provider = resolve(provider, DEFAULT_PROVIDER).map_err(anyhow::Error::msg)?;
    let needs = ViewRequirements {
        storage: opts.storage,
        disable_javascript: opts.disable_js,
        shell_bridge: true,
    };
    browser_engine::build_checked(provider, needs, |identity| match provider.id {
        #[cfg(windows)]
        "webview2" => webview2::build(parent, opts, identity).map_err(|e| format!("{e:#}")),
        #[cfg(all(windows, feature = "servo-engine"))]
        "servo" => servo::build(parent, opts, identity).map_err(|e| format!("{e:#}")),
        #[cfg(windows)]
        "trident" => trident::build(parent, opts, identity).map_err(|e| format!("{e:#}")),
        _ => {
            let _ = (parent, &opts, &identity);
            Err(format!("no factory for {}", provider.id))
        }
    })
    .map_err(anyhow::Error::msg)
}
