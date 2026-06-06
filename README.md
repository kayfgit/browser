# browser

A modal, mode-dispatching terminal browser — *only what's needed, when needed.*

Instead of one heavyweight engine for everything, this is a fast Rust shell that
routes each request to the lightest backend that can satisfy it. Reading docs and
quick searches never load a browser engine at all, so they cost tens of MB instead
of gigabytes. Heavy interactive pages fall back to a system webview *on demand*
(roadmap), so you only pay the Chromium cost when you actually need it.

## Status

MVP (Windows-first, also builds on Linux):

| Mode | What it does | State |
|------|--------------|-------|
| **text** | Fetch → readability extraction → reader view in the terminal, with numbered, followable links | ✅ working |
| **search** | DuckDuckGo-lite or SearXNG → numbered results | ✅ working |
| **video** | `yt-dlp` + `mpv` to watch e.g. YouTube in ~150 MB | 🛣 roadmap |
| **full** | On-demand system WebView2 for JS-heavy/DRM sites | 🛣 roadmap |

## Build & run

```sh
cargo build --release
./target/release/browser                       # welcome screen
./target/release/browser https://docs.rs       # open a page in text mode
./target/release/browser :s rust ownership      # search
./target/release/browser some unprefixed words  # auto-routed (words → search)
./target/release/browser --config config/config.toml https://en.wikipedia.org/wiki/Rust_(programming_language)
```

Headless render (no terminal needed; good for testing/piping):

```sh
./target/release/browser --dump https://example.com
```

## Keys (in the TUI)

| Key | Action | Key | Action |
|-----|--------|-----|--------|
| `j`/`k`, `Space`, `PgUp/PgDn` | scroll | `f` | follow a link by number |
| `g` / `G` | top / bottom | `/` | find in page |
| `H` / `L` | history back / forward | `r` | reload |
| `:` | command bar | `q` | quit |

Command bar verbs: `:text`/`:t`, `:search`/`:s`, `:video`/`:v`, `:open`/`:o`,
`:reload`/`:r`, `:back`, `:forward`, `:quit`/`:q`.

## Config

Copy [`config/config.toml`](config/config.toml) to your platform config dir
(`%APPDATA%\browser\config.toml` on Windows) and edit. You can set the default
mode, per-host routing rules (exact host or `*.wildcard`), the search provider,
and key bindings. A missing config falls back to sane defaults.

## Desktop shell (`browser-desktop`)

A separate, keyboard-driven GUI frontend that boots a **WebView2 (Chromium)** engine
*only when you open a page*. The window chrome (welcome screen + command bar) is drawn
natively with a pixel buffer, so an **idle shell holds no engine (~30 MB)**; opening a
tab spawns the engine on demand and closing it frees the renderer.

```sh
cargo run -p browser-desktop                 # welcome window, no engine
cargo run -p browser-desktop youtube.com     # open a page on startup
```

Verified on Windows 11: idle ≈ 30 MB with zero WebView2 processes; one tab adds the
WebView2 process set; quitting (or even a crash) returns to baseline — no orphans.

### Install (Windows, per-user)

```powershell
pwsh -File install.ps1          # build release, install, Start Menu shortcut, add to PATH
```

Installs `browser.exe` (+ its `browser-pty-host.exe` companion) to
`%LOCALAPPDATA%\Programs\browser`, adds a Start Menu shortcut named **browser**, and puts
it on your PATH (so `browser <url>` works in a new terminal). No admin required; nothing
else is needed at runtime (WebView2 ships with Windows 11; assets are baked into the exe).
Flags: `-NoBuild`, `-NoPath`, `-NoShortcut`, `-InstallDir <path>`. Remove it with
`pwsh -File uninstall.ps1`.

**Modes (qutebrowser-style):**
- **Normal** — shell has focus; `:`/`o` open the command bar; `/` find-in-page; `j`/`k`/`d`/`u`
  scroll and `g`/`G` jump to top/bottom; `f` hint mode; `n`/`p` switch tabs; `1`–`9` jump to a tab; `<`/`>` reorder the
  current tab; `x` close tab; `H`/`L` history; **`Ctrl +`/`Ctrl -`/`Ctrl 0`** zoom the
  whole UI (native chrome + web pages + terminal) in / out / reset. A native tab bar shows
  all open tabs.
- **Hint** — `f` labels every clickable element (qutebrowser-style); type the label to
  follow it, `Esc` cancels. Injected JS draws the badges and clicks the target, while the
  shell keeps the keyboard — so it works on CSP-strict sites (needs JS, so not on `:nojs` tabs).
  A hint on a text field / search box focuses it and drops into Insert so you can type
  (Esc or click-away to leave).
