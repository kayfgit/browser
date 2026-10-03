# Engine integration progress

The first two stages are implemented: the WebView2 boundary, followed by provider
selection, view identity, storage requirements, callback routing and persistence.
Default builds install WebView2; Windows builds with `servo-engine` also install
the experimental Servo provider. The research and remaining roadmap
are in [engine-switching-research.md](engine-switching-research.md).

Commands available now:

- `:engines` lists installed providers and their supported view options without starting
  a browser runtime.
- `:engine` shows the current and default providers.
- `:engine webview2` selects WebView2 for the active web pane; selecting its current
  provider is a no-op. It also recovers an unavailable-engine placeholder.
- `:engine blink` resolves the family alias to the installed concrete provider.
- `:engine default webview2` persists the provider for new web tabs. Existing tabs keep
  their provider when reopened or reloaded.
- `:engine servo` reopens the active HTTP(S) pane in Servo in the opt-in build.
  `:engine default servo` applies it to new web tabs.
- Uninstalled names (including `servo` in a default build) return an error without replacing a view.

The desktop owns `Box<dyn browser_engine::EngineView>` for web tabs and the temporary
profile-switch keepalive. All Wry/WebView2 objects stay under `engines/webview2/`.
Navigation, scripts, focus, bounds, visibility and zoom cross the contract; history,
extensions, data clearing and suspension are optional services. The contract has no
platform dependency, native-handle escape hatch or downcast.

Each view receives a process-unique, monotonically allocated ID for that incarnation.
IDs are not tab indices and are never reused. Page callbacks carry the originating ID;
a closed/replaced view cannot address its successor. Background URL/load events update
their own tab, while focus, modal-state, clipboard and fullscreen events require the
active view. Real pane clicks select the emitting visible pane instead of consulting a
possibly moved OS cursor. Navigation-intent stamps are also per-view. Hint and popup
new-tab requests retain the source provider and private setting.

Saved tabs, closed tabs and shell navigation entries record their concrete provider.
Older session files default to WebView2. An unavailable provider restores a native
placeholder that keeps the URL, page mode, provider and position in the split layout.
It remains restorable when the session is saved again. `:reload` retries its provider;
`:engine webview2` explicitly reopens it using the available provider. Private tabs
remain excluded from saved sessions and the closed-tab list.

View requirements explicitly distinguish persistent and private storage, disabling
page JavaScript and the document-script/message support needed by the shell bridge.
Unsupported requirements fail before factory dispatch. The WebView2 adapter additionally
verifies the actual runtime's storage mode on a hidden blank view before loading user
content: Wry 0.55 can otherwise fall back to a regular controller on older runtimes.
Private extension installation is also deferred until after verification. Existing
persistent WebView2 data stays in its legacy location; there is no cookie migration.

Profile operations select the active view's context. From a native page they select a
persistent view of the configured default provider, never an arbitrary private view or
another provider. Extension pickers retain their exact source view and cached items;
request tokens reject outdated list results. Closing/replacing the source prevents a
picker from changing another context. Browsing-data completion reports stay associated
with the original request and initiating AI chat, even if the page is subsequently closed.

Creation failure retains the outgoing view and shell history. Candidate surfaces stay
hidden until placement succeeds. A successful engine change reopens an HTTP(S) URL and
preserves pane position and compatible tab settings; it does not transfer live DOM,
forms, sockets, sign-ins or engine-native history. Before registering a second production
provider, add and verify unload/POST-navigation handling and runtime lifecycle behavior.
Currently the only live-to-live selection is WebView2-to-WebView2, which is a no-op.

Validation:

- `cargo test -p browser -p browser-engine --locked --offline`: 115 desktop tests
  and four contract tests passed; the opt-in native-runtime test is excluded by default.
- `cargo test -p browser --locked --offline runtime_storage_mode_is_verified_before_user_content -- --ignored --test-threads=1`:
  the native-runtime test passed separately. It creates hidden ordinary/private views
  in an isolated directory under `target/engine-tests`, verifies each mode, and rejects
  both mismatches. These temporary profiles are retained for diagnostics.
