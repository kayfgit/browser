# Architecture

A map of the code for people who want to change it. It doesn't cover everything, and
it isn't a user guide (that's [docs/user-guide.md](docs/user-guide.md)); it's the
tour you'd get from someone who knows the codebase: what the pieces are, what to read
first, and where to start for common changes. Building and testing are in
[CONTRIBUTING.md](CONTRIBUTING.md).

A test (`crates/desktop/tests/architecture_doc.rs`) checks that every file linked
here exists and that every module in `crates/desktop/src` appears in the code map, so
this page fails CI instead of going stale.

## The big picture

**browser** is a *shell*, not an engine. It owns one window and paints its own chrome
(tab bar, command bar, status line) into a pixel buffer. Web pages are WebView2 child
windows laid over the content area, created when you open a page and dropped when you
close it; an idle browser runs no engine at all. Terminals, `:read` pages, `:ai` chats
and the internal pages are drawn by the shell itself, with no engine.

Everything the program knows lives in one struct, `App`. Everything that happens
arrives as an event on one thread, the [tao](https://docs.rs/tao) event loop:

```
 window events ──┐                              ┌─> App::draw ──> pixels
 (keys, mouse,   │                              │   (chrome.rs)
  resize, focus) │                              │
 timer wake-ups ─┼─> events.rs ──> App methods ─┤
                 │   (dispatch)    (change      └─> engine calls
 UserEvents ─────┘                  state)          (navigate, show/hide,
 (pages, threads)                                    run a script)
```

Background work (fetching a `:read` page, a Groq reply, terminal output from the pty
host) runs on other threads and reports back by sending a `UserEvent` through
`App.proxy`. Nothing else touches `App` from another thread.

## Where to start reading

Read these in order. Each stop assumes the ones before it; after the last one,
everything else in the code map is a feature you can read on its own.

1. [crates/desktop/src/main.rs](crates/desktop/src/main.rs): startup. It builds the
   window and the `App`, restores the session, then hands every event to
   `events::handle`. The module comment summarizes the modes.
2. [crates/desktop/src/app.rs](crates/desktop/src/app.rs): the `App` struct (skim the
   field comments), `UserEvent` (every message the rest of the program can send the
   shell), and `ModeKind` (Normal, Command, Insert, Passthrough, Hint, Scroll, the
   pane modes, …). Most behavior depends on the current mode.
3. [crates/desktop/src/events.rs](crates/desktop/src/events.rs): the event loop.
   `handle` splits events into `on_window_event`, `on_user_event` and `on_timer`;
   `schedule_wakeup` decides when the loop next needs to wake.
4. [crates/desktop/src/keys.rs](crates/desktop/src/keys.rs): `App::handle_key`
   dispatches on the mode; `key_normal` is where most single-key bindings live, and
   `key_command` drives the command bar.
5. [crates/desktop/src/commands.rs](crates/desktop/src/commands.rs): `App::run_command`,
   one `match` arm per `:command`. Then
   [crates/desktop/src/actions.rs](crates/desktop/src/actions.rs): the action registry
   (`ACTIONS` and `App::perform`), shared by the command bar and the `:ai` assistant.
6. [crates/desktop/src/chrome.rs](crates/desktop/src/chrome.rs): `App::draw`, which
   paints the tab bar, the panes the shell draws itself, and the command bar.
7. [crates/desktop/src/tabs.rs](crates/desktop/src/tabs.rs): `Tab` and `TabContent`
   (web, read, pager, terminal, `:ai`, …) and how each kind is opened, followed by
   [crates/desktop/src/engines/](crates/desktop/src/engines/mod.rs), the only code
   that talks to WebView2.

## How things flow

**A key while the shell has focus.** `WindowEvent::KeyboardInput` →
`on_window_event` → `App::handle_key` → the mode's handler (for example `key_normal`,
or `key_command` → `run_command` on Enter). Handlers change `App` and call
`window.request_redraw()`; the next `RedrawRequested` runs `App::draw`.

