//! A low-level keyboard hook (`WH_KEYBOARD_LL`) that lets the shell intercept its
//! reserved mode-exit chords even when keyboard focus lives inside the web page —
//! including a cross-origin iframe, where the injected [`BRIDGE_JS`](crate::BRIDGE_JS) can't run. That
//! is the root cause of "stuck in passthrough, only alt-tab gets me out": the leave
//! chord was caught solely by in-page JS.
//!
//! It posts SEMANTIC [`UserEvent`](crate::app::UserEvent)s back to the event loop. It
//! acts ONLY while our own window is the foreground window, so other apps are never
//! touched.
//!
//! It also enforces Normal mode's rule that the shell owns the keyboard on a web tab.
//! When the page holds keyboard focus in Normal mode (a click on one of its controls,
//! a script `.focus()`, a click inside an iframe), a shell key is swallowed and sent
//! back as [`UserEvent::ReplayToShell`](crate::app::UserEvent::ReplayToShell): the
//! shell takes focus back, then [`replay`] re-injects the same keystroke so it lands
//! in the shell. Without this, `:` and friends went to the page until the user
//! alt-tabbed (the "frozen command bar after clicking a page button" bug).

/// Mode codes the hook reads (lock-free) to decide what to intercept. Kept in sync
/// from the event loop via [`set_mode`]. `OTHER` means the shell owns the keyboard
/// (welcome/read/term/command) — the hook stays completely out of the way.
pub(crate) const MODE_OTHER: u8 = 0;
/// Normal mode, but the page kept keyboard focus after a click on a control, so its
/// menu/popover stays open. Menu keys (arrows, Enter, Space, Tab, paging) still reach
/// the page; Esc snaps the shell back; any other key is replayed to the shell.
pub(crate) const MODE_NORMAL_YIELDED: u8 = 1;
/// Web Insert (light field typing): the page holds OS focus; the hook catches Esc
/// (leave), frame-proof. Ctrl+V is left to the page as a normal paste.
pub(crate) const MODE_INSERT: u8 = 2;
/// Web passthrough (sticky content control): the page (or a cross-origin iframe) holds
/// OS focus, so the hook is what catches the leave chords — Ctrl+S or Shift+Esc — and
/// returns to Normal. Plain Esc is deliberately left for the page (a web SSH/vim needs it).
pub(crate) const MODE_PASSTHROUGH: u8 = 3;
/// Normal mode on a web tab, no yield: the shell should hold the keyboard. If the page
/// has it anyway, every key is replayed to the shell.
pub(crate) const MODE_NORMAL_WEB: u8 = 4;

