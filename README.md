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
- **Normal** — shell has focus; `:`/`o` open the command bar; `j`/`k`/`d`/`u`
  scroll; `f` hint mode; `n`/`p` switch tabs; `1`–`9` jump to a tab; `<`/`>` reorder the
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
  info), `:y`/`:yank` (copy the current URL to the clipboard). `:open <text>` that isn't a URL
  (e.g. `:open rust ownership`, or a bare word like `rustlang`) goes to the search engine — Google
  by default, changeable with `:search`. `:edit`/`:e` re-opens in the tab's own mode (a `:research`
  tab edits back to `:research`, not `:open`). The command bar is a full single-line editor with a
  blinking caret: `Left`/`Right` (and `Ctrl+`+arrows for words) move the caret, `Home`/`End` jump,
  **`Shift`+movement selects** text (and `Ctrl+A` selects all), `Ctrl+C`/`Ctrl+X`/`Ctrl+V`
  copy/cut/paste, `Backspace`/`Delete` (and the selection), `Ctrl+W` / `Ctrl`/`Alt+Backspace` delete
  a word, `Ctrl+Delete` the next word, `Ctrl+U` to the line start, `Esc`/`Ctrl+C` cancel.
- **`:te <command>` (command runner)** — runs a local shell command on a background thread;
  the result replaces the command-bar text (vim-style). **Strictly shell-initiated** — never
  reachable from page content.
- **`:te` (embedded terminal)** — opens a real terminal tab: **xterm.js** in a webview bridged
  to your shell (`:shell <program>`, default `nu`). The PTY + shell run in a separate
  `browser-pty-host` companion process — so a live ConPTY can't deadlock the browser's exit —
  confined to a kill-on-close **job object** so the OS reaps it (and its conhost + shell) when
  the tab closes, the browser quits, or even crashes. Terminal tabs are tinted orange; type
  freely, `Shift+Esc` returns to the shell.
- **Read mode** — `:read <url>` extracts the article with the `dom_smoothie` readability
  pipeline and renders **just the text** (clean dark stylesheet, `<base>` for relative links) in a
  WebView2 tab. It's deliberately the leanest tier: readability strips ads/trackers/page scripts,
  and a strict **Content-Security-Policy** on the generated document blocks all images, media, web
  fonts and scripts from loading at all — so a read tab fetches almost nothing. The only JS that
  runs is the shell's own host-injected scroll/focus bridge (exempt from the page CSP, like hint
  mode), so navigation keys still work with zero page scripts. Best for docs/wikis/news. Read tabs
  are tinted **green** and show `[read]` in the status line. (Servo remains a possible future
  drop-in behind the same `:read` command once it matures.)
- **Research mode** — `:research <url|query>` / `:rs` is the middle tier between `:read` and a full
  `:open`: a normal page with **JavaScript on and images kept** (so SPAs and visual lookups work),
  but an injected pruner removes the heavy/noisy stuff — `<video>`, `<audio>`, `<iframe>`/`<embed>`/
  `<object>` (players, ad and social embeds) — on load and as the page mutates. Like `:open`, a
  non-URL argument (e.g. `:rs best rust http client`) goes to the search engine. For the genuinely
  "how do I…/what's the best…" Google browsing where you want pictures but not a video player and a
  dozen ad frames. Research tabs are tinted **cyan** and show `[research]`. (Page scripts still run,
  so this prunes the DOM rather than blocking requests — true network-level ad/tracker blocking
  would need dropping to the raw WebView2 COM API, a possible later upgrade.)
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
