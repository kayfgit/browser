# User guide

This covers every mode and feature in detail. For the quick version see the
[README](../README.md); inside the browser, `:commands` lists every key and command
and `:help <topic>` jumps to one (`:help theme`, `:help selection`, `:help bangs`).

- [The window](#the-window)
- [Modes](#modes)
- [Opening pages](#opening-pages)
- [Tab types](#tab-types)
- [Splits](#splits)
- [Sessions, profiles and saved pages](#sessions-profiles-and-saved-pages)
- [The terminal](#the-terminal)
- [Ad blocking and privacy](#ad-blocking-and-privacy)
- [Command bar tricks](#command-bar-tricks)
- [Resource monitor](#resource-monitor)
- [Errors](#errors)
- [The `:ai` assistant](#the-ai-assistant)
- [Updates](#updates)
- [What's new](#whats-new)
- [Engines: WebView2 and Servo](#engines-webview2-and-servo)

## The window

The window is borderless: the tab bar is on top, the command/status bar at the
bottom, and everything is driven from the keyboard. Drag the tab bar to move the
window; `:resize` and `:move` enter modes where `h`/`j`/`k`/`l` size or move it
(`Esc` to finish), and `:f` toggles fullscreen.

With no tabs open no browser engine is running at all (about 30 MB). The WebView2
engine starts with the first web tab, and quitting (or a crash) leaves no stray
processes behind.

Zoom comes in two independent kinds:

- `+` / `-` / `)` zoom the **page content** of web tabs.
- `Ctrl +` / `Ctrl -` / `Ctrl 0` zoom the **browser UI** (tab bar, command bar) and
  terminals.

## Modes

The browser is modal, like vim or qutebrowser. The mode decides where your keys go.

- **Normal**: the browser has the keyboard. `j`/`k` scroll, `h`/`l` scroll sideways,
  `Ctrl+D`/`Ctrl+U` move half a page, `gg`/`G` jump to top/bottom, `H`/`L` go
  back/forward, `r` reloads. `n`/`p` switch tabs, `1`–`9` jump to a tab
  (`Shift+1`–`9` for tabs 11–19), `<`/`>` move the current tab, `x` closes it and
  `u` (or `Ctrl+Shift+T`) reopens the last closed one. `U` undoes the last layout change
  and `R` redoes it (see [Splits](#splits)). `yy` copies the page address.
  Clicking a button, link or menu on a page lets the page keep the keyboard so its
  menu stays usable: arrow keys, Enter, Space and Tab operate it. Any other key hands
  the keyboard back to the browser and still counts, so `:` opens the command bar
  right away; `Esc` just hands it back. If the button opens a text field (a search
  icon, say), you're put in Insert mode to type there.
- **Command**: `:` opens the command bar. `o`/`O` prefill `open` (this tab) or
  `open -t` (new tab).
- **Insert**: `i`, or clicking/hinting a text field, lets you type into a page field.
  `Esc` leaves, and so does clicking away or navigating. It's meant for filling in a
  search box or a form.
- **Passthrough**: `Ctrl+V` sends *every* key to the page and stays on through clicks,
  navigation and fullscreen. It's for web apps that need the whole keyboard (web
  terminals, games, video players). `Ctrl+S` or `Shift+Esc` leaves.
- **Hint**: `f` labels every clickable element; type a label to click it. `F` (or
  typing the label in UPPERCASE) opens links in a new tab, and `yf` copies the link
  address instead. A hint on a text field focuses it and enters Insert. `Esc` cancels.
- **Scroll**: `s` labels scrollable boxes inside the page (a chat log, a code panel).
  Pick one and `h`/`j`/`k`/`l`, `gg`/`G`, `Ctrl+D`/`Ctrl+U` or `PgUp`/`PgDn` scroll only
  that box, which gets a cyan outline. `Esc` releases it.
- **Selection**: `v` or `V` puts a cursor in the middle of the view. Move it with vim
  motions (`h`/`j`/`k`/`l`, `w`/`b`/`e`, `0`/`$`, `gg`/`G`), press `v` for a character
  selection or `V` for whole lines, and `y` to copy. `Esc` leaves. It works on web tabs,
  read tabs, and inside a box selected with `s`.
- **Find**: `/` searches the page as you type and highlights every match; `Enter` keeps
  the highlights, `n`/`N` step through them, `Esc` clears. Works on web, read and pager
  tabs.

Hints, Insert, Passthrough and web Selection mode rely on a small script injected into
the page, so they need JavaScript (not available on `:nojs` tabs).

Right-clicking a web page opens a small menu: **Copy** (the selection), and over a link
**Open in new tab** and **Copy link address** (plus **Copy image address** over an
image). On natively drawn surfaces (a terminal, a read tab, a pager, the command bar)
right-click copies the current selection.

## Opening pages

- `:open <url>` (`:o`) opens in the current tab; `:open -t <url>` or `:tabopen` (`:t`)
  opens a new tab, and `-n` opens a private (InPrivate) tab: `:open -tn <url>`.
- Text that isn't a URL goes to your search engine: `:open rust ownership`. The default
  is Google; change it with `:search ddg` (or `google`, `wiki`, … or any URL template
  with `%s` for the query).
- `:edit` (`:e`) puts the current address in the command bar so you can tweak it. It
  reopens in the tab's own mode, so editing a `:research` tab stays in `:research`.
- `:y` (`:yank`) copies the current address.
- `:nojs` toggles JavaScript off for new tabs; `:nojs <url>` opens one page without it.
- Pages opened from the command line (`browser <url>`) skip session restore for that run.

## Tab types

- **Web** (`:open`): a normal WebView2 page.
- **Research** (`:research`, `:rs`): a normal page with JavaScript and images, but
  `<video>`, `<audio>`, iframes and embeds are removed as the page loads and changes.
  Good for "what's the best…" searches where you want pictures but not a video player
  and a dozen ad frames. Captcha widgets are left alone. Tinted cyan.
- **Read** (`:read <url>`): extracts the article and draws it natively, with no engine
  and nothing but text. Headings, code, lists, quotes and links are styled; images and
  media are left out. `f` follows links (in place), `H`/`L` step through read history,
  `r` re-extracts, `v` selects text. `:read <words>` shows search results you can follow.
  Tinted green. Best for docs, wikis and news.
- **Terminal** (`:te`): see [The terminal](#the-terminal). Tinted orange.
- **Pagers** (`:error`, `:errors`, `:res`, `:version`, `:history`, `:saved`, …):
  read-only vim buffers. `h`/`j`/`k`/`l`, `w`/`b`/`e`, `0`/`^`/`$`, `f`/`t` (with `;`/`,`),
  `gg`/`G`, `v`/`V` to select and `y` to yank, including text objects like `yiw` or `yi(`.

## Splits

Splits work like tmux: a tab in the tab bar can hold several panes, and each pane can
be any tab type.

- `Ctrl+W` then `s` / `v` splits stacked / side by side (also `:split` / `:vsplit`).
- `Ctrl+W` then `h`/`j`/`k`/`l` moves focus; `H`/`J`/`K`/`L` resizes (keep tapping,
  `Esc` to finish).
- `Ctrl+W` then `c` closes the focused pane; `Ctrl+W` then `m` grabs it so `h`/`j`/`k`/`l`
  swap it with its neighbours (`Enter` keeps, `Esc` reverts). `Ctrl+W` then a tab number
  pulls that tab into the split.
- Clicking a pane focuses it. Splits are saved with the session.
- `U` undoes the last layout change and `R` redoes it (also `:undo` / `:redo`): closing a
  tab or pane, a split, moving, swapping or breaking out a pane, flipping or resizing a
  split, moving a tab. Undoing a close reopens the page in the pane it was in (a
  terminal restarts in its directory). A run of resizes counts as one change.

## Sessions, profiles and saved pages

**Sessions** are saved explicitly, vim-style: `:w` saves the open tabs (including splits
and terminal working directories), window position and size, zoom, ad blocking and search
settings. `:quit` (or `:leave`, `:l`) quits, and like closing the window it does *not*
save, so the last written session stays as it was. Vim's `:q`, `:wq` and `:x` don't
quit the browser, so a habit from a terminal can't close it. The next launch without a URL argument
restores it.

**Profiles** are named snapshots of a whole workspace:

- `:saveprofile work` (`:sp work`) saves everything that's open as `work`.
- `:profile work` (`:p work`) switches to it; `:profile` or `:profiles` opens a picker
  (`Enter` switches, `d` deletes) and `:delprofile <name>` removes one.
  `:profile default` returns to the live session.
- `:saveprofile` is the only thing that writes a profile; `:w` never touches one.
- `:scratch` parks everything open and gives you an empty browser to try something in;
  `:scratch` again brings the parked tabs back.

History, customization and `:ai` chats are shared by all profiles.

**Saved pages**: `:save` (also `:bookmark`, `:favorite`) keeps the current page for later
and `:save <name>` gives it a label. `:saved` lists them (`Enter` opens, `Shift+Enter`
opens in a new tab, `d` deletes). They're written immediately and aren't affected by
profiles or `:restore`.

## The terminal

`:te` opens a native terminal tab running your shell. It uses Alacritty's terminal
engine and needs no web engine. Change the shell with `:shell <program>` (for example
`:shell nu`, `:shell cmd`, `:shell wsl`). The default is PowerShell: `pwsh` if
PowerShell 7 is installed, otherwise Windows PowerShell. `:shell` on its own shows the
current one, and `:w` saves your choice with the session.

- `i` enters Passthrough so keys go to the shell (`Esc` included).
- `Ctrl+S` gives the keyboard back to the browser and enters copy mode: a vim cursor
  over the output and scrollback (`h`/`j`/`k`/`l`, `w`/`b`, `f` to find, `v` to select,
  `y` to yank). `i` or `Enter` returns to the live shell.
- `Ctrl+V` pastes; you can also drag to select, and double/triple-click takes a word/line.
- Typing `exit` closes the tab.
- `:te <command>` (with a command) runs it once and shows the output in the command bar.

When you save the session, each terminal's working directory is saved too, and the next
launch reopens the shell there. How the directory is detected depends on the shell:

- **cmd**: automatic.
- **nushell**: automatic. For exact results while a long-running program is open, add
  `$env.config.shell_integration.osc9_9 = true` to `config.nu`.
- **PowerShell**: `Set-Location` doesn't change the process directory, so add a prompt
  hook to `$PROFILE`:

  ```powershell
  function prompt { "PS $($executionContext.SessionState.Path.CurrentLocation)$('>' * ($nestedPromptLevel + 1)) $([char]27)]9;9;$($executionContext.SessionState.Path.CurrentLocation.ProviderPath)$([char]27)\" }
  ```

- **WSL**: the Linux directory isn't visible from Windows. Add this to the distro's
  `~/.bashrc` (for zsh, put the `printf` in a `precmd()` function) and restore re-enters
  WSL in the saved directory:

  ```bash
  __browser_cwd() { printf '\e]9;9;\\\\wsl.localhost\\%s%s\e\\' "$WSL_DISTRO_NAME" "${PWD//\//\\}"; }
  PROMPT_COMMAND=__browser_cwd
  ```

## Ad blocking and privacy

Ad blocking is on by default and has two halves that work together:

- **uBlock Origin Lite**, bundled and loaded into WebView2, blocks ad and tracker
  requests at the network level.
- **Native layers** hide leftover ad containers, strip YouTube's video ads, stop
  pop-unders, and block forced redirects to known ad/scam domains (matched against
  EasyList, EasyPrivacy, uBlock's lists and Peter Lowe's list).

`:ads` toggles blocking, `:adblock native` runs only the native layers (useful for
ruling the extension out if a site breaks), and `:extensions` lists installed extensions.

Other protections and toggles:

- Downloads of executables and installers are blocked unless you run `:downloads`.
- Edge's "translate this page?" bar is disabled, and links routed through Google's
  `translate.goog` proxy go to the real site instead.
- Favicons come from WebView2's own cache, never from a third-party favicon service.
- `:clear history|cookies|cache|all [15m|1h|24h|7d|all]` erases browsing data.
- `:mute`, `:css`, `:video` and `:scrollbar` toggle muting, page styles, video players and scrollbars live, without a reload.
- Private tabs (`:open -n`) are never saved to the session or the closed-tab list.

## Command bar tricks

- **Editing**: arrows, `Ctrl+arrows` by word, `Home`/`End`, `Shift` to select, `Ctrl+A`,
  `Ctrl+C`/`X`/`V`, `Ctrl+W` or `Ctrl+Backspace` delete a word, `Ctrl+U` deletes to the
  start. `Esc` cancels.
- **Autocomplete**: a dim suggestion completes the command name, or an address from your
  history; `Tab` or `Ctrl+Right` accepts it. On commands with fixed choices (`:adblock`,
  `:clear`, `:theme`, …) `Tab`/`Shift+Tab` cycle them.
- **Bangs**: a `!key` anywhere in a search jumps to that site's search, DuckDuckGo-style.
  `!yt lofi` searches YouTube, `dragon scimitar !osrs` the RuneScape wiki. A bang with no
  query opens the site's home page. The bangs are the 13,000+ of
  [Kagi's list](https://github.com/kagisearch/bangs) (the ones Helium uses), built in and
  offline: `:bangs` lists them all (`/` searches the list), `:bangs <word>` filters by key
  or name, `:bang <key>` says what one does. Add your own with `:bang <key> <url>`, putting
  `%s` where the search goes (`:bang rs https://runescape.wiki/?search=%s`); yours win over
  Kagi's for the same key. `:unbang <key>` removes a bang (yours, or switches off one of
  Kagi's), and `:resetbangs <key>` brings it back (`:resetbangs` alone resets them all).
  `:ai` can do all of this too ("add a !rs bang for the RuneScape wiki", "I removed a bang
  by accident").
- **Maths**: type an expression (`+ - * / % ^`, parentheses) to see the result live;
  `Enter` replaces the line with it so you can keep going.
- **Aliases**: `:alias gh open github.com` makes `:gh` open GitHub; `:unalias gh` removes it.
- **Themes**: `:theme` changes the bar colours, height and terminal font and colours;
  `:restore` (or `Ctrl+Alt+Shift+R` anywhere) resets all customization.

## Resource monitor

`:res` shows the browser's real footprint across its whole process tree (the browser,
every WebView2 process and every terminal): total and per-process memory, CPU and disk
I/O, refreshed about once a second. Selecting text pauses the refresh so rows don't move
while you copy.

Task Manager files WebView2's processes under a separate "WebView2 Manager" group, so
`:res` is the easiest place to see the true total.

## Errors

Failures show in red in the status bar and are kept for the session with the command
that caused them and a timestamp. `:error` opens the latest in a pager, `:errors` all of
them.

## The `:ai` assistant

`:ai` opens an assistant that can act on the browser for you: "open github and gmail side
by side", "wipe my cookies from the last hour", "make the command bar taller and green".
It uses [Groq](https://groq.com/); the first time, it asks for your API key, which is
stored in the browser's data folder. `:model` picks the model, and `:aihist` lists past
chats. The `:commands` page lists everything it can do.

`i` starts typing a question, Enter sends it, and Esc or Ctrl+S leaves the field.
Answers are rendered: headings, lists, tables, links and code blocks show formatted,
and `v`/`y` select and copy the text without the markdown syntax. `H`/`L` step
through saved chats.

## Updates

The browser updates itself. It checks at launch and every few hours; a newer version is
downloaded in the background, checked against the checksum published with the release,
and installed when you quit, so the next launch is the new version. The status bar shows
`[0.4.0 installs when you quit]` meanwhile. Nothing asks for permission: the installer
is per-user.

`:update` checks now and shows what's in the new version; `:update install` installs it
right away and restarts the browser with your tabs. `:update notify` only announces new
versions (install them with `:update install`), `:update off` stops checking, and
`:update auto` goes back to the default.

Copies installed by 0.3.0 or earlier live in Program Files; the first `:update install`
moves them to the per-user install (Windows asks once, to remove the old copy). Updating
only works for a copy installed with the installer; a copy built from source updates with
`git pull`.

## What's new

`:news` (or `:changelog`) shows what changed in each release, newest first; `f`
follows a commit link. The first launch after an update says so in the status bar.

## Engines: WebView2 and Servo

Web pages render in one of two engines. **WebView2** (Microsoft Edge's Chromium,
built into Windows) is the default. **[Servo](https://servo.org/)** is an independent
engine written in Rust, shipped with the browser.

- `:engines` lists the installed engines.
- `:engine servo` reopens the current page in Servo; `:engine webview2` goes back.
- `:engine default servo` makes new web tabs open in Servo.

Both engines can sit side by side in a split. Modes, every hint type (`f`, `F`,
`yf`, `s`), zoom and history work the same in either.

Servo's limits for now: no extensions (so no uBlock Origin) and no network-level ad
blocking, no private (`:open -n`) or no-JavaScript (`:nojs`) tabs, no downloads, and
selecting page text is limited. Many sites render or behave differently than in
Chromium; YouTube, for one, shows its page but not its videos yet, so use
`:engine webview2` there. Servo keeps its own cookies and storage, so you sign in to sites separately
in each engine. Switching engines reopens the page's address; form contents and other
page state don't carry over.

If a Servo page crashes, only its pane is affected: it shows what happened, and
`:reload` opens the page again. Once Servo has started, it stays loaded until you quit
the browser.
