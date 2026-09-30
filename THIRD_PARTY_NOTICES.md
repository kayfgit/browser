# Third-party notices

browser's own source code is MIT-licensed (see [LICENSE](LICENSE)). The release
build also bundles the third-party components below, which keep their own
licenses. They are distributed unmodified, alongside browser rather than as part
of it.

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

The optional Servo engine (`servo-engine` feature, not part of release builds) is
MPL-2.0.

## Runtime components not bundled

- Microsoft Edge WebView2 Runtime: installed by Windows, under Microsoft's own terms.
- System fonts are loaded from `C:\Windows\Fonts` at runtime and never redistributed.