**A key while a page has focus.** The page's window gets it, not ours. Keys the shell
must still see come back two ways, both ending in a `UserEvent`:
[engines/webview2/keys.rs](crates/desktop/src/engines/webview2/keys.rs) catches Esc
and Ctrl/Alt chords (iframes included) through WebView2's `AcceleratorKeyPressed`,
and `shellKey` in [scripts/bridge.js](crates/desktop/scripts/bridge.js) hands other
keys typed in Normal mode back to the shell, which replays them
([shellkeys.rs](crates/desktop/src/shellkeys.rs)).

**A message from a page.** Injected scripts ([crates/desktop/scripts/](crates/desktop/src/scripts.rs))
call `window.__post("name")`. The IPC handler in
[engines/webview2/mod.rs](crates/desktop/src/engines/webview2/mod.rs) matches the
string and sends a `UserEvent` tagged with the page's view id.
`App::route_engine_event` ([engines/events.rs](crates/desktop/src/engines/events.rs))
drops events from closed views and handles a few itself for the tab that sent them
(URL changes, pop-ups). Notifications (blocked redirects, pop-ups and downloads) go
on to `on_user_event`. Anything else gets there only if it's listed in
`active_ui_event` **and** comes from the active tab, so a background page can't
change the mode, focus or clipboard.

**Opening a page.** `:open` → `open_tab_private` → `open_web_provider`
([engines/shell.rs](crates/desktop/src/engines/shell.rs)) → `build_provider_view` →
`engines::build`, which creates the WebView2 view with the injected scripts and the
navigation, download and key handlers. `place_tab_escaping_split` then puts the tab
in the tab strip.

## Code map

### The workspace

| Crate | What it is |
|---|---|
| [crates/desktop](crates/desktop/Cargo.toml) | The browser (package `browser`). Almost all work happens here. |
| [crates/engine](crates/engine/src/lib.rs) | The contract between the shell and a page engine: `EngineView`, providers, view ids. No WebView2 code. |
| [crates/core](crates/core/src/lib.rs) | Shared, engine-free logic: the `Document` model, config types, bangs, maths, search routing. |
| [crates/backend-text](crates/backend-text/src/lib.rs) | Fetches a page and extracts readable text for `:read`. |
| [crates/backend-search](crates/backend-search/src/lib.rs) | DuckDuckGo-lite / SearXNG results for `:read <query>`. |
| [crates/tui](crates/tui/src/lib.rs), [crates/cli](crates/cli/src/main.rs) | The original terminal-only reader. Legacy, not shipped. |

Also: [docs/](docs/user-guide.md) (user guide and engine design notes) and
[experiments/servo](experiments/servo/README.md) (the Servo qualification lab).

### crates/desktop

Besides `src/`, the package holds [scripts/](crates/desktop/scripts/bridge.js) (the
JavaScript injected into pages), [assets/](crates/desktop/assets/easylist.txt) (filter
lists compiled into the executable), [extensions/](crates/desktop/extensions/uBOLite.chromium)
(the bundled uBlock Origin Lite, packed in by [build.rs](crates/desktop/build.rs)) and
[wix/](crates/desktop/wix/main.wxs) (the MSI installer definition).

The core, in reading order:

- [main.rs](crates/desktop/src/main.rs): startup.
- [app.rs](crates/desktop/src/app.rs): `App`, `UserEvent`, `ModeKind`; window size and zoom, focus reclaim, status and error reporting, session save/restore.
- [events.rs](crates/desktop/src/events.rs): the event loop.
- [keys.rs](crates/desktop/src/keys.rs): key dispatch per mode, autocomplete, the caret modes.
- [commands.rs](crates/desktop/src/commands.rs): `:command` parsing and dispatch, `BUILTIN_VERBS`, `COMMANDS` (autocomplete), argument completion.
- [actions.rs](crates/desktop/src/actions.rs): the action registry used by commands and the `:ai` assistant, plus aliases.
- [chrome.rs](crates/desktop/src/chrome.rs): drawing the shell.
- [draw.rs](crates/desktop/src/draw.rs): the text painter, colours and `Theme`.
- [tabs.rs](crates/desktop/src/tabs.rs): tab kinds, opening and placing tabs, ad-block switching.
- [panes.rs](crates/desktop/src/panes.rs): tmux-style splits (each tab-strip entry is a tree of panes).

