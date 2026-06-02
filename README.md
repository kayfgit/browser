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
  scroll the page; `n`/`p` switch tabs; `x` close tab; `H`/`L` history.
- **Command** — `:open <url>`, `:close`, `:tabnext`/`:tabprev`, `:reload`, `:quit`,
  and `:nojs` (toggle JavaScript-off for new tabs) / `:nojs <url>` (open one JS-disabled).
- **Passthrough** — `Ctrl+V` (or `i`) sends *all* keys to the page, for terminals (ttyd)
  and web apps. Leave with **Shift+Esc** — bare `Esc` passes through to the page, so vim
  etc. work. Shift+Esc is caught by an injected JS→IPC hook even while the page is focused.
  (Note: a `:nojs` tab can't run that hook; ttyd needs JS anyway, so they don't co-occur.)

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
