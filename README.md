# browser (im bad with names)

A keyboard-driven, modal browser for Windows that only runs the heavy machinery
when a page actually needs it.

https://github.com/user-attachments/assets/61babbe2-3704-4593-adb3-fc9c70efba8a

Most browsers keep a full Chromium engine running for everything. Here the window
itself is a small native Rust program, and each tab runs on the lightest backend
that works for it:

| Tab type | What runs | Cost |
|---|---|---|
| **Web** (`:open`) | Microsoft Edge WebView2 (Chromium), started when the first page opens | a normal browser tab |
| **Research** (`:research`) | WebView2 with video, audio and embeds stripped out | lighter than a web tab |
| **Read** (`:read`) | Article text extracted and drawn natively, no engine at all | a few MB |
| **Terminal** (`:te`) | A native terminal (Alacritty's VT engine) with your shell | a few MB |

With no pages open, the browser sits at about 30 MB with zero engine processes.

> **Status: alpha.** It's used daily by its author, but expect rough edges. Windows 10/11
> (x64) only for now. Please [report bugs](../../issues).

## Features

- **Vim-style modes**: Normal, Insert, Passthrough, Hint, Selection (caret) and Command,
  inspired by qutebrowser.
- **Link hints**: `f` labels every clickable element; type the label to follow it.
- **Splits**: tmux-like panes inside a tab (`Ctrl+W` then `s`/`v`), and the panes can mix
  web pages, read views and terminals.
- **Built-in terminal**: `:te` opens your shell (nushell, PowerShell, cmd, WSL…) in a tab.
- **Ad blocking**: bundled uBlock Origin Lite, plus a guard against forced redirects
  and pop-unders.
- **Sessions and profiles**: `:w` saves your tabs, splits and window layout;
  `:saveprofile work` / `:profile work` switch between whole workspaces.
- **Command bar**: autocomplete from history, `!bangs` (`!yt lofi`, `!gh wry`), and
  inline maths (`:20*8` → `= 160`).
- **`:res`**: live memory, CPU and disk use for every process the browser owns.
- **Optional `:ai` assistant** (needs your own [Groq](https://groq.com/) API key) that can
  change settings, open pages and manage data for you.

## Videos

I make YouTube videos about the browser: what it can do, and me implementing new
features. Find them on [@kayfofalltrades](https://www.youtube.com/@kayfofalltrades).

## Help wanted: more engines

browser already runs pages in two engines: WebView2 (Blink, what Chrome and Edge use)
and [Servo](https://servo.org/). I'd love to add the other engines that actually render
the web differently, so web developers can check their site against all of them from one
window, without a Mac or a pile of browsers. My PC is too weak to build or test most of
these myself, so this is where help would matter most.

| Engine | Why it's worth it | How it would plug in | Effort |
|---|---|---|---|
| **WebKit** (Safari, every iPhone browser) | Catches the "broken in Safari" bugs without a Mac. It's WebKit rather than Safari: no Apple fonts, some codecs missing, no iOS touch behaviour. | Playwright's prebuilt Windows WebKit, downloaded only when installed (`:engine install webkit`), running as its own process with its window placed in our pane. | Large |
| **Gecko** (Firefox) | The other independent engine with real users, with its own layout and standards quirks. | Mozilla has no desktop embedding API, so the same way as WebKit: a separate (Playwright-patched) Firefox process, driven remotely. Building that external-engine host once covers both. | Large |
| **Trident** (Internet Explorer 11's MSHTML) | For anyone maintaining old enterprise, government or bank intranet apps. Built into every Windows and supported by Microsoft until at least 2029. For testing, not browsing. | Windows' WebBrowser control in a child window, like our other panes. No download. | Small |
| **Ladybird** | A fully independent engine written from scratch from the specs, good at exposing pages that rely on quirks. | Once it has a Windows build: its 2026 alpha is Linux and macOS only. | Later |

Forks (Edge, Brave, Opera, Arc, Waterfox, Orion, …) aren't on the list: they share an
engine with one above. If you want to help, open an issue or a draft PR. How engines plug
in is described in [ARCHITECTURE.md](ARCHITECTURE.md) (the `crates/engine` contract) and
[docs/engine-integration.md](docs/engine-integration.md), which covers how Servo was added.

## Install

> [!WARNING]
> **Releases might be broken.** My PC is very bad, so I usually don't test a release on
> it before publishing. I release it and install the update like everybody else. If
> something is broken, [open an issue](../../issues) and I'll fix it asap.

Download `browser-x86_64-pc-windows-msvc.msi` from the
[latest release](../../releases/latest) and run it. It installs just for you, without
admin rights, into `%LOCALAPPDATA%\Programs\browser`, adds a Start Menu shortcut and a
`browser` command, and from then on the browser updates itself (see `:update`).

You need the [Microsoft Edge WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/).
Windows 11 ships with it; on Windows 10 install the Evergreen runtime if pages don't load.

The executables aren't code-signed yet, so Windows SmartScreen may warn on first run
("More info" → "Run anyway").

## Getting started

Launch **browser** from the Start Menu, or run `browser <url>` in a terminal.

| Key | Action |
|---|---|
| `:` | command bar (`:open github.com`, `:read <url>`, `:te`, …) |
| `o` / `O` | open a page here / in a new tab |
| `j` `k` / `Ctrl+D` `Ctrl+U` | scroll / half page |
| `f` / `F` | follow a link / open it in a new tab |
| `/` then `n` `N` | find in page |
| `H` / `L` | back / forward |
| `n` / `p`, `1`–`9` | next / previous tab, jump to tab |
| `x` / `u` | close tab / reopen it |
| `U` / `R` | undo / redo a layout change (close, split, pane move, resize) |
| `i` | type into a page field (`Esc` to leave) |
| `Ctrl+V` | passthrough: every key goes to the page (`Ctrl+S` or `Shift+Esc` to leave) |
| `v` | select text with vim motions, `y` to copy |
| `:w` / `:quit` | save the session / quit |

`:commands` (or `:help <topic>`) opens the full reference inside the browser. The
[user guide](docs/user-guide.md) covers every mode and feature in detail.

## Building from source

See [CONTRIBUTING.md](CONTRIBUTING.md). To find your way around the code, start with
[ARCHITECTURE.md](ARCHITECTURE.md).

## License

MIT. See [LICENSE](LICENSE). Bundled third-party components, including uBlock
Origin Lite (GPLv3) and the filter lists, keep their own licenses; see
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).