/// A keystroke the hook swallowed from the page, to be re-injected into the shell
/// once it holds keyboard focus again (see [`replay`]). The modifier flags record what
/// was held when the key went down, in case they're released before the replay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KeyReplay {
    pub vk: u16,
    pub scan: u16,
    pub extended: bool,
    pub shift: bool,
    pub ctrl: bool,
}

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};

    use tao::event_loop::EventLoopProxy;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::SystemInformation::GetTickCount;
    use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, DispatchMessageW, GetClassNameW, GetForegroundWindow, GetGUIThreadInfo,
        GetMessageW, GetWindowThreadProcessId, SetWindowsHookExW, TranslateMessage, GUITHREADINFO,
        KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    use std::sync::atomic::AtomicU32;

    use super::{KeyReplay, MODE_INSERT, MODE_NORMAL_WEB, MODE_NORMAL_YIELDED, MODE_PASSTHROUGH};
    use crate::app::UserEvent;

    const VK_TAB: u32 = 0x09;
    const VK_ESCAPE: u32 = 0x1B;
    const VK_S: u32 = 0x53;
    const VK_R: u32 = 0x52;
    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12; // Alt
    const VK_LWIN: i32 = 0x5B;
    const VK_RWIN: i32 = 0x5C;
    const VK_LSHIFT: u16 = 0xA0;
    const VK_LCONTROL: u16 = 0xA2;
    /// `KBDLLHOOKSTRUCT.flags` bit for an extended key (right-hand Ctrl/Alt, arrows, …).
    const LLKHF_EXTENDED: u32 = 0x01;
    /// `dwExtraInfo` stamped on the keystrokes [`replay`] injects, so the hook lets its
    /// own replays through untouched.
    const REPLAY_MARKER: usize = 0x6272_7773;

    /// How long after regaining focus a lone `Tab` is treated as the Alt+Tab straggler
    /// and swallowed rather than typed into the page. Matches the shell-side guard in
    /// `App::handle_key`.
    const TAB_SWALLOW_MS: u32 = 300;

    static MODE: AtomicU8 = AtomicU8::new(super::MODE_OTHER);
    static HWND_VAL: AtomicIsize = AtomicIsize::new(0);
    /// The UI (event-loop) thread, whose input state says whether the page or the shell
    /// window holds keyboard focus.
    static UI_THREAD: AtomicU32 = AtomicU32::new(0);
    /// `GetTickCount` at the moment our window last gained focus, stamped from the event
    /// loop via [`note_focus_gain`] AND self-stamped by the hook the first time it sees
    /// a key with us foreground after we weren't (the straggler `Tab` can arrive BEFORE
    /// the event loop has processed `Focused(true)` — the self-stamp closes that hole).
    /// Read by the hook (lock-free) to swallow the stray `Tab` that Alt+Tab delivers
    /// right after the switch. `0` = no focus gain recorded yet.
    static LAST_FOCUS_TICK: AtomicU32 = AtomicU32::new(0);
    /// Whether our window was foreground when the hook last fired — the edge detector
    /// behind the self-stamp above (alt+tabbing away always fires hooked keys with a
    /// foreign foreground window, so this flips false while the user is elsewhere).
    static WAS_FG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    // The proxy lives on (and is only used from) the hook thread — the LL hook proc
    // runs in the context of the thread that installed it — so a thread-local
    // sidesteps `EventLoopProxy`'s Sync bound.
    thread_local! {
        static PROXY: RefCell<Option<EventLoopProxy<UserEvent>>> = const { RefCell::new(None) };
    }

    pub(crate) fn set_mode(code: u8) {
        MODE.store(code, Ordering::Relaxed);
    }

    /// Record that our window just regained focus (called from `WindowEvent::Focused`).
    /// The hook uses this to swallow the Alt+Tab straggler `Tab` on a focused web page.
    pub(crate) fn note_focus_gain() {
        // SAFETY: `GetTickCount` is a pure, always-safe millisecond counter read.
        let now = unsafe { GetTickCount() };
        // Never store 0 (the "no gain yet" sentinel) — bump to 1 on the rare wrap.
        LAST_FOCUS_TICK.store(now.max(1), Ordering::Relaxed);
    }

    /// Install the hook on a dedicated thread with its own message loop.
    ///
    /// Windows calls a low-level hook by sending a message to the thread that installed
    /// it, and silently REMOVES the hook if that thread doesn't answer in time. On the
    /// UI thread, which blocks in WebView2/COM calls and was observed never to receive
    /// the calls at all, the hook was dead: every chord it exists for (leaving
    /// passthrough from an iframe, Esc out of a page-focus yield) silently stopped
    /// working, leaving alt-tab as the only way out. A thread that does nothing but pump
    /// messages always answers immediately.
    pub(crate) fn install(hwnd: isize, proxy: EventLoopProxy<UserEvent>) {
        HWND_VAL.store(hwnd, Ordering::Relaxed);
        // SAFETY: always-safe thread id read.
        UI_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
        let spawned = std::thread::Builder::new()
            .name("keyboard-hook".into())
            .spawn(move || {
                PROXY.with(|p| *p.borrow_mut() = Some(proxy));
                unsafe {
                    let hmod = GetModuleHandleW(None)
                        .map(|h| HINSTANCE(h.0))
                        .unwrap_or_default();
                    // The HHOOK lives as long as this thread, i.e. the whole process.
                    let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hmod), 0);
                    debug_log(format!("installed: {:?}", hook.as_ref().map(|h| h.0)));
                    if hook.is_err() {
                        return;
                    }
                    let mut msg = MSG::default();
                    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("keyboard hook thread: {e}");
        }
    }

    /// Whether a key is physically down. Asynchronous state, because the hook runs on
    /// its own thread, whose synchronous key state never sees any input.
    fn down(vk: i32) -> bool {
        // SAFETY: `GetAsyncKeyState` only reads the key state.
        (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
    }

    /// Whether keyboard focus on the UI thread sits somewhere other than the shell
    /// window, i.e. in the page. No focus window at all counts too: reclaiming focus
    /// is the right move then as well.
    fn page_has_focus() -> bool {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        // SAFETY: `info` is a valid GUITHREADINFO with its size set.
        if unsafe { GetGUIThreadInfo(UI_THREAD.load(Ordering::Relaxed), &mut info) }.is_err() {
            return false;
        }
        info.hwndFocus.0 as isize != HWND_VAL.load(Ordering::Relaxed)
    }

    fn post(ev: UserEvent) {
        PROXY.with(|p| {
            if let Some(px) = p.borrow().as_ref() {
                let _ = px.send_event(ev);
            }
        });
    }

    /// Modifier keys held when a key went down.
    #[derive(Clone, Copy, Default)]
    struct Mods {
        ctrl: bool,
        shift: bool,
        alt: bool,
        win: bool,
    }

    /// Modifiers and lock keys: pressing one alone is never a shell command.
    fn is_modifier_or_lock(vk: u32) -> bool {
        matches!(vk, 0x10..=0x12 | 0x14 | 0x5B | 0x5C | 0x90 | 0x91 | 0xA0..=0xA5)
    }

    /// Keys that operate a page's open menu, dropdown or popover: Tab, Enter, Space,
    /// Page Up/Down, End, Home and the arrows.
    fn is_menu_key(vk: u32) -> bool {
        matches!(vk, 0x09 | 0x0D | 0x20 | 0x21..=0x28)
    }

    /// Decide whether to swallow a key-down and which event to raise, given the
    /// current mode and whether the page (not the shell window) holds keyboard focus.
    /// `None` = let the key through unchanged.
    fn decide(
        mode: u8,
        vk: u32,
        m: Mods,
        page_has_focus: bool,
        key: KeyReplay,
    ) -> Option<UserEvent> {
        let (ctrl, shift) = (m.ctrl, m.shift);
        match mode {
            // Sticky passthrough leaves ONLY on Ctrl+S or Shift+Esc — frame-proof, so a
            // page can never hold those chords hostage. Plain Esc is intentionally NOT a
            // leave chord: it must reach the page so a web SSH/vim gets its Escape.
            MODE_PASSTHROUGH if (ctrl && vk == VK_S) || (shift && vk == VK_ESCAPE) => {
                Some(UserEvent::ExitToNormal)
            }
            // Insert (light field typing): Esc leaves. Ctrl+V is NOT intercepted — it
            // reaches the page as a normal paste.
            MODE_INSERT if vk == VK_ESCAPE && !shift => Some(UserEvent::ExitToNormal),
            // Normal mode on a web tab while the PAGE holds the keyboard. Alt and Win
            // chords (system shortcuts, AltGr) and bare modifiers always pass.
            MODE_NORMAL_YIELDED | MODE_NORMAL_WEB if page_has_focus => {
                if is_modifier_or_lock(vk) || m.alt || m.win {
                    return None;
                }
                if mode == MODE_NORMAL_YIELDED {
                    // Esc reclaims the shell keyboard (and blurs the page, closing its
                    // menu as a side effect). Menu keys keep driving the open menu.
                    if vk == VK_ESCAPE {
                        return Some(UserEvent::ReclaimNormal);
                    }
                    if is_menu_key(vk) {
                        return None;
                    }
                }
                Some(UserEvent::ReplayToShell(key))
            }
            _ => None,
        }
    }

    /// Whether the foreground window belongs to US, so the hook may act on it. This is
    /// our main shell window OR any other top-level window owned by our process —
    /// WebView2 spins up auxiliary top-level windows (popup hosts, the fullscreen-video
    /// helper, permission/print hosts) and a page (e.g. clicking a Google result) can
    /// briefly hand one of those the foreground. When that happens the OLD gate
    /// (`fg == main window` only) went inert, so NONE of the leave chords worked and the
    /// user was locked in passthrough until they alt-tab'd back — the recurring "stuck
    /// in passthrough" bug. We still refuse to touch GENUINELY other apps (PID check),
    /// and we exclude standard dialog windows (`#32770` — file/print/message boxes) so
    /// Esc keeps cancelling those rather than getting swallowed to leave passthrough.
    unsafe fn fg_is_ours(fg: HWND) -> bool {
        if fg.0 as isize == HWND_VAL.load(Ordering::Relaxed) {
            return true;
        }
        if fg.0.is_null() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(fg, Some(&mut pid));
        if pid != GetCurrentProcessId() {
            return false;
        }
        // Skip standard dialog class so Esc still closes native file/print/msg dialogs.
        let mut buf = [0u16; 32];
        let n = GetClassNameW(fg, &mut buf);
        let class = String::from_utf16_lossy(&buf[..n.max(0) as usize]);
        class != "#32770"
    }

    /// With `BROWSER_HOOK_DEBUG` set, append a line to `%TEMP%\browser-hook.log`: the
    /// install result and every key decision. For diagnosing focus problems on a
    /// user's machine, where physical keystrokes are the only faithful test.
    fn debug_log(line: String) {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if !*ON.get_or_init(|| std::env::var_os("BROWSER_HOOK_DEBUG").is_some()) {
            return;
        }
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(std::env::temp_dir().join("browser-hook.log"))
        {
            let ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            let _ = writeln!(f, "{ms} {line}");
        }
    }

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            // Our own replayed keystrokes are meant for the shell: never intercept them.
            if (*(lparam.0 as *const KBDLLHOOKSTRUCT)).dwExtraInfo == REPLAY_MARKER {
                return CallNextHookEx(None, code, wparam, lparam);
            }
            // Only ever act when WE are foreground — never intercept keys for other apps.
            let fg = GetForegroundWindow();
            let ours = fg_is_ours(fg);
            // Focus-gain edge: first hooked key with us foreground after we weren't.
            // Stamp the tick HERE too — this very key may be the Alt+Tab straggler,
            // arriving before the event loop has run `note_focus_gain`.
            if ours && !WAS_FG.swap(true, Ordering::Relaxed) {
                LAST_FOCUS_TICK.store(GetTickCount().max(1), Ordering::Relaxed);
            } else if !ours {
                WAS_FG.store(false, Ordering::Relaxed);
            }
            if ours {
                let msg = wparam.0 as u32;
                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                    let ctrl = down(VK_CONTROL);
                    let shift = down(VK_SHIFT);
                    let alt = down(VK_MENU);
                    // The brick-proof panic button: Ctrl+Alt+Shift+R resets all
                    // customization to defaults. Checked here, ABOVE the mode gate and
                    // independent of the (potentially rebound) keybind layer, so it
                    // works in any mode no matter how keys get customized.
                    if ctrl && alt && shift && kb.vkCode == VK_R {
                        post(UserEvent::RestoreDefaults);
                        return LRESULT(1);
                    }
                    let mode = MODE.load(Ordering::Relaxed);
                    // Swallow the lone `Tab` that Alt+Tab leaves behind right after the
                    // switch — in ANY mode: on a focused web page it would move form
                    // focus / type a tab; with the shell focused it lands in whatever
                    // mode is active. Only a bare Tab (no live modifier — Alt is already
                    // released by the time this straggler lands) within the guard window.
                    // We never touch the Tab while Alt is held, so the OS Alt+Tab
                    // switcher itself is unaffected. (`handle_key` has a matching guard
                    // for shell-delivered keys.)
                    if kb.vkCode == VK_TAB && !alt && !ctrl {
                        let last = LAST_FOCUS_TICK.load(Ordering::Relaxed);
                        if last != 0 && GetTickCount().wrapping_sub(last) < TAB_SWALLOW_MS {
                            return LRESULT(1); // swallow: the page must not see it
                        }
                    }
                    let win = down(VK_LWIN) || down(VK_RWIN);
                    // Only Normal on a web tab cares who holds focus.
                    let page_has_focus =
                        matches!(mode, MODE_NORMAL_WEB | MODE_NORMAL_YIELDED) && page_has_focus();
                    let mods = Mods {
                        ctrl,
                        shift,
                        alt,
                        win,
                    };
                    let key = KeyReplay {
                        vk: kb.vkCode as u16,
                        scan: kb.scanCode as u16,
                        extended: kb.flags.0 & LLKHF_EXTENDED != 0,
                        shift,
                        ctrl,
                    };
                    let decision = decide(mode, kb.vkCode, mods, page_has_focus, key);
                    debug_log(format!(
                        "vk={:#x} mode={mode} page_focus={page_has_focus} -> {}",
                        kb.vkCode,
                        match &decision {
                            None => "pass",
                            Some(UserEvent::ReplayToShell(_)) => "replay to shell",
                            Some(UserEvent::ReclaimNormal) => "reclaim",
                            Some(UserEvent::ExitToNormal) => "exit to normal",
                            Some(_) => "other",
                        }
                    ));
                    if let Some(ev) = decision {
                        post(ev);
                        return LRESULT(1); // swallow: the page must not also see it
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }

    /// Re-inject a keystroke the hook took from the page, now that the shell holds
    /// keyboard focus again, so it arrives as ordinary input. A modifier that was held
    /// at the original press but has been released since is pressed around the key,
    /// so a quick `:` still arrives as `:` rather than `;`.
    pub(crate) fn replay(key: KeyReplay) {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
            KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY,
        };
        fn input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(vk),
                        wScan: scan,
                        dwFlags: flags,
                        time: 0,
                        dwExtraInfo: REPLAY_MARKER,
                    },
                },
            }
        }
        // SAFETY: `GetAsyncKeyState` only reads the key state.
        let released = |vk: i32| (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) == 0;
        let press_shift = key.shift && released(VK_SHIFT);
        let press_ctrl = key.ctrl && released(VK_CONTROL);
        let none = KEYBD_EVENT_FLAGS(0);
        let ext = if key.extended {
            KEYEVENTF_EXTENDEDKEY
        } else {
            none
        };
        let mut inputs = Vec::with_capacity(6);
        if press_shift {
            inputs.push(input(VK_LSHIFT, 0, none));
        }
        if press_ctrl {
            inputs.push(input(VK_LCONTROL, 0, none));
        }
        inputs.push(input(key.vk, key.scan, ext));
        inputs.push(input(key.vk, key.scan, ext | KEYEVENTF_KEYUP));
        if press_ctrl {
            inputs.push(input(VK_LCONTROL, 0, KEYEVENTF_KEYUP));
        }
        if press_shift {
            inputs.push(input(VK_LSHIFT, 0, KEYEVENTF_KEYUP));
        }
        // SAFETY: `inputs` is a valid slice of initialised INPUT structs.
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::khook::MODE_OTHER;

        const COLON: u32 = 0xBA; // VK_OEM_1: `;` and `:` on a US layout
        const KEY: KeyReplay = KeyReplay {
            vk: 0xBA,
            scan: 0x27,
            extended: false,
            shift: true,
            ctrl: false,
        };
        const NONE: Mods = Mods {
            ctrl: false,
            shift: false,
            alt: false,
            win: false,
        };
        const SHIFT: Mods = Mods {
            shift: true,
            ..NONE
        };

        #[test]
        fn a_shell_key_is_replayed_when_the_page_holds_focus() {
            for mode in [MODE_NORMAL_YIELDED, MODE_NORMAL_WEB] {
                let ev = decide(mode, COLON, SHIFT, true, KEY);
                assert!(matches!(ev, Some(UserEvent::ReplayToShell(k)) if k == KEY));
            }
        }

        #[test]
        fn nothing_is_touched_while_the_shell_holds_focus() {
            for mode in [MODE_NORMAL_YIELDED, MODE_NORMAL_WEB, MODE_OTHER] {
                assert!(decide(mode, COLON, SHIFT, false, KEY).is_none());
            }
        }

        #[test]
        fn a_yielded_page_keeps_its_menu_keys_and_esc_reclaims() {
            for vk in [
                0x09, 0x0D, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
            ] {
                assert!(decide(MODE_NORMAL_YIELDED, vk, NONE, true, KEY).is_none());
            }
            assert!(matches!(
                decide(MODE_NORMAL_YIELDED, VK_ESCAPE, NONE, true, KEY),
                Some(UserEvent::ReclaimNormal)
            ));
            // Without a yield, even the menu keys belong to the shell.
            assert!(decide(MODE_NORMAL_WEB, 0x28, NONE, true, KEY).is_some());
        }

        #[test]
        fn modifiers_and_system_chords_pass_through() {
            for vk in [0x10, 0x11, 0x12, 0xA0, 0xA2, 0x5B] {
                assert!(decide(MODE_NORMAL_WEB, vk, NONE, true, KEY).is_none());
            }
            let alt = Mods { alt: true, ..NONE };
            let win = Mods { win: true, ..NONE };
            assert!(decide(MODE_NORMAL_WEB, 0x46, alt, true, KEY).is_none());
            assert!(decide(MODE_NORMAL_WEB, 0x44, win, true, KEY).is_none());
        }
    }
}

#[cfg(windows)]
pub(crate) use imp::{install, note_focus_gain, replay, set_mode};

#[cfg(not(windows))]
pub(crate) fn set_mode(_code: u8) {}

#[cfg(not(windows))]
pub(crate) fn note_focus_gain() {}

#[cfg(not(windows))]
pub(crate) fn replay(_key: KeyReplay) {}

#[cfg(not(windows))]
pub(crate) fn install(
    _hwnd: isize,
    _proxy: tao::event_loop::EventLoopProxy<crate::app::UserEvent>,
) {
}
