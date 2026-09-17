# Servo / WebView2 qualification lab

This is an isolated Windows experiment for stage 3 of the
[engine roadmap](../../docs/engine-switching-research.md). It puts Servo 0.5.0 in
a Tao child window on the left and WebView2 in a separate child surface on the
right. One Tao event loop drives both. It does **not** install or register a Servo
provider itself. The main browser now has a separate opt-in `servo-engine` feature;
use the repository-root `run-servo.ps1` to test `:engine servo` in the real shell.

## Shared shell bridge

The lab now injects the same `crates/desktop/scripts/bridge.js` and `hints.js`
assets used by the daily browser. Its Servo transport uses the public asynchronous
JavaScript evaluation API to read an acknowledged message queue. It does not need
a network endpoint, custom URL navigation, console interception or an engine fork.

Click Servo and press **F6** to toggle between passthrough and the lab shell.
In Normal mode, **f** shows hints, **i** focuses the first input in Insert mode,
and **v** enters passthrough. Type a hint label to activate its target; Escape
cancels hints or leaves Insert. Ctrl+S or Shift+Escape leaves passthrough. The
window title displays the Servo mode and received-message count. Shell commands
apply only to Servo in this lab; WebView2 is an ordinary-input comparison pane.
Servo uses its native context menu; WebView2's
production script keeps its existing menu behavior by default.

In Normal mode, **+** (or **=**) and **-** change page zoom, **0** resets it,
**r** reloads, and **H** / **L** go backward / forward in history (Shift+h/l).
These controls call the shared `browser-engine::EngineView` contract. A navigation
returns the lab to passthrough; press F6 again for shell commands. The title shows
the zoom percentage. The fixture's anchor links create entries for testing history.

The transport reads at most 32 messages per batch, polls at 16 ms while the Servo
surface or lab shell has native focus and 100 ms otherwise, and permits one
outstanding read per document. Acknowledgements prevent redelivery, native
navigation epochs reject retired replies, and document tokens prevent an old
acknowledgement from erasing a new document's messages. Tokens are routing data,
not authentication. Messages run in the page's realm and remain untrusted.

The queue is capped at 256 messages / 65,536 UTF-16 code units, with 8,192 code
units per message. Malformed replies, sequence gaps, overflow or a ten-second
unanswered read stop the bridge for that navigation; reload to recover. Reads
begin after Servo reports that the new document's head has been parsed.

This first transport covers the **top-level document only**. Subframe injection
is intentionally skipped so a frame cannot consume Escape into an undrained
queue. Cross-origin-frame routing, all production message actions and provider
lifecycle integration remain before registering Servo. Polling also cannot give
WebView2's synchronous `nav-intent` timing guarantee; a Servo navigation policy
must account for that rather than copying that guard unchanged.

The lab has its own Cargo workspace and committed lockfile. Ordinary desktop
builds do not resolve or compile Servo. The content-security-policy dependency is
pinned to the version in Servo v0.5.0's release lockfile: 0.8.3 adds an enum variant
that breaks compilation of servo-net 0.5.0.

The lab also carries a small
[servo-paint-api patch](vendor/servo-paint-api/PATCH.md). The original Windows path
queried `GL_VERSION` before activating the newly created WGL context and panicked
before loading a page. The patch activates the context before GL initialization
and cleans it up if activation fails. It affects this workspace only.

## Build and run