- **Command** — `:open <url>`, `:research <url|query>`/`:rs` (lighter browse), `:edit`/`:e` (edit
  the current URL), `:read <url>` (text-only reader),
  `:te <command>` (run a local command), `:close`, `:tabnext`/`:tabprev`, `:reload`, `:quit`,
  `:nojs` (toggle JS-off for new tabs) / `:nojs <url>`, `:f` (toggle fullscreen), `:resize` and
  `:move` (window-control modes — then `hjkl`, `Esc` to finish), `:search [template]` (show/set the
  search engine; `%s` = query), `:commands` (full keybind/command reference), `:version` (build
  info), `:y`/`:yank` (copy the current URL to the clipboard), `:error`/`:errors` (review failures —
  see below). `:open <text>` that isn't a URL
  (e.g. `:open rust ownership`, or a bare word like `rustlang`) goes to the search engine — Google
  by default, changeable with `:search`. `:edit`/`:e` re-opens in the tab's own mode (a `:research`
  tab edits back to `:research`, not `:open`). The command bar is a full single-line editor with a
  blinking caret: `Left`/`Right` (and `Ctrl+`+arrows for words) move the caret, `Home`/`End` jump,
  **`Shift`+movement selects** text (and `Ctrl+A` selects all), `Ctrl+C`/`Ctrl+X`/`Ctrl+V`
  copy/cut/paste, `Backspace`/`Delete` (and the selection), `Ctrl+W` / `Ctrl`/`Alt+Backspace` delete
  a word, `Ctrl+Delete` the next word, `Ctrl+U` to the line start, `Esc`/`Ctrl+C` cancel.
- **Bangs** — a `!key` token in any `:open`/`:research` target (or typed straight: `:!yt cats`)
  redirects to that site's search, DuckDuckGo-style: `!yt lofi` → YouTube, `!osrs dragon` → the Old
  School RuneScape Wiki, `!w`/`!gh`/`!so`/`!cr`/`!dr`/`!mdn`/`!ddg`/`!g`/… (see `:commands`). A bang
  with no query opens the site's home; the bang can also trail the query (`dragon scimitar !osrs`).
- **Quick maths** — type an arithmetic expression in the command bar (`+ - * / % ^`, parentheses)
  and the result shows live, right-aligned (`:20*8` → `= 160`). Press `Enter` to replace the line
  with the result so you can copy it or keep calculating (`160` → `160+10`).
- **`:res` (resource readout)** — a **live** monitor of the browser's real footprint across its
  **whole process tree** (the shell + every WebView2 engine process + any terminal `pty-host`s): a
  grand total + per-process **memory, CPU%, and disk I/O**, sorted by memory, in an engine-free pager
  that auto-refreshes ~1×/sec. It **freezes automatically while you're selecting text** (visual mode),
  so you can highlight and yank pids/figures with vim motions without the rows shifting under you; the
  refresh resumes once the selection clears. Task Manager scatters the WebView2 engine
  processes under their own "WebView2 Manager" group (the Edge runtime gives its broker a separate app
  identity), so this is the one place you see the true total.
- **Session restore** — on quit the open tabs (web/`:nojs`/`:research`/`:read` and terminals), the
  window position + size, zoom, JS-off, and search-engine settings are saved to `session.toml` in the
  data dir; the next launch with no CLI argument reopens them exactly. Passing a URL/command on the
  command line skips restore for that run.
- **`:te <command>` (command runner)** — runs a local shell command on a background thread;
  the result replaces the command-bar text (vim-style). **Strictly shell-initiated** — never
  reachable from page content.
- **`:te` (embedded terminal)** — opens a real terminal tab: **xterm.js** in a webview bridged
  to your shell (`:shell <program>`, default `nu`). The PTY + shell run in a separate
  `browser-pty-host` companion process — so a live ConPTY can't deadlock the browser's exit —
  confined to a kill-on-close **job object** so the OS reaps it (and its conhost + shell) when
  the tab closes, the browser quits, or even crashes. Terminal tabs are tinted orange; type
  freely, `Shift+Esc` returns to the shell.
