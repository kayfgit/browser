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

### Experimental Servo in the desktop shell (Windows)

The opt-in `servo-engine` feature adds Servo alongside WebView2 in the actual browser.
With the native tools from the [Servo lab](experiments/servo/README.md) installed:

```powershell
./run-servo.ps1 -UseLocalLinker                 # main browser, existing shell session
./run-servo.ps1 -UseLocalLinker -Scratch        # throwaway shell layout
./run-servo.ps1 -Action Smoke -UseLocalLinker   # isolated native integration check
```

Open an HTTP(S) page, then use `:engine servo` or `:engine webview2` to reopen the
active pane. Splits can mix engines. `:engine default servo` chooses Servo for new
web tabs; `:engines` lists capabilities. Direct Cargo builds keep WebView2 only
unless the feature is enabled; the Windows installer includes both engines by default.

Servo is experimental: storage/sign-ins are separate, private/no-JavaScript views
are rejected, and extensions/uBlock, downloads, full IME and some page controls are
not implemented. See [integration notes](docs/engine-integration.md) for details.
After its first use, the Servo runtime stays loaded until browser exit; individual
pages still close normally.

### Install (Windows, per-user)

```powershell
pwsh -File install.ps1                         # build WebView2 + Servo, install, Start Menu shortcut, add to PATH
pwsh -File install.ps1 -WebView2Only            # optional WebView2-only release build
```

Both commands update the same `browser` Start Menu shortcut and installed executable.
The default installation reuses the development build and native tools from
`run-servo.ps1`; it does not start a separate optimized Servo release build.
Use the plain command for future updates to retain both engines. The old `-Servo`
flag still works for compatibility. `-WebView2Only` replaces the installed executable
with one that has no Servo support. `-NoBuild` uses
the existing binaries for the selected variant. Close the installed browser before
updating it. Sessions and engine data are preserved.

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
