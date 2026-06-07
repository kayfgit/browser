# browser

A modal, mode-dispatching terminal browser — *only what's needed, when needed.*

Instead of one heavyweight engine for everything, this is a fast Rust shell that
routes each request to the lightest backend that can satisfy it. Reading docs and
quick searches never load a browser engine at all, so they cost tens of MB instead
of gigabytes. Heavy interactive pages fall back to a system webview only on demand.

## Build & run

```sh
cargo build --release
./target/release/browser                       # welcome screen
./target/release/browser https://docs.rs       # open a page in text mode
./target/release/browser :s rust ownership     # search
```

### Desktop shell:

```sh
cargo run -p browser-desktop                   # welcome window, no engine
cargo run -p browser-desktop youtube.com       # open a page on startup
```

### Install (Windows, per-user)

```powershell
pwsh -File install.ps1                         # build release, install, Start Menu shortcut, add to PATH
```

### Uninstall

```powershell
pwsh -File uninstall.ps1
```

## Keys

| Key | Action |
|-----|--------|
| `j`/`k`, `Space`, `PgUp/PgDn` | scroll |
| `g` / `G` | top / bottom |
| `f` | follow a link |
| `/` | find in page |
| `H` / `L` | history back / forward |
| `r` | reload |
| `:` | command bar |
| `q` | quit |

## More

See [detailedREADME.md](detailedREADME.md) for the full feature set, the desktop
shell (`browser-desktop`), config, architecture, and roadmap.
