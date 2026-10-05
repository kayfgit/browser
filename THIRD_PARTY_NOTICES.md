# Third-party notices

browser's own source code is MIT-licensed (see [LICENSE](LICENSE)). The release
build also bundles the third-party components below, which keep their own
licenses. They are distributed unmodified (except the one Servo file noted below),
alongside browser rather than as part of it.

## uBlock Origin Lite

- Location: `crates/desktop/extensions/uBOLite.chromium/`
- Author: Raymond Hill and contributors
- License: GNU General Public License v3.0. The full text ships with the
  extension (`LICENSE.txt`).
- Source: <https://github.com/uBlockOrigin/uBOL-home>

The extension is loaded into WebView2 as an unpacked browser extension.

## Filter lists and resources

These are compiled into the executable. Only the network rules are kept; cosmetic
rules were stripped.

| File (`crates/desktop/assets/`) | Upstream | License |
|---|---|---|
| `easylist.txt` | [EasyList](https://easylist.to/) | GPLv3+ or CC BY-SA 3.0+ (dual) |
| `easyprivacy.txt` | [EasyPrivacy](https://easylist.to/) | GPLv3+ or CC BY-SA 3.0+ (dual) |
| `ublock-filters.txt` | [uBlock Origin filters (uAssets)](https://github.com/uBlockOrigin/uAssets) | GPLv3 |
| `adblock-resources.json` | uBlock Origin's redirect resources, in [adblock-rust](https://github.com/brave/adblock-rust)'s format | GPLv3 |
| `yoyo.txt` | [Peter Lowe's Ad and tracking server list](https://pgl.yoyo.org/adservers/) | No formal license. The maintainer permits combining and redistributing it; contact pgl@yoyo.org for anything beyond that. |
| `blocklist-extra.txt` | This project | MIT |

## Bangs

`crates/core/data/kagi-bangs.json` is Kagi's bang list, compiled into the executable.

- Upstream: <https://github.com/kagisearch/bangs> (release in `crates/core/data/kagi-bangs.version`)
- License: MIT, Copyright (c) 2024 Kagi Search; the full text is in
  `crates/core/data/kagi-bangs.LICENSE`.

## Rust dependencies

The executables statically link many Rust crates, each under its own license (mostly
MIT and/or Apache-2.0). `Cargo.lock` lists the exact versions, and
`cargo tree --edges normal` shows what's compiled in.

Notable ones include [wry](https://github.com/tauri-apps/wry) (Apache-2.0/MIT),
[tao](https://github.com/tauri-apps/tao) (Apache-2.0),
[alacritty_terminal](https://github.com/alacritty/alacritty) (Apache-2.0),
[adblock-rust](https://github.com/brave/adblock-rust) (MPL-2.0), and
[portable-pty](https://github.com/wezterm/wezterm) (MIT).

## Servo

- Release builds include the [Servo](https://servo.org/) web engine (`servo-engine`
  feature), version 0.6.0, with SpiderMonkey as its JavaScript engine.
- License: Mozilla Public License 2.0. SpiderMonkey (through `mozjs_sys`) is also
  MPL-2.0. Servo's own dependencies keep their licenses; `cargo tree --edges normal
  --features servo-engine -p browser` lists them.
- Source: <https://github.com/servo/servo/tree/v0.6.0>, and every crate at its locked
  version on <https://crates.io>.
- One file is modified: `rendering_context.rs` of `servo-paint-api` 0.6.0, which
  makes a new OpenGL context current before loading OpenGL on Windows. The modified
  source is in this repository at `vendor/servo-paint-api/`, with
  the change described in its `PATCH.md`.

## Runtime components not bundled

- Microsoft Edge WebView2 Runtime: installed by Windows, under Microsoft's own terms.
- System fonts are loaded from `C:\Windows\Fonts` at runtime and never redistributed.
