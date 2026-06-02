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

## Architecture

```
core            config, intent routing, the Document model, the Backend trait
backend-text    reqwest + dom_smoothie (readability) + DOM→Document walker
backend-search  reqwest + DDG-lite / SearXNG → Document
tui             ratatui app: command bar, viewport, history, find-in-page
cli             the `browser` binary
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
