//! The JavaScript the browser injects into pages, one file each under
//! `crates/desktop/scripts/`. Every web view gets `IPC_PRELUDE`, `BRIDGE_JS`,
//! `FIND_JS`, `CARET_JS` and `FEATURES_JS`, plus `NAVGUARD_JS` (in its own init script
//! on WebView2); Servo views also get `ADBLOCK_JS`; `RESEARCH_JS` only in `:research`
//! tabs; `HINT_JS` on demand.

/// Defines `window.__post`, the ONE safe path for page→shell IPC, prepended to every
/// injected bundle so it exists before anything posts. wry builds an `http::Uri` from
/// the *sender frame's* URL and `unwrap()`s it (`webview2/mod.rs`); a post from a frame
/// whose URL isn't a valid http(s) URI — a `file://` page, or an `about:blank` / `data:`
/// / `blob:` ad iframe — panics inside the FFI callback and ABORTS the whole process.
/// So `__post` only forwards when the frame is http(s); elsewhere it silently drops the
/// message (those pages lose IPC niceties like focus-reclaim, but they no longer crash).
pub(crate) const IPC_PRELUDE: &str = include_str!("../scripts/ipc-prelude.js");

/// Injected into every page: the page half of the mode system. It reads a synchronous
/// `window.__mode` flag (kept in sync by the shell) and, per mode, handles the keys
/// the shell owns: in `insert`, Esc leaves (and leaving the field ends Insert); in
/// `passthrough`, only Ctrl+S / Shift+Esc leave; in `normal`, a key that reaches the
/// page is handed back to the shell (`shell-key:`, see `shellKey`). It also reports
/// clicks (field → Insert, control → let the page keep focus), link hovers, SPA
/// navigations and HTML fullscreen, and draws the right-click menu.
pub(crate) const BRIDGE_JS: &str = include_str!("../scripts/bridge.js");

/// Injected on demand to drive hint mode. Defines `window.__hintShow/Input/Clear`.
/// The shell collects the typed label and calls `__hintInput`; the page filters
/// badges and, on an exact match, clicks the target and reports back via IPC.
pub(crate) const HINT_JS: &str = include_str!("../scripts/hints.js");

/// Injected into `:research` tabs. Strips the heavy/noisy stuff (video, audio,
/// embeds, ad/social iframes) on document-create and as the page mutates, while
/// leaving images and text intact — a lighter browse for "how do I…" research.
/// Page scripts still run (so SPAs work); this only prunes the DOM after the fact,
/// since wry exposes no sub-resource request blocker to stop the loads outright.
pub(crate) const RESEARCH_JS: &str = include_str!("../scripts/research.js");

/// Page-side cosmetic ad blocker, for Servo only (WebView2 tabs get all of this from the
/// bundled uBlock Origin Lite). Injected at document-start while adblock is on:
///   * cosmetic — imperatively hide generic ad containers (EasyList-ish) plus YouTube's
///     ad slots (inline `display:none`, which survives a strict CSP a `<style>` wouldn't);
///   * YouTube — prune the ad descriptors from the player-response JSON, skip/seek past
///     in-player ads, and remove the "ad blocker" enforcement modal.
///
/// It follows the live `on` flag `NAVGUARD_JS` owns (`window.__adblockCosmetic`). To be
/// replaced by an engine-level blocker for Servo.
#[cfg(all(windows, feature = "servo-engine"))]
pub(crate) const ADBLOCK_JS: &str = include_str!("../scripts/adblock.js");

/// The redirect/popup guard, injected into every web tab (all frames) while adblock is on:
/// reports a trusted cross-site gesture (`nav-intent`, the signal the native redirect
/// guard needs) and neuters scripted cross-origin `window.open` popunders. It owns the
/// live toggle: the shell flips it via `window.__setAdblock` on `:ads` and bakes the
/// initial value as `__adblockDefault` per tab.
pub(crate) const NAVGUARD_JS: &str = include_str!("../scripts/navguard.js");

/// Live page-feature toggles, injected into every web tab. Four independent flags,
/// each seeded from `window.__featureDefaults` (baked per tab from the shell's state)
/// and flipped live by `window.__setToggle(name, on)` — no reload:
///   * `mute`      — keep every `<video>`/`<audio>` muted (observed + a 1s safety tick).
///   * `css`       — disable every stylesheet/`<style>`/`<link rel=stylesheet>`.
///   * `video`     — strip video players and embeds, sparing captcha iframes.
///   * `scrollbar` — hide the page's scrollbars.
///
/// (Pop-up/popunder blocking lives under `:ads` — the native new-window handler plus
/// the all-frames `window.open` neuter in [`NAVGUARD_JS`].)
///
/// Mirrors the `:ads` pattern so `:mute`/`:css` apply instantly to all tabs.
pub(crate) const FEATURES_JS: &str = include_str!("../scripts/features.js");

/// Page-side selection mode (vim selection on live web pages). The shell
/// forwards motions here; we drive a real DOM Selection via `Selection.modify`
/// (Chromium supports character/word/line/lineboundary/documentboundary), draw a
/// fixed-width block cursor over the logical cursor, and post the yanked
/// text back over IPC. `__caretEnter` places the caret at the viewport center
/// WITHOUT selecting — press `v`/`V` again for charwise/linewise visual; `__caretKey`
/// moves or extends and scrolls the window when the cursor nears a viewport edge
/// (vim scrolloff); `__caretYank` copies; `__caretEsc` collapses then exits (posting
/// `caret-exit` so the shell leaves selection mode).
pub(crate) const CARET_JS: &str = include_str!("../scripts/selection.js");

/// Page-side find-in-page: `__find(q)` highlights every match (CSS Custom Highlight
/// API — no DOM mutation, so it can't break the page), scrolls to the first, and
/// `__findNext`/`__findPrev` move the "current" highlight. `__findClear` removes it.
pub(crate) const FIND_JS: &str = include_str!("../scripts/find.js");