- **Read mode** — `:read <url>` (or `:read <query>`, which reads the search-results page) extracts the article with the `dom_smoothie` readability pipeline
  and renders it **engine-free**: the cleaned `Document` is painted by the shell's own softbuffer
  text renderer, so a read tab spawns **zero WebView2 processes** (verified: 0 child engine procs vs.
  a normal tab's set). This is the leanest tier by far — a few MB instead of a Chromium process group
  — ideal on low-RAM machines. `j`/`k`/`d`/`u` scroll the laid-out text and **`f` hint mode** labels
  the links natively (home-row labels, same as web hint mode); typing a label follows it by
  re-extracting that page in place, `r` reloads. Press **`v`/`V` for caret/visual selection** — a
  cursor appears mid-view and you highlight article text with vim motions and **`y` to yank** (copy),
  `Esc` to leave. Headings, code, lists, quotes and links are styled;
  images/media are simply absent. Best for docs/wikis/news. Read tabs are tinted **green** and show
  `[read]` in the status line. (No back/forward yet on read tabs; Servo remains a possible future
  richer drop-in behind the same `:read`.)
- **Research mode** — `:research <url|query>` / `:rs` is the middle tier between `:read` and a full
  `:open`: a normal page with **JavaScript on and images kept** (so SPAs and visual lookups work),
  but an injected pruner removes the heavy/noisy stuff — `<video>`, `<audio>`, `<iframe>`/`<embed>`/
  `<object>` (players, ad and social embeds) — on load and as the page mutates. Like `:open`, a
  non-URL argument (e.g. `:rs best rust http client`) goes to the search engine. For the genuinely
  "how do I…/what's the best…" Google browsing where you want pictures but not a video player and a
  dozen ad frames. Research tabs are tinted **cyan** and show `[research]`. (Page scripts still run,
  so this prunes the DOM rather than blocking requests — true network-level ad/tracker blocking
  would need dropping to the raw WebView2 COM API, a possible later upgrade.)
- **Errors** — failures (a `:open` that the WebView2 engine rejects, a read that won't extract,
  an unknown command, a terminal that won't start) are shown **in red** in the status bar and kept
  in a per-session log, each tagged with the **command that raised it and a timestamp**. Because the
  status bar is one truncated line, `:error` (or `:err`) opens the most recent failure — and
  `:errors` (`:errs`) every failure this session, newest first — in an **engine-free, read-only vim
  pager** (no WebView2), oldest error first. It's a real vim buffer you can't edit but *can* navigate
  and copy from: `hjkl`/arrows, `w`/`b`/`e`, `0`/`^`/`$`, **`f`/`t` (and `F`/`T`, `;`/`,`) to jump to a
  char inline**, `gg`/`G`, `Ctrl+D`/`Ctrl+U`; **`v`/`V` to visually select** then **`y` to yank**; and
  operator motions/text objects — so to lift a code out of `WindowsError(HRESULT(0x8007139f))` you put
  the cursor inside and press **`yi(`** (or `yiw`, or `yf)`). A block cursor shows your position; the
  tab is tinted red and shows `[error]`. (`1`–`9` still switch tabs; `n`/`p` are inert here.)
- **Find in page** — `/` opens a search that highlights **every** match as you type and jumps to
  the first; **`n`/`N`** step forward/back through matches and **`Esc`** clears. It works in every
  tab type: web pages (via the CSS Custom Highlight API — no DOM mutation, so it can't break the
  page), engine-free **read** tabs, and the **error** pager. Matching is case-insensitive; the status
  bar shows the query and (on native tabs) a `cur/total` counter.
- **No auto-translate** — Edge's "translate this page?" bar is disabled, and any navigation that gets
  routed through Google's `*.translate.goog` proxy (which mangles the URL) is intercepted and
  redirected to the original site, so you always see the real page at its real address.
- **Resize / Move** — entered by `:resize` / `:move`; `hjkl` size or reposition the
  window, `Esc` exits. The window is **borderless** (no OS title bar) — all window
  control is command-driven.
- **Insert** — `i` (or a hint on a text field) to type into a field. The shell still honors
  **Esc** (leave) and **Ctrl+V** (→ passthrough); it auto-exits when focus leaves the field
  (you click away). Temporary, for filling in inputs.
- **Passthrough** — `Ctrl+V` sends *every* keystroke to the page with no exceptions and
  **persists across clicks and navigation**; the only way out is **Shift+Esc**. For ttyd and
  full web apps. (Both Insert and Passthrough use an injected JS bridge, so they need
  JavaScript — not available on `:nojs` tabs.)

Roadmap: image-block for a leaner read mode, a native YouTube mode (thumbnails via
Piped/Invidious + `mpv`), and Servo for the read tier.

## Architecture

```
core            config, intent routing, the Document model, the Backend trait
backend-text    reqwest + dom_smoothie (readability) + DOM→Document walker
backend-search  reqwest + DDG-lite / SearXNG → Document
tui             ratatui app: command bar, viewport, history, find-in-page
cli             the `browser` (terminal) binary
desktop         tao + wry (WebView2) + softbuffer/fontdue native chrome; `browser-desktop`
pty-host        `browser-pty-host`: companion process owning a PTY+shell, bridged over a pipe
```

Every backend produces a `Document`; the TUI only knows how to render a
`Document`. Adding a mode = a new `Backend` impl + a route arm — that's the
customization seam.

## Known limitations

- Readability is imperfect on SPA/JS-rendered pages; route those to `full` mode
  (once it lands) via a config rule.
- Inline punctuation immediately after a link may get an extra space (wrapping
  treats words as space-separated units).
- `full`/`video` modes are not implemented yet (return a clear roadmap message).
