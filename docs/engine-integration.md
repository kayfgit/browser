# Engine integration progress

The first implementation stage is the WebView2 extraction. WebView2 remains the only
production provider. Runtime switching commands and a second engine are not implemented
yet. The broader design and its research are in [engine-switching-research.md](engine-switching-research.md).

The desktop shell now owns `Box<dyn browser_engine::EngineView>` for web tabs and the
temporary profile-switch keepalive. Navigation, scripts, focus, zoom, visibility and
physical bounds cross that interface. Native history, extensions, browsing-data clearing
and suspension are optional service interfaces. The contract has no native handles,
downcasts, Wry dependency, platform dependency or startup side effects.

All Wry and WebView2 COM access is confined to `crates/desktop/src/engines/webview2/`.
That adapter owns view construction, initialization scripts, callback registration,
native history, navigation guards, extension APIs, data clearing, suspension and favicon
retrieval. Favicon decoding/painting, navigation policy, hints/find/caret scripts and
workspace state remain shell code. The adapter receives an explicit options snapshot,
not access to `App`.

This is an internal module boundary in the existing desktop binary. It is not yet an
independently packaged provider crate or stable plugin SDK. Construction still takes
WebView2-specific options and callbacks still use the existing shell `UserEvent` enum.
The unused speculative engine contract was replaced with the smaller implemented
interface; unimplemented factories, software rendering hooks and downcast escape hatches
were removed rather than advertised as working features.

The existing document-start scripts, private-controller option, browser environment
arguments, extension loading policy, navigation/download guards and keepalive behavior
are retained. Creating a blank/native tab still requires no engine. View destruction
releases the owned view; shared runtime shutdown is the provider's responsibility and
need not finish synchronously.

Data clearing now distinguishes dispatch from completion. A failed WebView2 completion
reports an error in the initiating AI chat or status bar instead of claiming the data
was cleared. Extension-list failures also reach the status bar. Existing extension
mutations and suspension remain best effort; their immediate return does not confirm
asynchronous completion.

Validation for this stage:

- `cargo check -p browser-desktop --offline` passed.
- `cargo test -p browser-desktop -p browser-engine --offline` passed: 110 desktop tests,
  including four new engine-boundary regressions; engine library/doc tests also passed.
- `node --test crates/desktop/tests/bridge-hints.cjs` passed all three regressions.
- `cargo build -p browser-desktop --locked --offline` passed. The existing Windows
  manifest `maxversiontested` linker warning remains.
- The test adapter exercises shell ownership/replacement, error propagation, physical
  pane bounds, absent optional services and private/research history metadata without
  loading a browser runtime. A source-boundary test prevents Wry/COM references from
  returning to other desktop modules.

The user also tested the live browser and reported that it looks and works correctly.
For future adapter changes, exercise the following live behavior before moving on to
a second runtime, exercise ordinary/no-JS/research/private pages, hints and copying,
split focus and tab switching, back/forward, zoom/resize, extensions, freeze/unfreeze,
profile switching, fullscreen and closing the last web tab in the desktop application.
Test data clearing only with an isolated WebView2 user-data directory: the shell's
`:profile` command saves workspace layouts and does not isolate browser storage.

Next stage: provider IDs and discovery, explicit storage requirements, per-view event
identity, capability validation and session persistence. Tests must cover unavailable
providers, failed creation, stale callbacks and rejected private-mode requests before
exposing engine-switching commands. Profile-scoped operations currently use the first
open web view; that routing must become context-aware before multiple providers coexist.
Afterward, prove a pinned Servo build beside WebView2 using a separate native surface.
