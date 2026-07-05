//! A low-level keyboard hook (`WH_KEYBOARD_LL`) that lets the shell intercept its
//! reserved mode-exit chords even when keyboard focus lives inside the web page —
//! including a cross-origin iframe, where the injected [`BRIDGE_JS`] can't run. That
//! is the root cause of "stuck in passthrough, only alt-tab gets me out": the leave
//! chord was caught solely by in-page JS.
//!
//! It posts SEMANTIC [`UserEvent`]s back to the event loop — it never replays raw
//! keystrokes (tao's `KeyEvent` isn't constructible), so it only handles the small,
//! fixed set of chords below and passes everything else straight through. It acts
//! ONLY while our own window is the foreground window, so other apps are never
//! touched, and only in the web Passthrough / yielded states — in ordinary
//! Normal-mode browsing the hook is inert and the existing focus model is unchanged.

/// Mode codes the hook reads (lock-free) to decide what to intercept. Kept in sync
/// from the event loop via [`set_mode`]. `OTHER` means the shell owns the keyboard
/// (welcome/read/term/command/plain-Normal) — the hook stays completely out of the way.
pub(crate) const MODE_OTHER: u8 = 0;
/// Normal mode, but the page kept keyboard focus after a click on a control (so its
/// menus/popovers stay open). Esc snaps the shell back into control.
pub(crate) const MODE_NORMAL_YIELDED: u8 = 1;
/// Web passthrough: the page (or a cross-origin iframe) holds OS focus, so the hook is
/// what catches the anti-trap leave chords — Esc, or Ctrl+S — and returns to Normal.
pub(crate) const MODE_PASSTHROUGH: u8 = 3;

#[cfg(windows)]
mod imp {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicIsize, AtomicU8, Ordering};

    use tao::event_loop::EventLoopProxy;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetClassNameW, GetForegroundWindow, GetWindowThreadProcessId,
        SetWindowsHookExW, KBDLLHOOKSTRUCT, WH_KEYBOARD_LL, WM_KEYDOWN, WM_SYSKEYDOWN,
    };

    use super::{MODE_NORMAL_YIELDED, MODE_PASSTHROUGH};
    use crate::app::UserEvent;

    const VK_ESCAPE: u32 = 0x1B;
    const VK_S: u32 = 0x53;
    const VK_R: u32 = 0x52;
    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;
    const VK_MENU: i32 = 0x12; // Alt

    static MODE: AtomicU8 = AtomicU8::new(super::MODE_OTHER);
    static HWND_VAL: AtomicIsize = AtomicIsize::new(0);

    // The proxy lives on (and is only used from) the main thread — the LL hook proc
    // runs in the context of the thread that installed it, which is our event-loop
    // thread — so a thread-local sidesteps `EventLoopProxy`'s Sync bound.
    thread_local! {
        static PROXY: RefCell<Option<EventLoopProxy<UserEvent>>> = const { RefCell::new(None) };
    }

    pub(crate) fn set_mode(code: u8) {
        MODE.store(code, Ordering::Relaxed);
    }

    pub(crate) fn install(hwnd: isize, proxy: EventLoopProxy<UserEvent>) {
        HWND_VAL.store(hwnd, Ordering::Relaxed);
        PROXY.with(|p| *p.borrow_mut() = Some(proxy));
        unsafe {
            let hmod = GetModuleHandleW(None).map(|h| HINSTANCE(h.0)).unwrap_or_default();
            // The HHOOK lives for the whole process; the OS frees it at exit, so we
            // intentionally don't store/unhook it (which would need a Sync wrapper).
            let _ = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), Some(hmod), 0);
        }
    }

    fn post(ev: UserEvent) {
        PROXY.with(|p| {
            if let Some(px) = p.borrow().as_ref() {
                let _ = px.send_event(ev);
            }
        });
    }

    /// Decide whether to swallow a key-down and which event to raise, given the
    /// current mode. `None` = let the key reach the page unchanged.
    fn decide(mode: u8, vk: u32, ctrl: bool, _shift: bool) -> Option<UserEvent> {
        match mode {
            // Web passthrough leaves ONLY on Ctrl+S — frame-proof, so a page can never
            // hold that chord hostage. Esc is intentionally NOT a leave chord: it must
            // reach the page so a web SSH/vim gets its Escape.
            MODE_PASSTHROUGH if ctrl && vk == VK_S => Some(UserEvent::ExitToNormal),
            // Yielded: the page holds focus so its menu stays open; Esc reclaims the
            // shell keyboard (and blurs the page, closing the menu as a side effect).
            MODE_NORMAL_YIELDED if vk == VK_ESCAPE => Some(UserEvent::ReclaimNormal),
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

    unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code >= 0 {
            // Only ever act when WE are foreground — never intercept keys for other apps.
            let fg = GetForegroundWindow();
            if fg_is_ours(fg) {
                let msg = wparam.0 as u32;
                if msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN {
                    let kb = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
                    let ctrl = (windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(VK_CONTROL)
                        as u16
                        & 0x8000)
                        != 0;
                    let shift = (windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(VK_SHIFT)
                        as u16
                        & 0x8000)
                        != 0;
                    let alt = (windows::Win32::UI::Input::KeyboardAndMouse::GetKeyState(VK_MENU)
                        as u16
                        & 0x8000)
                        != 0;
                    // The brick-proof panic button: Ctrl+Alt+Shift+R resets all
                    // customization to defaults. Checked here, ABOVE the mode gate and
                    // independent of the (potentially rebound) keybind layer, so it
                    // works in any mode no matter how keys get customized.
                    if ctrl && alt && shift && kb.vkCode == VK_R {
                        post(UserEvent::RestoreDefaults);
                        return LRESULT(1);
                    }
                    if let Some(ev) = decide(MODE.load(Ordering::Relaxed), kb.vkCode, ctrl, shift) {
                        post(ev);
                        return LRESULT(1); // swallow: the page must not also see it
                    }
                }
            }
        }
        CallNextHookEx(None, code, wparam, lparam)
    }
}

#[cfg(windows)]
pub(crate) use imp::{install, set_mode};

#[cfg(not(windows))]
pub(crate) fn set_mode(_code: u8) {}

#[cfg(not(windows))]
pub(crate) fn install(_hwnd: isize, _proxy: tao::event_loop::EventLoopProxy<crate::app::UserEvent>) {}
