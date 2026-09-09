# Engine integration progress

The first two stages are implemented: the WebView2 boundary, followed by provider
selection, view identity, storage requirements, callback routing and persistence.
WebView2 is still the only installed provider. The research and remaining roadmap
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
- Uninstalled names such as `servo` and `gecko` return an error without replacing a view.

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

- `cargo test -p browser-desktop -p browser-engine --locked --offline`: 115 desktop tests
  and four contract tests passed; the opt-in native-runtime test is excluded by default.
- `cargo test -p browser-desktop --locked --offline runtime_storage_mode_is_verified_before_user_content -- --ignored --test-threads=1`:
  the native-runtime test passed separately. It creates hidden ordinary/private views
  in an isolated directory under `target/engine-tests`, verifies each mode, and rejects
  both mismatches. These temporary profiles are retained for diagnostics.
- `node --test crates/desktop/tests/bridge-hints.cjs`: all three regressions passed.
- `cargo build -p browser-desktop --locked --offline`: passed.
- The existing Windows manifest `maxversiontested` linker warning remains.

The user verified live browser behavior after both stages and reported that everything
works so far. Future adapter changes should repeat interactive checks: normal/no-JS/research/private pages, hints and copying,
split focus, background loads, history, zoom/resize, extensions, freeze/unfreeze,
profile switching and closing the last web tab. Test data clearing only with an
isolated WebView2 data directory: the shell's `:profile` stores workspace layouts and
does not isolate browser storage.

Next is a pinned Servo experiment alongside WebView2, with its own rendering surface
and provider-owned storage. The registry currently contains compiled-in metadata; it
is not a downloadable-provider installer or stable plugin ABI. After proving a second
engine, extract provider runtime/storage lifecycles, add the executable transport and
negotiate protocol versions. Gecko remains a separate feasibility experiment.
