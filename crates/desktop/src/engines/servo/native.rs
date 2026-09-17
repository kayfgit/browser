//! Operations on the Servo child HWND, confined to the Tao event-loop thread.
use servo::{ContextMenu, ContextMenuAction, ContextMenuItem, Cursor};
use std::sync::atomic::{AtomicBool, Ordering};
use tao::{platform::windows::WindowExtWindows, window::Window};
use windows::{
    Win32::{
        Foundation::{HWND, POINT},
        Graphics::Gdi::ClientToScreen,
        UI::{
            Input::KeyboardAndMouse::{GetFocus, SetFocus},
            WindowsAndMessaging::*,
        },
    },
    core::PCWSTR,
};

pub fn hwnd(window: &Window) -> HWND {
    HWND(window.hwnd() as *mut std::ffi::c_void)
}

pub fn focused(window: &Window) -> bool {
    unsafe { GetFocus() == hwnd(window) }
}

#[allow(dead_code)] // Also compiled by the standalone native-input qualification lab.
pub fn focused_handle() -> isize {
    unsafe { GetFocus().0 as isize }
}

pub fn focus(window: &Window) -> bool {
    // Tao's set_focus activates a top-level window; it does not assign keyboard
    // focus to a WS_CHILD. Keep activation with the parent and focus this child.
    unsafe {
        let _ = SetFocus(Some(hwnd(window)));
        GetFocus() == hwnd(window)
    }
}

fn scroll_lines(horizontal: bool) -> u32 {
    let mut lines = 3u32;
    unsafe {
        let _ = SystemParametersInfoW(
            if horizontal {
                SPI_GETWHEELSCROLLCHARS
            } else {
                SPI_GETWHEELSCROLLLINES
            },
            0,
            Some((&mut lines as *mut u32).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    lines
}

pub fn wheel_line_pixels(window: &Window, horizontal: bool) -> f64 {
    if !horizontal && scroll_lines(false) == u32::MAX {
        // Tao 0.35 maps Windows page scrolling to three lines per notch.
        f64::from(window.inner_size().height) / 3.0
    } else {
        // Tao already multiplies notches by the OS-configured line count.
        // Servo 0.5's compositor consumes physical pixels regardless of deltaMode.
        24.0 * window.scale_factor()
    }
}

#[allow(dead_code)] // The lab compares the observed scroll with this OS setting.
pub fn wheel_notch_pixels(window: &Window) -> f64 {
    let lines = scroll_lines(false);
    if lines == u32::MAX {
        f64::from(window.inner_size().height)
    } else {
        f64::from(lines) * 24.0 * window.scale_factor()
    }
}

pub fn refresh_cursor(window: &Window, cursor: Cursor) {
    unsafe {
        let mut point = POINT::default();
        if GetCursorPos(&mut point).is_err() || WindowFromPoint(point) != hwnd(window) {
            return;
        }
        // Cursor is thread-global. A background Servo callback must never change
        // WebView2's cursor. Restore ours after native dispatch too: WebView2 can
        // hide the pointer on typing while the pointer is over this child.
        let resource = match cursor {
            Cursor::None => {
                SetCursor(None);
                return;
            }
            Cursor::Pointer => IDC_HAND,
            Cursor::Text | Cursor::VerticalText => IDC_IBEAM,
            Cursor::Wait => IDC_WAIT,
            Cursor::Progress => IDC_APPSTARTING,
            Cursor::Help => IDC_HELP,
            Cursor::Crosshair | Cursor::Cell => IDC_CROSS,
            Cursor::NotAllowed | Cursor::NoDrop => IDC_NO,
            Cursor::Move | Cursor::AllScroll | Cursor::Grab | Cursor::Grabbing => IDC_SIZEALL,
            Cursor::EResize | Cursor::WResize | Cursor::EwResize | Cursor::ColResize => IDC_SIZEWE,
            Cursor::NResize | Cursor::SResize | Cursor::NsResize | Cursor::RowResize => IDC_SIZENS,
            Cursor::NeResize | Cursor::SwResize | Cursor::NeswResize => IDC_SIZENESW,
            Cursor::NwResize | Cursor::SeResize | Cursor::NwseResize => IDC_SIZENWSE,
            _ => IDC_ARROW,
        };
        if let Ok(handle) = LoadCursorW(None, resource) {
            SetCursor(Some(handle));
        }
    }
}

struct Popup(HMENU);
impl Drop for Popup {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyMenu(self.0);
        }
    }
}

// Smoke mode exercises the real native menu loop and dismisses only its own menu.
// This timer runs on the same UI thread; it never injects keys or moves the pointer.
static SMOKE_MENU_OPENED: AtomicBool = AtomicBool::new(false);
unsafe extern "system" fn dismiss_smoke_menu(window: HWND, _: u32, _: usize, _: u32) {
    unsafe {
        let mut info = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        if GetGUIThreadInfo(GetWindowThreadProcessId(window, None), &mut info).is_ok()
            && info.flags.0 & GUI_POPUPMENUMODE.0 != 0
        {
            SMOKE_MENU_OPENED.store(true, Ordering::Relaxed);
            let _ = EndMenu();
        }
    }
}
struct MenuTimer(HWND, usize);
impl Drop for MenuTimer {
    fn drop(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.0), self.1);
        }
    }
}