- `node --test crates/desktop/tests/bridge-hints.cjs`: all three regressions passed.
- `cargo build -p browser --locked --offline`: passed.
- The existing Windows manifest `maxversiontested` linker warning remains.

The user verified live browser behavior after both stages and reported that everything
works so far. Future adapter changes should repeat interactive checks: normal/no-JS/research/private pages, hints and copying,
split focus, background loads, history, zoom/resize, extensions, freeze/unfreeze,
profile switching and closing the last web tab. Test data clearing only with an
isolated WebView2 data directory: the shell's `:profile` stores workspace layouts and
does not isolate browser storage.

The pinned Servo experiment below is the first part of stage 3. The registry
currently contains compiled-in metadata; it
is not a downloadable-provider installer or stable plugin ABI. After proving a second
engine, extract provider runtime/storage lifecycles, add the executable transport and
negotiate protocol versions. Gecko remains a separate feasibility experiment.

## Stage 3: Servo qualification lab

The isolated [Servo/WebView2 lab](../experiments/servo/README.md) now contains a
Servo 0.5.0 child surface beside WebView2 in one Tao window and event loop. It has
its own workspace/lockfile, fresh engine-specific profiles, a local interactive
fixture and a smoke mode with an external startup/shutdown deadline. Normal desktop
builds remain independent of Servo. The complete lab passes `cargo check`, and its
native build and smoke test passed on 2026-09-09 using the local v143 linker.

The smoke test loaded both engines, verified document-start injection and Servo
script evaluation, painted and captured a Servo frame, and returned from teardown
within the external deadline. The captured PNG was visually inspected and showed
the fixture correctly. The user subsequently verified both panes, clicks, anchor
navigation, resizing and repeated startup/shutdown. Their input report led to native
child focus, wheel scaling, cursor handling and context-menu work in the lab, with
an expanded native-input smoke test. Integration into the daily browser remains a
separate qualification step; see the lab README for remaining manual checks and
Servo 0.5's upstream interactive text-selection limitation.

The expanded smoke passed on 2026-09-10: native focus transfer, exactly one character
per key, Backspace, committed accented text, the configured wheel distance, native
menu open/dismiss and focus recovery were verified against the real engines.
The mixed-focus cursor case and selecting individual menu actions still require
manual retesting. Full IME preedit remains a separate integration task.

