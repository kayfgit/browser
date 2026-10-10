//! Ad-blocking state: one on/off switch the shell and every tab's navigation handler read.
//!
//! Each engine has ONE blocker, and this switch drives all of them:
//!
//!   * WebView2: the bundled uBlock Origin Lite extension does the blocking — network
//!     rules, cosmetic hiding and scriptlets (YouTube included). It runs in its
//!     "optimal" mode here: the profile grants it `<all_urls>` and it registers its
//!     cosmetic/scriptlet content scripts. It can only change state on a page's next
//!     load, so `:ads` reloads the visible pages.
//!   * Servo: no extension support, so the page-side `ADBLOCK_JS` hides ad containers
//!     and handles YouTube until a native engine-level blocker replaces it.
//!   * Both: the redirect/popup guard (`NAVGUARD_JS` plus the native navigation and
//!     new-window handlers backed by the [`blocklist`](crate::blocklist) engine). That is
//!     browser-level work an extension can't do: it cancels forced cross-site top
//!     navigations nobody clicked for.
//!
//! HISTORY. Until 2026-10 the WebView2 side ran uBO Lite AND a native cosmetic/YouTube
//! layer together, on the belief that uBO Lite was stuck in its network-only "basic"
//! mode under WebView2. That was wrong (measured on runtime 154: it reports
//! `<all_urls>` and runs its content scripts). What was really wrong: the profile had
//! uBO Lite DISABLED and nothing turned it back on — re-adding the extension stopped
//! re-enabling it, and failures went unreported. The two layers also both pruned
//! YouTube's player data. Now the shell explicitly enables or disables the bundled copy
//! and reports failures in `:errors` (see `engines::webview2::extensions::sync_bundled`).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Whether ad blocking is on, shared (cloned `Arc`) into every web tab's navigation
/// handler so the redirect guard follows `:ads` without rebuilding the webviews.
pub(crate) struct Adblock {
    on: Arc<AtomicBool>,
}

impl Default for Adblock {
    fn default() -> Self {
        Self {
            on: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Adblock {
    pub(crate) fn on(&self) -> bool {
        self.on.load(Ordering::Relaxed)
    }

    pub(crate) fn set(&self, on: bool) {
        self.on.store(on, Ordering::Relaxed);
    }

    /// The flag handed to each tab's navigation handler; it follows [`set`](Self::set).
    pub(crate) fn shared_flag(&self) -> Arc<AtomicBool> {
        self.on.clone()
    }

    /// The session spelling: `"ubo"` (on) or `"off"`. Older builds read the same key,
    /// and treat anything but `"off"` as on, so a downgrade keeps the user's choice.
    pub(crate) fn session_name(&self) -> &'static str {
        if self.on() {
            "ubo"
        } else {
            "off"
        }
    }

    /// Adopt a session's saved mode. Only `"off"` is off; the retired `"native"` mode,
    /// `"ubo"` and anything unknown are on.
    pub(crate) fn restore(&self, mode: &str) {
        self.set(mode != "off");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retired_modes_restore_as_on() {
        let ab = Adblock::default();
        for (saved, on) in [("ubo", true), ("native", true), ("off", false), ("", true)] {
            ab.restore(saved);
            assert_eq!(ab.on(), on, "{saved:?}");
            assert_eq!(ab.shared_flag().load(Ordering::Relaxed), on);
        }
        ab.set(false);
        assert_eq!(ab.session_name(), "off");
        ab.set(true);
        assert_eq!(ab.session_name(), "ubo");
    }
}
