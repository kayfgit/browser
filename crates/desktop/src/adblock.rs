//! Ad-blocking state: [`AdblockMode`] (what "on" means) and [`Adblock`], the live
//! mode the shell and every tab's navigation handler read.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Whether ad blocking is on, and which halves run.
///
/// There used to be two rival ENGINES here, mutually exclusive so only one ran. That
/// framing was wrong: neither one is a whole ad blocker, and running either alone left a
/// hole the other would have covered.
///
///   * uBlock Origin Lite filters at the NETWORK level, inside Chromium's own stack — fast,
///     and free of any host-process cost. But under WebView2 it doesn't see its `<all_urls>`
///     grant, so it demotes itself to "Basic" (`js/mode-manager.js`), which does no COSMETIC
///     filtering at all. It cannot hide YouTube's own ad slots, and never will here.
///   * `ADBLOCK_JS` plus the blocklist engine is exactly the other half: cosmetic hiding,
///     YouTube player-response pruning, popunder neutering, and the redirect guard. It runs
///     as an initialization script, so it can't lose a race with an extension service
///     worker, and it toggles live with no reload.
///
/// Neither is a whole ad blocker alone, so "on" means both.
///
/// HISTORY, because two innocent suspects were convicted here before the real one was
/// found. YouTube watch pages used to render as skeletons and videos used to sit black
/// while the playlist auto-advanced. Blame fell first on the `WebResourceRequested`
/// sub-resource blocker, then on uBO Lite. Both were wrong: the cause was `ADBLOCK_JS`
/// monkey-patching `JSON.parse` and `Response.prototype.json`, which YouTube's integrity
/// checks act on (see the note in `ADBLOCK_JS`). With those wrappers gone, uBO Lite and
/// the native layers coexist fine — measured on the same watch URL, both modes reach
/// `readyState == complete` and play with zero spurious navigations.
///
/// (The sub-resource blocker is still gone, on its own merits: it intercepted on the host
/// UI thread, which Microsoft's own docs say pauses page loads. See git history.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum AdblockMode {
    /// Both halves: uBlock Origin Lite for network filtering plus the native layers. The
    /// default, and the best coverage available.
    Ubo,
    /// Native layers only, extension disabled. Kept as an escape hatch — if a site
    /// misbehaves, this rules the extension out in one command without losing cosmetic
    /// filtering, YouTube handling or the redirect guard.
    Native,
    /// No ad blocking: extension disabled, `ADBLOCK_JS` inert, guards stood down.
    Off,
}

impl AdblockMode {
    /// The `:adblock <mode>` spelling, also the session key.
    pub(crate) fn name(self) -> &'static str {
        match self {
            AdblockMode::Ubo => "ubo",
            AdblockMode::Native => "native",
            AdblockMode::Off => "off",
        }
    }

    /// Parse a session key, defaulting to `Ubo` (both halves) for anything unknown,
    /// including sessions written before the field existed.
    pub(crate) fn parse(s: &str) -> Self {
        match s {
            "native" => AdblockMode::Native,
            "off" => AdblockMode::Off,
            _ => AdblockMode::Ubo,
        }
    }

    /// Whether ad blocking is on at all. The native layers — `ADBLOCK_JS` and the
    /// redirect/popup guards — follow this, so they can't disagree about whether blocking
    /// is active. The extension is separate: see [`extension`](Self::extension).
    pub(crate) fn blocking(self) -> bool {
        !matches!(self, AdblockMode::Off)
    }

    /// Whether the uBlock Origin Lite extension should be loaded and enabled. Only in
    /// [`Ubo`](Self::Ubo), and only because the user asked for it — see [`AdblockMode`].
    pub(crate) fn extension(self) -> bool {
        matches!(self, AdblockMode::Ubo)
    }
}

/// The live ad-blocking state: the current mode, the mode a bare `:ads` switches back
/// to, and a flag shared (cloned `Arc`) into every web tab's navigation handler so the
/// native redirect guard follows `:ads` without rebuilding the webviews.
pub(crate) struct Adblock {
    mode: AdblockMode,
    /// Never `Off`: returning to `Off` would make the toggle a no-op. Kept (rather than
    /// always going back to the default) so native → off → native round-trips.
    prev: AdblockMode,
    on: Arc<AtomicBool>,
}

impl Default for Adblock {
    /// On: uBO Lite plus the native layers.
    fn default() -> Self {
        Self {
            mode: AdblockMode::Ubo,
            prev: AdblockMode::Ubo,
            on: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl Adblock {
    pub(crate) fn mode(&self) -> AdblockMode {
        self.mode
    }

    /// What a bare `:ads` turns back on.
    pub(crate) fn prev(&self) -> AdblockMode {
        self.prev
    }

    /// Whether the native layers are active ([`AdblockMode::blocking`]).
    pub(crate) fn blocking(&self) -> bool {
        self.mode.blocking()
    }

    /// The flag handed to each tab's navigation handler; it follows [`set`](Self::set).
    pub(crate) fn shared_flag(&self) -> Arc<AtomicBool> {
        self.on.clone()
    }

    /// Switch to `mode`, remembering the mode left (unless it was `Off`) for
    /// [`toggled`](Self::toggled).
    pub(crate) fn set(&mut self, mode: AdblockMode) {
        if self.mode != AdblockMode::Off {
            self.prev = self.mode;
        }
        self.mode = mode;
        self.on.store(mode.blocking(), Ordering::Relaxed);
    }

    /// The mode a bare `:ads` switches to: off when blocking, else the previous mode.
    pub(crate) fn toggled(&self) -> AdblockMode {
        match self.mode {
            AdblockMode::Off => self.prev,
            _ => AdblockMode::Off,
        }
    }

    /// Adopt a session's saved mode names. A saved `prev` of `off` falls back to the
    /// mode itself, then to the default.
    pub(crate) fn restore(&mut self, mode: &str, prev: &str) {
        self.mode = AdblockMode::parse(mode);
        self.prev = match AdblockMode::parse(prev) {
            AdblockMode::Off => match self.mode {
                AdblockMode::Off => AdblockMode::Ubo,
                on => on,
            },
            on => on,
        };
        self.on.store(self.mode.blocking(), Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_returns_to_the_mode_it_left() {
        let mut ab = Adblock::default();
        ab.set(AdblockMode::Native);
        ab.set(ab.toggled());
        assert_eq!(ab.mode(), AdblockMode::Off);
        assert!(!ab.shared_flag().load(Ordering::Relaxed));
        ab.set(ab.toggled());
        assert_eq!(ab.mode(), AdblockMode::Native);
        assert!(ab.shared_flag().load(Ordering::Relaxed));
    }

    #[test]
    fn restore_never_keeps_off_as_the_mode_to_return_to() {
        let mut ab = Adblock::default();
        ab.restore("off", "off");
        assert_eq!((ab.mode(), ab.prev()), (AdblockMode::Off, AdblockMode::Ubo));
        assert!(!ab.blocking() && !ab.shared_flag().load(Ordering::Relaxed));
        ab.restore("native", "off");
        assert_eq!(
            (ab.mode(), ab.prev()),
            (AdblockMode::Native, AdblockMode::Native)
        );
        assert!(ab.shared_flag().load(Ordering::Relaxed));
    }
}
