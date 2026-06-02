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

**Modes (qutebrowser-style):**
- **Normal** — shell has focus; `:`/`o` open the command bar; `j`/`k`/`Space`/`d`/`u`
  scroll; `f` hint mode; `n`/`p` switch tabs; `1`–`9` jump to a tab; `<`/`>` reorder the
  current tab; `x` close tab; `H`/`L` history. A native tab bar shows all open tabs.
- **Hint** — `f` labels every clickable element (qutebrowser-style); type the label to
  follow it, `Esc` cancels. Injected JS draws the badges and clicks the target, while the
  shell keeps the keyboard — so it works on CSP-strict sites (needs JS, so not on `:nojs` tabs).
  A hint on a text field / search box focuses it and drops into Insert so you can type
  (Esc or click-away to leave).
- **Command** — `:open <url>`, `:read <url>` (reader mode), `:close`, `:tabnext`/`:tabprev`,
  `:reload`, `:quit`, `:nojs` (toggle JS-off for new tabs) / `:nojs <url>`, `:f` (toggle
  fullscreen), `:resize` and `:move` (window-control modes — then `hjkl`, `Esc` to finish).
- **Read mode** — `:read <url>` extracts the article with the `dom_smoothie` readability
  pipeline and renders just that (clean dark stylesheet, `<base>` for relative links) in a
  WebView2 tab. Leanness comes from the **stripped article DOM** (no ads/trackers/page scripts —
  readability removes them), not from disabling the engine: JS stays on so scrolling, hint mode,
  and focus handling work normally. Works on docs/wikis/news where a from-scratch engine
  struggles. Read tabs are tinted **green** in the tab bar and show `[read]` in the status line.
  (Servo remains a possible future drop-in behind the same `:read` command once it matures.)
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