Use Windows x64, the MSVC Rust toolchain, CMake, and libclang. Upstream requires the
latest VS 2022 v143 C++ tools, Windows SDK >= 10.0.19041 and ATL; see the
[Servo Windows build instructions](https://book.servo.org/building/windows.html).
The default feature set includes baked-in resources and SpiderMonkey JIT. Media
via GStreamer and WebGPU are not enabled in this experiment.

From the repository root:

```powershell
./experiments/servo/run.ps1 -Action Check
./experiments/servo/run.ps1 -Action Test
./experiments/servo/run.ps1 -Action Smoke
./experiments/servo/run.ps1 -Action Run
./experiments/servo/run.ps1 -Action Run -Url https://example.com
```

`Smoke` builds first, runs the local fixture, checks both engines' document-start
script injection, evaluates JavaScript in Servo, paints after the check, and drops
both engines, and writes `target/servo-lab/debug/servo-smoke.png` for visual review.
A separate 90-second process deadline catches initialization or
shutdown hangs. Logs go to `target/servo-lab/smoke.{stdout,stderr}.log`.
The test also sends native messages to the lab's Servo HWND and checks DOM results:
clicking an input transfers focus from WebView2, typing inserts exactly one character,
Backspace deletes it, an IME-style text commit inserts an accented character,
one wheel notch scrolls the configured distance, right-click
opens and dismisses a native context menu, and a second WebView2-to-Servo transfer
recovers focus.
It uses no global input injection or pointer movement. Smoke mode dismisses menus
automatically using a timer on its own window; selecting menu actions and cursor
behavior still need manual checks.

`Run` opens the same local fixture for mouse, typing, accents, scrolling, animation,
anchor navigation, resizing and DPI checks. An optional HTTP(S) URL loads in both
engines. The native title identifies left/right engines. Each run creates distinct
Servo and WebView2 directories under `target/servo-lab/debug/lab-data`; these are
retained for diagnosis and never reuse the daily browser's profile.

The script respects `LIBCLANG_PATH`. For a small, repository-local libclang setup:

```powershell
python -m pip install --target target/servo-tools --no-deps libclang==18.1.1
```

The runner detects its `clang/native/libclang.dll` and sets `LIBCLANG_PATH` for
the build only. No global environment edits are needed. Alternatively point
`LIBCLANG_PATH` to an existing LLVM installation's `bin` directory.

If only VS 2019 is installed, its C++ library lacks symbols used by Servo's prebuilt
SpiderMonkey, such as `__std_search_1`. For this lab, an optional local final-link
workaround downloads Microsoft v143 linker/CRT packages, checks pinned SHA-256
hashes, and verifies the linker's Microsoft signature:

```powershell
./experiments/servo/setup-linker.ps1
./experiments/servo/run.ps1 -Action Smoke -UseLocalLinker
./experiments/servo/run.ps1 -Action Run -UseLocalLinker
```

It extracts files under `target/servo-tools`, changes no installed tools or global
environment, and passes the newer linker/libraries only to the final lab binary.
It still needs an existing MSVC compiler and Windows SDK. This is a qualification
workaround, not a replacement for upstream's supported full VS 2022/SDK setup.

Build output defaults to `target/servo-lab`; `-TargetDirectory` overrides it.
Debug symbols and incremental compilation are disabled to reduce disk use. Start
with one build job; `-Jobs 2` is available on machines with more free RAM. Roughly
30–40 GB free is a planning allowance, not a measured requirement. SpiderMonkey
normally downloads an upstream prebuilt
static library; a source-build fallback needs additional upstream tooling.

## Verified on 2026-09-09

The lab passed `cargo check` and built on Windows x64 with Rust 1.98.0, local
libclang 18.1.1, the existing VS 2019 compiler/SDK 10.0.18362, and the optional
Microsoft v143 final linker/libraries. This demonstrates this setup, not general
compatibility with every older toolchain.

`run.ps1 -Action Smoke -UseLocalLinker` passed: both engines loaded, document-start
injection was checked, Servo evaluated JavaScript and painted, a frame was captured,
and teardown returned within the external timeout. The captured frame was visually
inspected and showed the expected text, form, button, links and colored geometry.
The WGL initialization fix also passed a focused native creation/destruction probe.

On 2026-09-10 the expanded native-input smoke test passed, including single-character
typing (without duplicate insertion), Backspace, a committed `é`, native menu
open/dismiss and repeated focus recovery. One wheel notch moved 72 CSS pixels with
this machine's three-line setting. The build and formatting check passed, and the
smoke process returned from teardown within its deadline. These checks exercise
native messages and the real engines; they do not qualify a full OS IME session.

The shared bridge smoke also passed on 2026-09-10. It exercised the production
Escape handler and hints, a native `i`/Escape round trip without inserting the shell
key into the input, a same-document URL event, reinjection after full navigation,
messages under `script-src 'none'`, and preservation of WebView2 focus when a delayed
Servo message arrives. Rendering/input/menu checks and teardown still passed.

The input-hint regression also passed: the shell retains a hint key until its real
release and ignores Tao's focus-synthesized key events. Focusing an input therefore
does not insert the hint label or its held-key repeats. The smoke test replays the
captured hint event as a focus event, sends a native repeat, verifies the input stays
empty, then verifies a fresh press of the same key inserts exactly one character.

Six Rust tests (four protocol and two key-ownership tests) and nine Node tests passed, covering acknowledgements,
navigation/document changes, overflow/limits, malformed batches, hint behavior and
native-menu opt-in. Run them with:

```powershell
./experiments/servo/run.ps1 -Action Test
node --test crates/desktop/tests/bridge-hints.cjs experiments/servo/tests/bridge-queue.cjs
```

The daily browser's 115 desktop and four engine-contract tests passed after sharing
the scripts. Lab formatting and `git diff --check` passed. The repository-wide
default `cargo fmt --all -- --check` reports existing formatting differences across
untouched crates; this change does not reformat those files.

## Shared page adapter

`src/page.rs` now implements the same `EngineView` and optional `History` interfaces
used by the daily browser's WebView2 provider. It owns the Servo view, a shared
runtime reference, rendering context and native-window references in destruction
order. The event loop pumps and paints through the adapter. Shell script dispatch,
focus, navigation, bounds, zoom and history use the contract; engine-specific input
and asynchronous result observation remain internal lab plumbing.

The lab descriptor rejects private browsing, page-JavaScript disabling and a full
production shell-bridge requirement. It does not advertise extensions, browsing-data
clearing or suspension. Zoom rejects nonfinite/out-of-range values rather than
silently clamping them. This lab adapter is not registered; the main browser has its
own experimental adapter, sharing this lab's native input and queue protocol code.

The adapter smoke passed on 2026-09-10 through `dyn EngineView`: real navigation and
back/forward history, reload discarding a page marker, script dispatch, 150% page
zoom and reset (measured in DOM viewport width and pixel ratio), native/rendering
bounds, and hide/show focus isolation beside WebView2. The original input/hint/bridge
checks and final shutdown also passed. The CSP fixture now preserves spaces in its
data URL and verifies an inline page script is blocked while the host bridge works.
Eight Rust library tests pass, including unsupported-request and zoom validation.

## Qualification still required

- The user verified both panes rendering, clicks, anchor navigation, resizing and
  repeated startup/shutdown. WebView2 input behaved normally. The first Servo test
  exposed missing native child focus, unscaled wheel deltas, absent context menus
  and a cursor disappearing while WebView2 received typing.
- The adapter now assigns native child focus on clicks, converts wheel notches to
  physical pixels using Windows scroll settings and DPI, handles Servo cursor
  updates only over its surface, and supplies native context menus. It also avoids
  inserting ordinary characters twice: Tao's Windows keyboard and text-commit
  events can describe the same character. New-view menu
  actions are disabled until the lab has a new-view host. Retest typing, modifiers,
  accents, menu actions and the mixed-focus cursor case interactively.
- Drag-selecting ordinary page text is an upstream Servo 0.5 limitation: its
  [release notes](https://servo.org/blog/2026/08/31/july-in-servo/) describe visible
  DOM selections with interactive selection still forthcoming. Native focus fixes
  do not implement that engine feature.
- Full IME composition/preedit: Tao exposes committed text only; the lab forwards
  that text as a composition but cannot expose the composition UI correctly yet.
- Page-to-shell messages now work through the experimental transport above. Qualify
  subframes, sustained event traffic, real-site latency and the remaining production
  message actions before declaring a complete browser adapter.
- Full keyboard mapping, modifier state, clipboard, dialogs,
  downloads, popups, beforeunload, storage/private isolation and last-view shutdown
  all need qualification before registering an adapter.
- Multiple Servo views sharing a runtime, view-scoped native callbacks, replacing
  a view while replies are pending, and close/reopen lifecycle behavior are the next
  adapter qualification step. A single-view trait implementation does not establish
  those guarantees.
- Verify adjacent Servo/WebView2 panes in the actual browser, with Normal/Insert/
  Passthrough modes, hints and focus recovery. The standalone lab is the first step.

The implementation follows the APIs in the pinned
[Servo 0.5 minimal embedder](https://github.com/servo/servo/blob/v0.5.0/components/servo/examples/winit_minimal.rs),
[WebView delegate](https://github.com/servo/servo/blob/v0.5.0/components/servo/webview_delegate.rs),
and [user content manager](https://github.com/servo/servo/blob/v0.5.0/components/servo/user_content_manager.rs).
