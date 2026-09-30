//! The in-process boundary between the browser shell and a page renderer.
//!
//! This is a Rust source interface, not a binary plugin ABI. Views and their services
//! are UI-thread-affine. Dropping a view releases its resources; a shared runtime may
//! finish shutdown asynchronously after its last view closes.
//!
//! Keep this contract grounded in working adapters. Provider discovery, storage
//! contexts, tagged events and external-process transport are separate next steps;
//! see `docs/engine-switching-research.md` in the workspace.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

/// Identifies one incarnation of a view, never a tab index. Replacing a view gets
/// a new ID; delayed events from the old incarnation cannot address its successor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ViewId(u64);

impl ViewId {
    pub fn allocate() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(
            NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                .expect("view identity space exhausted"),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StorageMode {
    Persistent,
    Private,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewIdentity {
    pub id: ViewId,
    pub provider: String,
    pub storage: StorageMode,
}

#[derive(Clone, Copy, Debug)]
pub struct Capabilities {
    pub private: bool,
    pub disable_javascript: bool,
    pub document_scripts: bool,
    pub page_messages: bool,
}

/// Metadata lookup must not initialize a runtime or create a data directory.
#[derive(Clone, Copy, Debug)]
pub struct ProviderDescriptor {
    pub id: &'static str,
    pub family: &'static str,
    pub display_name: &'static str,
    pub capabilities: Capabilities,
}

#[derive(Clone, Copy, Debug)]
pub struct ViewRequirements {
    pub storage: StorageMode,
    pub disable_javascript: bool,
    pub shell_bridge: bool,
}

impl ProviderDescriptor {
    pub fn validate(&self, needs: ViewRequirements) -> EngineResult {
        let caps = self.capabilities;
        if needs.storage == StorageMode::Private && !caps.private {
            return Err(format!("{} cannot provide private browsing", self.id));
        }
        if needs.disable_javascript && !caps.disable_javascript {
            return Err(format!("{} cannot disable page JavaScript", self.id));
        }
        if needs.shell_bridge && (!caps.document_scripts || !caps.page_messages) {
            return Err(format!(
                "{} does not support the required shell bridge",
                self.id
            ));
        }
        Ok(())
    }
}

/// Resolve concrete IDs first. Family aliases only choose an installed provider;
/// a preferred provider wins within its family. Unknown IDs never fall back.
pub fn resolve_provider<'a>(
    providers: &'a [ProviderDescriptor],
    name: &str,
    preferred: &str,
) -> EngineResult<&'a ProviderDescriptor> {
    let name = name.trim().to_ascii_lowercase();
    providers
        .iter()
        .find(|p| p.id == name)
        .or_else(|| {
            providers
                .iter()
                .find(|p| p.family == name && p.id == preferred)
        })
        .or_else(|| providers.iter().find(|p| p.family == name))
        .ok_or_else(|| format!("engine '{name}' is not installed; use :engines to list providers"))
}

/// Validate requirements before invoking a factory. A rejected private request
/// must never create even a temporary persistent view.
pub fn build_checked<T>(
    provider: &ProviderDescriptor,
    needs: ViewRequirements,
    build: impl FnOnce(ViewIdentity) -> EngineResult<T>,
) -> EngineResult<T> {
    provider.validate(needs)?;
    build(ViewIdentity {
        id: ViewId::allocate(),
        provider: provider.id.into(),
        storage: needs.storage,
    })
}

pub type EngineResult<T = ()> = Result<T, String>;
/// Completion runs on the UI thread. A dispatch error means it will not be called.
pub type Completion<T = ()> = Box<dyn FnOnce(EngineResult<T>)>;

/// Child-surface bounds in physical pixels, relative to the shell window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RectPx {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

pub enum Source {
    Url(String),
    Html(String),
}

#[derive(Clone, Debug)]
pub struct ExtensionInfo {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug)]
pub enum DataKind {
    /// Engine-owned visited/download history; excludes the shell's own history.
    History,
    /// Cookies, including sign-in sessions.
    Cookies,
    /// HTTP cache and the Cache Storage API.
    Cache,
    /// All browsing data in this view's storage context, including site storage.
    All,
}

/// One live page. No native engine handle or downcast escapes this boundary.
/// Methods dispatch work; success does not imply that a navigation has loaded or
/// a script has completed. Script results will need a separate async operation.
pub trait EngineView {
    fn identity(&self) -> &ViewIdentity;
    /// Authorize an imminent shell-driven DOM navigation, such as a hint click.
    /// Providers without a navigation-intent gate need no action here.
    fn authorize_navigation(&self) {}
    fn load_url(&self, url: &str) -> EngineResult;
    fn reload(&self) -> EngineResult;
    fn url(&self) -> EngineResult<String>;
    fn set_bounds(&self, rect: RectPx) -> EngineResult;
    fn set_visible(&self, visible: bool) -> EngineResult;
    fn zoom(&self, factor: f64) -> EngineResult;
    fn focus(&self) -> EngineResult;
    fn focus_parent(&self) -> EngineResult;
    fn evaluate_script(&self, script: &str) -> EngineResult;
    /// Click at `(x, y)`, in CSS pixels from the top-left of the page's viewport, as
    /// real user input. Unlike a click dispatched from script, the page treats it as a
    /// user gesture (clipboard writes, pop-ups, fullscreen). Providers that can't
    /// inject trusted input return an error, and the caller falls back.
    fn trusted_click(&self, _x: f64, _y: f64) -> EngineResult {
        Err("this engine can't inject trusted input".into())
    }