pub fn show_context_menu(
    window: &Window,
    request: ContextMenu,
    smoke: bool,
) -> windows::core::Result<()> {
    unsafe {
        let popup = Popup(CreatePopupMenu()?);
        let mut actions = Vec::new();
        for item in request.items() {
            match item {
                ContextMenuItem::Separator => AppendMenuW(popup.0, MF_SEPARATOR, 0, None)?,
                ContextMenuItem::Item {
                    label,
                    action,
                    enabled,
                } => {
                    // The lab has no new-view host yet; don't offer actions that
                    // would silently discard the requested new view.
                    let enabled = *enabled
                        && !matches!(
                            action,
                            ContextMenuAction::OpenLinkInNewWebView
                                | ContextMenuAction::OpenImageInNewView
                        );
                    actions.push(enabled.then_some(*action));
                    let label: Vec<u16> = label
                        .replace('&', "&&")
                        .replace('\0', "")
                        .encode_utf16()
                        .chain([0])
                        .collect();
                    AppendMenuW(
                        popup.0,
                        MF_STRING | if enabled { MF_ENABLED } else { MF_GRAYED },
                        actions.len(),
                        PCWSTR(label.as_ptr()),
                    )?;
                }
            }
        }
        let rect = request.position();
        let mut point = POINT {
            x: rect.min.x,
            y: rect.max.y,
        };
        let _ = ClientToScreen(hwnd(window), &mut point);
        // Prefer the pointer for mouse invocation, retaining an element-relative
        // anchor for keyboard invocation or a pointer over another surface.
        let mut pointer = POINT::default();
        if GetCursorPos(&mut pointer).is_ok() && WindowFromPoint(pointer) == hwnd(window) {
            point = pointer;
        }
        let timer = if smoke {
            SMOKE_MENU_OPENED.store(false, Ordering::Relaxed);
            let id = SetTimer(
                Some(hwnd(window)),
                0x53455256,
                100,
                Some(dismiss_smoke_menu),
            );
            if id == 0 {
                return Err(windows::core::Error::from_hresult(
                    windows::core::HRESULT::from_win32(
                        windows::Win32::Foundation::GetLastError().0,
                    ),
                ));
            }
            Some(MenuTimer(hwnd(window), id))
        } else {
            None
        };
        let selected = TrackPopupMenu(
            popup.0,
            TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON,
            point.x,
            point.y,
            None,
            hwnd(window),
            None,
        )
        .0 as usize;
        drop(timer);
        if smoke && !SMOKE_MENU_OPENED.load(Ordering::Relaxed) {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_FAIL,
                "Native popup menu did not open",
            ));
        }
        if let Some(Some(action)) = selected.checked_sub(1).and_then(|index| actions.get(index)) {
            request.select(*action);
        } else {
            request.dismiss();
        }
    }
    Ok(())
}
