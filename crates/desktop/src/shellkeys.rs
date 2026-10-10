//! Keys the shell must get even while a web page holds keyboard focus.
//!
//! Which path a key takes depends on where focus is:
//! * **The shell window**: ordinary key events, [`App::handle_key`](crate::App::handle_key).
//! * **Anywhere in a web view, iframes included**, for Esc, function keys and Ctrl/Alt
//!   chords: WebView2's `AcceleratorKeyPressed` event (`engines/webview2/keys.rs`),
//!   which asks [`accelerator`] before the page sees the key. This is what leaves
//!   Passthrough (Ctrl+S, Shift+Esc) and Insert (Esc) and hands the keyboard back in
//!   Normal (Esc), wherever the focus is inside the page.
//! * **A page's main frame, for everything else**: the page script
//!   (`scripts/bridge.js`). In Normal mode it hands each key that reached the page to
//!   the shell (`shell-key:`), which takes focus back and [`replay`]s it.
//!
//! This replaces a low-level keyboard hook (`WH_KEYBOARD_LL`), which Windows stops
//! calling once WebView2 runs in the process, so it never worked with a page open.

use std::sync::atomic::{AtomicU8, Ordering};

use crate::app::UserEvent;

/// The shell doesn't need keys back from a page: no web page is focused in a typing
/// mode (welcome, read, terminal, command bar, …).
pub(crate) const MODE_OTHER: u8 = 0;
/// Normal mode on a web tab: the shell should hold the keyboard; Esc in the page
/// hands it back.
pub(crate) const MODE_NORMAL_WEB: u8 = 1;
/// Web Insert (typing into a field): Esc leaves (and reaches the page too, closing its
/// popup). Ctrl+V is left to the page (paste).
pub(crate) const MODE_INSERT: u8 = 2;
/// Web Passthrough (every key to the page): only Ctrl+S or Shift+Esc leave. Plain Esc
/// reaches the page, since a web terminal or editor needs it.
pub(crate) const MODE_PASSTHROUGH: u8 = 3;

static MODE: AtomicU8 = AtomicU8::new(MODE_OTHER);

/// Publish the shell's current key mode (see [`App::key_mode`](crate::App::key_mode)).
/// Called after every event, so engine callbacks read an up-to-date value.
pub(crate) fn set_mode(code: u8) {
    MODE.store(code, Ordering::Relaxed);
}

pub(crate) fn mode() -> u8 {
    MODE.load(Ordering::Relaxed)
}

const VK_ESCAPE: u32 = 0x1B;
const VK_R: u32 = 0x52;
const VK_S: u32 = 0x53;

/// The panic button: Ctrl+Alt+Shift+R resets all customization, in any mode, ahead of
/// every other binding.
pub(crate) fn is_reset_chord(vk: u32, ctrl: bool, alt: bool, shift: bool) -> bool {
    ctrl && alt && shift && vk == VK_R
}

/// What an accelerator key pressed inside a focused web view means to the shell in
/// `mode`, or `None` to let the page have it.
pub(crate) fn accelerator(
    mode: u8,
    vk: u32,
    ctrl: bool,
    alt: bool,
    shift: bool,
) -> Option<UserEvent> {
    if is_reset_chord(vk, ctrl, alt, shift) {
        return Some(UserEvent::RestoreDefaults);
    }
    match mode {
        MODE_PASSTHROUGH if (ctrl && !alt && vk == VK_S) || (shift && vk == VK_ESCAPE) => {
            Some(UserEvent::ExitToNormal)
        }
        MODE_INSERT if vk == VK_ESCAPE && !shift => Some(UserEvent::InsertEscape),
        MODE_NORMAL_WEB if vk == VK_ESCAPE && !ctrl && !alt => Some(UserEvent::ReclaimNormal),
        _ => None,
    }
}

/// Whether a key the shell takes ([`accelerator`]) should ALSO reach the page. Only Esc
/// in Insert: leaving a field with Esc should close what the page tied to it (a search
/// popup, a dropdown) exactly as in any browser — so the page gets the Esc too, and the
/// shell still leaves Insert even when the field sits in an iframe the page script
/// doesn't cover.
pub(crate) fn page_sees_too(mode: u8, vk: u32, shift: bool) -> bool {
    mode == MODE_INSERT && vk == VK_ESCAPE && !shift
}

/// With `BROWSER_KEY_DEBUG` set, append a line to `%TEMP%\browser-keys.log`: every
/// accelerator decision and key replay. For diagnosing focus problems on a user's
/// machine.
pub(crate) fn debug_log(line: impl FnOnce() -> String) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("BROWSER_KEY_DEBUG").is_some()) {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::temp_dir().join("browser-keys.log"))
    {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let _ = writeln!(f, "{ms} {}", line());
    }
}