Two build issues have concrete workarounds: pin content-security-policy to 0.8.1
(the version in Servo's release lockfile), and supply libclang locally. This machine
has VS 2019 and SDK 10.0.18362; Servo documents a newer baseline. The lab includes
an optional, checksum-verified Microsoft v143 linker/library setup for testing with
the existing compiler, without modifying the installed toolchain.

The first native run also exposed a Servo 0.5 initialization bug: WGL restores the
previous context after creation, but Servo loaded GL functions and queried
`GL_VERSION` before activating the new context. The lab carries a documented local
patch to `servo-paint-api` that fixes this order and cleans up on activation failure.
A focused native context probe and the full two-engine smoke test passed afterward.
The shared Cargo registry and the daily browser's dependencies were not modified.

The lab is not yet a registered `EngineView` provider. Full IME composition and the
remaining qualification items in the lab README still precede enabling
`:engine servo` in the daily browser.

## Stage 3: shared shell bridge

The user verified the corrected native input and approved proceeding to the bridge.
Servo 0.5 has no native host-message callback, so the lab implements an experimental
transport through its public asynchronous evaluation API. Document-start code queues
untrusted messages; the host reads bounded batches and acknowledges accepted sequence
numbers. Native navigation epochs reject retired replies, and document tokens keep
old acknowledgements from clearing a new document. Overflow and malformed responses
stop the current bridge instead of silently dropping shell actions. This transport
uses no network endpoint, console interception or new Servo patch.

The production bridge and hint scripts now live in `crates/desktop/scripts` and are
included by both WebView2 and the lab. An explicit provider option lets Servo retain
its native context menu; WebView2 keeps its existing default behavior. The lab host
demonstrates Normal/Insert/Passthrough transitions and hint activation, and preserves
WebView2 focus when delayed Servo messages arrive.
Shell commands in the standalone lab apply only to Servo; its WebView2 pane remains
an ordinary-input comparison surface.

This is a top-level-document transport with polling overhead (16 ms while focused,
100 ms otherwise), not a new native IPC capability. Subframes and the remaining host
actions are not yet wired. In particular, polling cannot reproduce the synchronous
`nav-intent` stamping used by WebView2's navigation guard. Provider runtime ownership,
view-scoped routing and the remaining capabilities must be integrated separately.

Validation on 2026-09-10: the native smoke passed shared Escape and hint handling,
native Insert/Escape transitions, SPA URL messages, full navigation and strict-CSP
reinjection, plus background focus isolation. Four protocol tests, nine Node tests,
115 desktop tests and four engine-contract tests passed. Lab formatting and diff
whitespace checks passed; repository-wide default formatting reports pre-existing
differences in untouched crates.

The subsequent input-hint fix keeps shell keys owned through physical release and
suppresses Tao's synthetic focus events, preventing the hint label from being typed
into the newly focused input. Two key-ownership tests and an expanded native smoke
passed: focus-event replay and held-key repeat leave the input empty, while a fresh
press of the same key types once.

## Stage 3: Servo page contract

The lab now depends on `browser-engine` and implements its existing `EngineView`
and `History` contracts in `experiments/servo/src/page.rs`. The adapter retains its
runtime, GL context and native surfaces through view teardown. Lab shell operations
use this interface for script dispatch, focus, URL navigation, reload, bounds, zoom
and history. Normal-mode `+`/`-`/`0`, `r`, and `H`/`L` expose the new operations.

Native qualification on 2026-09-10 passed through a trait object, including DOM
layout changes at 150% zoom and reset, native/rendering dimensions, hide/show focus
isolation, back/forward destinations, and reload discarding a JavaScript marker.
The existing input and hint regressions and complete teardown passed. Eight lab
library tests cover transport, key ownership and conservative capability/zoom policy.
The CSP fixture encoding was corrected, and the test now verifies that its inline
page script is blocked while injected host messaging continues to work.

No production provider was registered. The lab's metadata explicitly rejects
private/no-JavaScript requests and the full production bridge requirement. The next
step is multi-view runtime ownership and scoped callbacks, including retiring and
recreating views with pending replies. Subframes, dialogs, unload policy and the
other lab qualification gates still precede enabling `:engine servo`.

## Experimental main-browser integration

The user chose to start daily testing before every qualification item is complete.
The Windows-only `servo-engine` feature now registers Servo in the real browser.
`run-servo.ps1 -UseLocalLinker` builds and launches that application, with its usual
commands and saved layouts. `-Scratch` starts a throwaway shell layout. The feature
is not part of a default Cargo build. Plain `install.ps1` installs the same tested
dual-engine build and PTY companion through the normal Start Menu shortcut.
`-Servo` remains accepted for compatibility; `-WebView2Only` explicitly installs
a WebView2-only release build. The Servo install automatically uses the local
native linker when present and reuses the lab cache instead of doing a separate
optimized release build.

Installation validation on 2026-09-17: `install.ps1 -Servo` updated the existing
per-user executable, PTY companion and `browser.lnk` in the Start Menu Programs
folder. Installed binary hashes matched the tested build. The installed executable
passed the visible split smoke from the shortcut's working directory, with isolated
profiles, including native command-bar engine switches and pane geometry checks.

The main adapter owns each view, rendering context and child window. Servo 0.5
initializes process-global options only once: destroying and recreating its runtime
panics. Therefore the first Servo view initializes a shared runtime retained until
application shutdown. Closed pages and surfaces are released, and the runtime can
serve new views afterward. A fresh shell still starts without Servo, but after first
use its runtime memory stays allocated until exit. Window construction uses a scoped event-loop target on the UI
thread. Child events/redraws are separated from parent chrome events. Input, native
menus, hint-key ownership and the bounded queue parser are shared with the lab.
Callbacks carry view identities, and transport replies additionally carry navigation
epochs; stale views cannot mutate their replacements. Servo initialization failures
reported by the factory leave the outgoing pane intact.

The normal shell bridge, hints, find, caret and feature scripts run in top-level
Servo documents. Navigation, native history, zoom, focus and layout use `EngineView`.
WebView2's accelerator-key handling (`shellkeys.rs`) doesn't apply to Servo views;
the shell's focus reclaim provides mode recovery there. Servo storage lives
under the browser data directory in `engines/servo`, separate from WebView2 cookies
and sign-ins. Private/no-JavaScript creation is rejected before building a view.

Known limits for this experimental build: no extensions/uBlock or native network
filter, downloads, full IME preedit, interactive page-text selection, subframe bridge,
or complete dialogs/permission controls. Page-side blocking is only the existing
JavaScript layer. Engine switches reopen the URL without transferring forms or live
page state; full unload/POST policy remains unfinished. Servo runs in-process, so
a runtime crash can terminate the application. Use `:engine webview2` for sites that
need capabilities Servo does not yet provide.

`run-servo.ps1 -Action Smoke -UseLocalLinker` runs a loopback-only fixture in the main
browser, using fresh Servo and WebView2 directories. It exercises switching, native
input hints, shared-runtime splits, retired callbacks and last-view close/reopen.
Logs and temporary profiles are retained under `target/servo-lab/desktop-smoke`.
The test does not write the user's config or saved session. The two profile overrides
are `BROWSER_SERVO_DATA_DIR` and `BROWSER_WEBVIEW2_DATA_DIR`.

The smoke explicitly shows its temporary window and requires a presented Servo
frame, not just a loaded document. `-Scenario Split` starts with two WebView2 panes
on the loopback fixture and switches the right pane using native command-bar Enter;
`-Scenario Example` repeats that sequence on `https://example.com/` and therefore
requires network access. Both scenarios switch back through the command bar and
check that the split survives. The default scenario retains the broader input,
runtime-lifetime and stale-callback checks. Each run records its process ID beside
the logs for targeted debugging.

Validation on 2026-09-15: the final main-browser build passed the native smoke,
including hint activation without leaking the label into the input, subsequent
typing, two Servo panes sharing one runtime, mixed WebView2/Servo splits, rejected
private/no-JavaScript requests, retired callbacks, and closing/reopening the last
Servo pane. The test process exited successfully. Standard tests passed (115
desktop and four engine tests, with one existing native test ignored), as did all
eight lab library tests and nine Node bridge tests. Formatting checks passed for
the new modules using their respective desktop/lab editions. Whitespace checks
passed for this work; the user's separate TODO additions were preserved.

The initial smoke above did not require visible frame presentation and missed a
redraw starvation bug. Servo 0.5 repeats `notify_new_frame_ready` on each runtime
pump until a view paints. Pumping after every shell event and posting every repeated
notification kept the Windows posted-message queue busy, starving `WM_PAINT` and
leaving a visible Servo pane black. The adapter now coalesces requests per view,
keeping the pending flag set until a successful paint. Hidden panes also retain
that flag until shown and painted, so they cannot flood the event loop.

The visible `Example` scenario reproduced the missing first frame before the fix.
After coalescing, `Example`, the network-independent `Split` scenario, and the full
default native smoke all passed on 2026-09-15, including presentation and successful
process exit. The split scenarios also verified a subsequent command-bar switch
back to WebView2. Formatting and scoped whitespace checks passed.

Visual integration: Servo children disable Tao's undecorated-window shadows and
native resizing. The shadow insets otherwise shift the client area eight pixels
right and one down on the tested desktop, leaving a black strip at the split.
Repeated bounds/visibility assignments are no-ops. Split chrome now presents only
the regions outside live web children, preserving their pixels during command-bar
typing, cursor blinks and loading animation; borders and native panes still repaint.
This avoids the full-surface GDI copy overwriting Servo's OpenGL output.

`:resources` identifies the loaded Servo runtime and open-view count. Servo's CPU,
memory and I/O are already included in the browser process row and overall totals;
the monitor explains that the shell and Servo share those counters. It does not
invent a separate per-engine memory figure. A loaded runtime remains listed after
its last view closes, until application exit.

Visual validation on 2026-09-15: native geometry checks reproduced the `(8, 1)`
client-area offset before disabling child shadows and passed afterward. The split
smoke passed exact client bounds, 20 command-bar redraws, resource-accounting text,
and switching back; the broader visible smoke passed input/hints, multiple Servo
views, stale callbacks and close/reopen. A pixel-coverage regression verifies that
presentation rectangles exclude all web pixels while covering every remaining
pixel exactly once, including borders, overlapping/clipped children and frozen mode.

## Servo 0.6.0 and shipping

On 2026-10-03 Servo was upgraded to 0.6.0, the base of its next LTS line, and became
a regular engine: release builds (cargo-dist, `[package.metadata.dist] features`)
include it, and the `:engines` listing no longer calls it experimental. A plain
`cargo build` still leaves it out, because compiling Servo is slow and memory-hungry.

The upgrade's only embedding-API change that affected the adapter was the mouse-button
rename (`Left`/`Middle`/`Right` became `Primary`/`Auxiliary`/`Secondary`). The vendored
`servo-paint-api` patch is still needed: 0.6.0 still loads OpenGL before making the new
WGL context current. `content-security-policy` follows 0.6.0's release lockfile (0.8.2).

**Crash isolation.** Servo runs in multi-process mode: page scripts and layout run in
content processes, which Servo starts as `browser --content-process <token>` (the
first thing `main` checks). `BROWSER_SERVO_SINGLE_PROCESS=1` turns this off for
debugging. Servo reports a panicking page through `notify_crashed`, but a hard crash or
a killed process leaves its pages silently dead, so `engines/servo/watchdog.rs` puts
the browser in an observe-only job object and receives a notification when a content
process exits abnormally. Every Servo page then gets a liveness check; pages that fail
it or don't answer within three seconds become a crash placeholder
(`UserEvent::EngineCrashed`) that keeps the URL, and `:reload` builds a fresh view.
Reloading or scripting the dead page itself doesn't work in 0.6.0 (it trips an
assertion in the replacement content process). Servo 0.6 also waits without limit at
shutdown for a dead page to confirm it closed, so `servo::shutdown` exits the process
after five seconds; the session, terminals and updates are handled before that point.
Release builds now unwind on panic instead of aborting, so a panic in one of Servo's
threads in the browser process can't take the whole browser down.

Crashes in the browser-process parts of Servo (networking, the compositor, OpenGL)
are not isolated. Moving all of Servo into its own host process is the next step, and
the one that separately packaged engines and Gecko need anyway.

**Shared page messages.** WebView2 and Servo previously decoded the page bridge's
messages separately, and Servo's copy had fallen behind: it dropped `hint-click`
(hints on buttons did nothing), `shell-key` and `reclaim`. Both now use
`engines::events::decode_page_message`. Servo implements trusted clicks with embedder
mouse input, which pages treat as a user gesture.

Validation on 2026-10-03, all with the native smoke against the real engines:
`Default` (now including follow on inputs, buttons and links, new-tab, copy and scroll
hints, and a trusted-click check), `Split`, and `Crash` (kills the content process,
requires the placeholder, reloads, then requires a clean quit). Workspace fmt, clippy
and tests passed on the default build.