State that has its own type (each module holds the rules for keeping its fields
consistent, with tests):

- [cmdline.rs](crates/desktop/src/cmdline.rs): the command bar's text, caret and selection.
- [status.rs](crates/desktop/src/status.rs): the status message and when it clears.
- [visited.rs](crates/desktop/src/visited.rs): visited-URL history with visit times.
- [adblock.rs](crates/desktop/src/adblock.rs): the ad-block mode and the flag shared with every tab.
- [config.rs](crates/desktop/src/config.rs): the persisted customization (`config.toml`).
- [session.rs](crates/desktop/src/session.rs): what's saved on `:w` and restored at launch.
- [profiles.rs](crates/desktop/src/profiles.rs): named sessions (`:profile`, `:scratch`).

Keyboard and focus:

- [shellkeys.rs](crates/desktop/src/shellkeys.rs): keys the shell needs while a page has focus, and replaying them.
- [hints.rs](crates/desktop/src/hints.rs): hint mode (`f`, `F`, `yf`, `s`).
- [find.rs](crates/desktop/src/find.rs): find in page (`/`).

Pages and engines:

- [engines/](crates/desktop/src/engines/mod.rs): adapters behind the `crates/engine` contract. [engines/webview2/](crates/desktop/src/engines/webview2/mod.rs) is the only code that owns WebView2 objects; `engines/servo/` is the experimental second engine.
- [scripts.rs](crates/desktop/src/scripts.rs): the JavaScript injected into pages (the files are in [crates/desktop/scripts/](crates/desktop/scripts/bridge.js)).
- [navguard.rs](crates/desktop/src/navguard.rs) and [blocklist.rs](crates/desktop/src/blocklist.rs): blocking ad redirects and known-bad domains.
- [extensions.rs](crates/desktop/src/extensions.rs), [bundled_extensions.rs](crates/desktop/src/bundled_extensions.rs): the extension picker and the bundled uBlock Origin Lite.
- [favicon.rs](crates/desktop/src/favicon.rs), [freeze.rs](crates/desktop/src/freeze.rs), [data.rs](crates/desktop/src/data.rs): tab icons, `:freeze`, clearing browsing data.

Tabs the shell draws itself:

- [read_view.rs](crates/desktop/src/read_view.rs): lays out a `:read` document.
- [pages.rs](crates/desktop/src/pages.rs): internal pages (`:errors`, `:res`, `:history`, …) and the `:commands` help (`CMD_ROWS`).
- [vim.rs](crates/desktop/src/vim.rs): the read-only vim pager those pages use.
- [term.rs](crates/desktop/src/term.rs), [pty_term.rs](crates/desktop/src/pty_term.rs), [proc_cwd.rs](crates/desktop/src/proc_cwd.rs): terminal tabs. The shell runs in a separate process, [bin/browser-pty-host.rs](crates/desktop/src/bin/browser-pty-host.rs).
- [schemes.rs](crates/desktop/src/schemes.rs): installing terminal colour schemes.
- [ai.rs](crates/desktop/src/ai.rs), [markdown.rs](crates/desktop/src/markdown.rs): the `:ai` tab and rendering its answers.
- [news.rs](crates/desktop/src/news.rs): `:news`, from the changelog.
- [bookmarks.rs](crates/desktop/src/bookmarks.rs): `:save` / `:saved`.
- [procmon.rs](crates/desktop/src/procmon.rs): the resource numbers behind `:res`.