    // Optional services are queried explicitly, rather than successful no-ops.
    // Runtime versions can still reject an operation even if its service exists.
    fn history(&self) -> Option<&dyn History> {
        None
    }
    fn extensions(&self) -> Option<&dyn Extensions> {
        None
    }
    fn browsing_data(&self) -> Option<&dyn BrowsingData> {
        None
    }
    fn suspension(&self) -> Option<&dyn Suspension> {
        None
    }
}

pub trait History {
    fn can_go(&self, forward: bool) -> bool;
    fn go(&self, forward: bool) -> EngineResult;
}

/// Extensions belong to the view's storage context, which may be shared by views.
pub trait Extensions {
    fn list(&self, done: Completion<Vec<ExtensionInfo>>) -> EngineResult;
    /// Mutations dispatch asynchronously; acceptance is not a completion report.
    fn set_enabled(&self, id: String, enabled: bool) -> EngineResult;
    fn set_all_enabled(&self, enabled: bool) -> EngineResult;
    fn install_dir(&self, dir: &Path) -> EngineResult;
}

pub trait BrowsingData {
    /// Range is Unix seconds; None means all time. Only `done` confirms erasure.
    fn clear(&self, kind: DataKind, range: Option<(f64, f64)>, done: Completion) -> EngineResult;
}

pub trait Suspension {
    /// Hide the view first. Acceptance does not guarantee suspension: the runtime
    /// may decline it while page work is active.
    fn suspend(&self) -> EngineResult;
    fn resume(&self) -> EngineResult;
}

// Runtime polymorphism is the point of this contract: keep every service object safe.
const _: () = {
    #[allow(dead_code)]
    fn assert_dyn_compatible(
        _: &dyn EngineView,
        _: &dyn History,
        _: &dyn Extensions,
        _: &dyn BrowsingData,
        _: &dyn Suspension,
    ) {
    }
};

#[cfg(test)]
mod provider_tests {
    use super::*;
    const PROVIDERS: &[ProviderDescriptor] = &[
        ProviderDescriptor {
            id: "webview2",
            family: "blink",
            display_name: "WebView2",
            capabilities: Capabilities {
                private: true,
                disable_javascript: true,
                document_scripts: true,
                page_messages: true,
            },
        },
        ProviderDescriptor {
            id: "cef",
            family: "blink",
            display_name: "CEF test descriptor",
            capabilities: Capabilities {
                private: false,
                disable_javascript: false,
                document_scripts: false,
                page_messages: false,
            },
        },
    ];
    fn needs(storage: StorageMode) -> ViewRequirements {
        ViewRequirements {
            storage,
            disable_javascript: false,
            shell_bridge: true,
        }
    }

    #[test]
    fn concrete_ids_and_family_preferences_resolve_without_a_factory() {
        assert_eq!(
            resolve_provider(PROVIDERS, "blink", "cef").unwrap().id,
            "cef"
        );
        assert_eq!(
            resolve_provider(PROVIDERS, "WEBVIEW2", "cef").unwrap().id,
            "webview2"
        );
        assert!(resolve_provider(PROVIDERS, "servo", "webview2").is_err());
        assert!(resolve_provider(&[], "blink", "webview2").is_err());
    }

    #[test]
    fn private_rejection_never_calls_the_factory() {
        let called = std::cell::Cell::new(false);
        let result = build_checked(&PROVIDERS[1], needs(StorageMode::Private), |_| {
            called.set(true);
            Ok(())
        });
        assert!(result.unwrap_err().contains("private"));
        assert!(!called.get());
    }

    #[test]
    fn unsupported_bridge_and_nojs_are_rejected() {
        assert!(PROVIDERS[1]
            .validate(needs(StorageMode::Persistent))
            .unwrap_err()
            .contains("bridge"));
        let needs = ViewRequirements {
            storage: StorageMode::Persistent,
            disable_javascript: true,
            shell_bridge: false,
        };
        assert!(PROVIDERS[1]
            .validate(needs)
            .unwrap_err()
            .contains("JavaScript"));
    }

    #[test]
    fn failed_creation_consumes_its_identity_and_preserves_the_error() {
        let mut first = None;
        let error = build_checked(
            &PROVIDERS[0],
            needs(StorageMode::Private),
            |identity| -> EngineResult<()> {
                first = Some(identity.id);
                Err("runtime initialization failed".into())
            },
        )
        .unwrap_err();
        assert_eq!(error, "runtime initialization failed");
        let second = build_checked(&PROVIDERS[0], needs(StorageMode::Private), Ok).unwrap();
        assert_ne!(first.unwrap(), second.id);
        assert_eq!(second.storage, StorageMode::Private);
        assert_eq!(second.provider, "webview2");
    }
}