/// A keystroke the page handed back to the shell, to be re-injected once the shell
/// holds keyboard focus again (see [`replay`]). The modifier flags record what was held
/// when the key went down, in case they're released before the replay.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct KeyReplay {
    pub vk: u16,
    pub scan: u16,
    pub extended: bool,
    pub shift: bool,
    pub ctrl: bool,
}

impl KeyReplay {
    /// A key reported by a page (the DOM `keyCode`, which on Windows is the virtual-key
    /// code) with the modifiers it was pressed with. The scan code is looked up, since
    /// the shell matches some bindings on the physical key.
    pub(crate) fn from_vk(vk: u16, shift: bool, ctrl: bool) -> Self {
        #[cfg(windows)]
        let scan = {
            use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_VSC};
            // SAFETY: a pure keyboard-layout lookup.
            unsafe { MapVirtualKeyW(u32::from(vk), MAPVK_VK_TO_VSC) as u16 }
        };
        #[cfg(not(windows))]
        let scan = 0;
        // Insert/Delete, Home/End, Page Up/Down, the arrows, Win keys, numpad divide.
        let extended = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B | 0x5C | 0x6F);
        KeyReplay {
            vk,
            scan,
            extended,
            shift,
            ctrl,
        }
    }
}

/// Re-inject a keystroke the page handed back, now that the shell holds keyboard focus
/// again, so it arrives as ordinary input. A modifier that was held at the original
/// press but has been released since is pressed around the key, so a quick `:` still
/// arrives as `:` rather than `;`.
#[cfg(windows)]
pub(crate) fn replay(key: KeyReplay) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
        KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    const VK_SHIFT: i32 = 0x10;
    const VK_CONTROL: i32 = 0x11;
    const VK_LSHIFT: u16 = 0xA0;
    const VK_LCONTROL: u16 = 0xA2;
    fn input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
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
    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    debug_log(|| {
        format!(
            "replay vk={:#x}: sent {sent}/{} inputs",
            key.vk,
            inputs.len()
        )
    });
}

#[cfg(not(windows))]
pub(crate) fn replay(_key: KeyReplay) {}

#[cfg(test)]
mod tests {
    use super::*;

    const ESC: u32 = VK_ESCAPE;

    #[test]
    fn passthrough_leaves_only_on_ctrl_s_or_shift_esc() {
        let exit = |vk, ctrl, shift| {
            matches!(
                accelerator(MODE_PASSTHROUGH, vk, ctrl, false, shift),
                Some(UserEvent::ExitToNormal)
            )
        };
        assert!(exit(VK_S, true, false));
        assert!(exit(ESC, false, true));
        // Plain Esc and other chords belong to the page (a web terminal needs them).
        assert!(accelerator(MODE_PASSTHROUGH, ESC, false, false, false).is_none());
        assert!(accelerator(MODE_PASSTHROUGH, 0x43, true, false, false).is_none());
        // Ctrl+Alt+S is AltGr+S on some layouts: a character, not the leave chord.
        assert!(accelerator(MODE_PASSTHROUGH, VK_S, true, true, false).is_none());
    }

    #[test]
    fn insert_leaves_on_esc_and_normal_reclaims_on_esc() {
        assert!(matches!(
            accelerator(MODE_INSERT, ESC, false, false, false),
            Some(UserEvent::InsertEscape)
        ));
        assert!(matches!(
            accelerator(MODE_NORMAL_WEB, ESC, false, false, false),
            Some(UserEvent::ReclaimNormal)
        ));
        // Ctrl+V pastes in Insert.
        assert!(accelerator(MODE_INSERT, 0x56, true, false, false).is_none());
        // Esc in Insert also reaches the page (closing its popup); nothing else is shared.
        assert!(page_sees_too(MODE_INSERT, ESC, false));
        assert!(!page_sees_too(MODE_NORMAL_WEB, ESC, false));
        assert!(!page_sees_too(MODE_PASSTHROUGH, ESC, true));
        assert!(accelerator(MODE_OTHER, ESC, false, false, false).is_none());
    }

    #[test]
    fn the_reset_chord_works_in_every_mode() {
        for mode in [MODE_OTHER, MODE_NORMAL_WEB, MODE_INSERT, MODE_PASSTHROUGH] {
            assert!(matches!(
                accelerator(mode, VK_R, true, true, true),
                Some(UserEvent::RestoreDefaults)
            ));
        }
        assert!(!is_reset_chord(VK_R, true, true, false));
    }

    #[test]
    fn page_keys_replay_with_their_scan_code() {
        let k = KeyReplay::from_vk(0xBA, true, false);
        assert_eq!(
            (k.vk, k.shift, k.ctrl, k.extended),
            (0xBA, true, false, false)
        );
        assert!(KeyReplay::from_vk(0x2E, false, false).extended); // Delete
    }
}