## Rules

Things that look changeable but aren't:

- **Every WebView2 environment must use identical options.** All webviews share one
  user-data folder, and WebView2 refuses (`0x8007139F`) to create a second environment
  with different arguments. Any new webview must use `BROWSER_ARGS`.
- **Only the user can run shell commands.** `:te` must never become reachable from
  page content (IPC, injected scripts) or from the `:ai` assistant.
- **Don't monkey-patch `JSON.parse` or `Response.prototype.json` in page scripts.**
  YouTube's integrity checks detect it and refuse to play. Use `Object.defineProperty`
  on the specific properties instead.
- **Page-to-shell messages go through `window.__post`**, never `window.ipc` directly:
  wry aborts the whole process when a frame whose URL isn't http(s) posts.
- **Match exact IPC strings before single-letter prefixes** in IPC handlers (`"ready"`
  starts with `r`).
- **Page events must be handled in `route_engine_event` or listed in
  `active_ui_event`** (`engines/events.rs`); anything else is silently dropped.
- **Don't use a low-level keyboard hook (`WH_KEYBOARD_LL`).** Once WebView2 runs in the
  process, Windows stops calling the process's hooks. Keys the shell needs while a page
  has focus go through `shellkeys.rs` (see "How things flow").
- **Keep the shell's own UI text within Consolas.** A symbol it lacks (⇧, ⚠, ⁝, ↗, …)
  makes the painter load a fallback font, and fontdue holds every glyph of it in memory:
  Segoe UI Symbol costs about 45 MB. Web pages rendered by WebView2 aren't affected.
- **Any draw path must tolerate a window smaller than the chrome**: a minimized
  window can be shorter than the tab and command bars.
- **Keep related fields in one type.** If two `App` fields must change together, give
  them a small type with the rule in its methods (like `cmdline.rs` or `visited.rs`)
  rather than adding loose fields.

## Your first change

**Add a `:command`.**
1. Add a `match` arm to `App::run_command` in `commands.rs`.
2. Add the verb and its short forms to `BUILTIN_VERBS` (a test fails if they differ).
3. Add the full spelling to `COMMANDS` so autocomplete offers it; the first prefix
   match wins, so its position matters. Fixed-choice arguments go in `arg_candidates`.
4. Document it: a row in `CMD_ROWS` (`pages.rs`) puts it in `:commands` and
   `:help <verb>`; mention it in `docs/user-guide.md`.
5. If it changes state or data that the `:ai` assistant should be able to change too,
   make it an action instead: add an `ActionSpec` to `ACTIONS` and an arm to
   `App::perform` (`actions.rs`), then call `self.run_action(...)` from the command.

**Add a key in Normal mode.**
1. Add it to `key_normal` in `keys.rs`: Ctrl chords are in the `control_key()` block,
   plain keys in the character `match` further down.
2. Document it in the key tables in `commands_document` (`pages.rs`) and in
   `docs/user-guide.md`.
3. If it must also work while a page has focus, it has to be Esc or a Ctrl/Alt chord:
   add it to `shellkeys::accelerator` with a test.

**Make a page tell the shell something.**
1. In a script under `crates/desktop/scripts/`, call `window.__post("my-event")`.
2. Add a `UserEvent` variant in `app.rs`.
3. Match `"my-event"` in the IPC handler in `engines/webview2/mod.rs` and send the
   event.
4. Let it through `App::route_engine_event` in `engines/events.rs`: list it in
   `active_ui_event` (accepted from the active tab only), or handle it right there if
   it's about the tab that sent it. Otherwise it's silently dropped.
5. Handle it in `on_user_event` in `events.rs`.

**Change how something looks.** Drawing is in `App::draw` and its helpers in
`chrome.rs`; colours are constants and `Theme` in `draw.rs`. Test with a small window.

**Run your change** with `cargo run -p browser -- --scratch`, so it can't touch your
real session.
