# Contributing

## Building

You need Windows 10/11 x64, a stable Rust toolchain (`rustup` with the MSVC target) and
the WebView2 runtime (preinstalled on Windows 11).

```powershell
cargo build --release -p browser-desktop -p browser-pty-host
.\target\release\browser-desktop.exe
```

`browser-pty-host.exe` must sit next to the main executable: terminals run in that
companion process.

Launch development builds with `--scratch` so they can't touch your real session or
profiles:

```powershell
cargo run -p browser-desktop -- --scratch
cargo run -p browser-desktop -- --scratch https://example.com
```

`--scratch` sends every session write to a throwaway file. Typing `:scratch` after
launch is *not* the same thing: that parks the current tabs into your real session first.

Useful environment variables:

| Variable | Effect |
|---|---|
| `BROWSER_WEBVIEW2_DATA_DIR` | use an isolated WebView2 profile directory |
| `BROWSER_TEST_QUIT_MS` | quit after N ms without saving (for headless start/stop checks) |
| `BROWSER_YT_DEBUG=1` | log YouTube page-lifecycle probes to the console |

## Checks

CI runs these on every push and pull request; run them before pushing:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## Commit messages

Releases are automated with [release-please](https://github.com/googleapis/release-please),
which reads commit messages on `main` to decide the next version and write the changelog.
Use [Conventional Commits](https://www.conventionalcommits.org/):

| Prefix | Meaning | Version bump (while < 1.0) |
|---|---|---|
| `feat:` | new feature | minor (0.2.0 → 0.3.0) |
| `fix:` | bug fix | patch (0.2.0 → 0.2.1) |
| `perf:` | performance improvement | patch |
| `docs:`, `refactor:`, `style:`, `test:`, `chore:`, `ci:`, `build:` | no user-visible change | none |
| `feat!:` or a `BREAKING CHANGE:` footer | breaking change | minor |

Commits without a recognised prefix still land, but they don't appear in the changelog
or trigger a release. Squash-merging a pull request uses the PR title as the commit
message, so give PRs conventional titles.

## Releasing

1. Merge work into `main` with conventional commit messages.
2. release-please keeps a **release pull request** open that bumps the version in
   `Cargo.toml` and updates `CHANGELOG.md`.
3. Merging that pull request tags the version, creates the GitHub Release, and builds
   and uploads the installers ([cargo-dist](https://opensource.axo.dev/cargo-dist/)).

Nobody edits the version by hand.

## Layout

```
crates/
  desktop/        the browser (tao + wry/WebView2, softbuffer/fontdue native UI)
    src/engines/  engine adapters behind the browser-engine contract (WebView2, Servo)
    scripts/      JavaScript injected into pages (bridge, hints, selection, scroll)
    assets/       filter lists compiled into the executable
    extensions/   bundled uBlock Origin Lite
  pty-host/       companion process that owns the ConPTY and shell for terminal tabs
  engine/         engine-agnostic view/provider contract
  core/           shared Document model, config, bangs, maths, search routing
  backend-text/   readability extraction for :read (reqwest + dom_smoothie)
  backend-search/ DuckDuckGo-lite / SearXNG results for :read <query>
  tui/, cli/      the original terminal-only reader (legacy, not in releases)
docs/             user guide and engine design notes
experiments/servo standalone Servo/WebView2 qualification lab
```

Things worth knowing before changing them:

- **Every WebView2 environment must use identical options.** All webviews share one
  user-data folder, and WebView2 refuses (`0x8007139F`) to create a second environment
  with different arguments. Any new webview must use `BROWSER_ARGS`.
- **Only the user can run shell commands.** `:te` must never become reachable from
  page content (IPC, injected scripts).
- **Don't monkey-patch `JSON.parse` or `Response.prototype.json` in page scripts.**
  YouTube's integrity checks detect it and refuse to play. Use `Object.defineProperty`
  on the specific properties instead.
- **Match exact IPC strings before single-letter prefixes** in IPC handlers (`"ready"`
  starts with `r`).
- **Any draw path must tolerate a window smaller than the chrome**: a minimized
  window can be shorter than the tab and command bars.

## The Servo engine (experimental)

The `servo-engine` feature adds Servo as a second engine, selectable per pane with
`:engine servo`. It needs the native toolchain from the
[Servo lab](experiments/servo/README.md) and isn't part of release builds.

```powershell
./run-servo.ps1 -UseLocalLinker                 # run the browser with Servo available
./run-servo.ps1 -UseLocalLinker -Scratch        # same, in a throwaway session
./run-servo.ps1 -Action Smoke -UseLocalLinker   # isolated integration check
```

`install.ps1` installs a local build for your own use (`-WebView2Only` for an optimized
release build without Servo). End users should use the installers from GitHub Releases.
See [docs/engine-integration.md](docs/engine-integration.md) for the engine design.
