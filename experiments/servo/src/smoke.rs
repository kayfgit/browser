//! Native messages target only this lab's HWNDs; no global SendInput or mouse moves.
use super::{Lab, Message, native};
use std::time::{Duration, Instant};
use tao::event_loop::EventLoopProxy;
use windows::Win32::{
    Foundation::{LPARAM, POINT, WPARAM},
    Graphics::Gdi::ClientToScreen,
    UI::{Input::KeyboardAndMouse::GetFocus, WindowsAndMessaging::*},
};

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Locate,
    Focus,
    Type,
    Backspace,
    Ime,
    ImeBackspace,
    Scroll,
    Menu,
    Recovery,
    Done,
}
impl Step {
    fn next(self) -> Self {
        match self {
            Self::Locate => Self::Focus,
            Self::Focus => Self::Type,
            Self::Type => Self::Backspace,
            Self::Backspace => Self::Ime,
            Self::Ime => Self::ImeBackspace,
            Self::ImeBackspace => Self::Scroll,
            Self::Scroll => Self::Menu,
            Self::Menu => Self::Recovery,
            Self::Recovery | Self::Done => Self::Done,
        }
    }
}

pub struct InputSmoke {
    step: Step,
    due: Instant,
    point: (i32, i32),
    menu_seen: bool,
    pending: bool,
    recovery_clicked: bool,
}
impl Default for InputSmoke {
    fn default() -> Self {
        Self {
            step: Step::Locate,
            due: Instant::now(),
            point: (0, 0),
            menu_seen: false,
            pending: false,
            recovery_clicked: false,
        }
    }
}
impl InputSmoke {
    pub fn done(&self) -> bool {
        self.step == Step::Done
    }
    pub fn context_menu(&mut self) {
        self.menu_seen = true;
    }
    pub fn tick(&mut self, lab: &Lab, proxy: &EventLoopProxy<Message>) -> Result<(), String> {
        if self.pending || self.done() || Instant::now() < self.due {
            return Ok(());
        }
        if self.step == Step::Recovery && !self.recovery_clicked {
            click(&lab.servo_window, self.point, false)?;
            self.recovery_clicked = true;
            self.due = Instant::now() + Duration::from_millis(400);
            return Ok(());
        }
        let script = match self.step {
            Step::Locate => {
                lab.webview2.focus().map_err(|e| e.to_string())?;
                "const r = document.querySelector('input').getBoundingClientRect(); [r.x + r.width / 2, r.y + r.height / 2].join(',')"
            }
            Step::Focus => {
                if unsafe { GetFocus() } != native::hwnd(&lab.servo_window) {
                    return Err("Servo click did not transfer native focus from WebView2".into());
                }
                "String(document.activeElement === document.querySelector('input'))"
            }
            Step::Type => {
                "JSON.stringify([document.querySelector('input').value, document.querySelector('#key').textContent, document.hasFocus(), document.activeElement.tagName])"
            }
            Step::Backspace => "String(document.querySelector('input').value === '')",
            Step::Ime => "String(document.querySelector('input').value === 'é')",
            Step::ImeBackspace => "String(document.querySelector('input').value === '')",
            Step::Scroll => "String(window.scrollY)",
            Step::Menu => {
                if !self.menu_seen {
                    return Err("Native right-click did not request a context menu".into());
                }
                lab.webview2.focus().map_err(|e| e.to_string())?;
                "String(document.querySelector('input').value === '')"
            }
            Step::Recovery => {
                if unsafe { GetFocus() } != native::hwnd(&lab.servo_window) {
                    return Err("Second Servo click did not recover keyboard focus".into());
                }
                "String(document.activeElement === document.querySelector('input'))"
            }
            _ => unreachable!(),
        };
        self.pending = true;
        let proxy = proxy.clone();
        lab.page.raw().evaluate_javascript(script, move |result| {
            let result = match result {
                Ok(servo::JSValue::String(value)) => Ok(value),
                other => Err(format!("Input probe evaluation: {other:?}")),
            };
            let _ = proxy.send_event(Message::InputProbe(result));
        });
        Ok(())
    }

    pub fn result(&mut self, lab: &Lab, result: Result<String, String>) -> Result<(), String> {
        let value = result?;
        let window = &lab.servo_window;
        match self.step {
            Step::Locate => {
                let focused = unsafe { GetFocus() };
                if focused.0.is_null()
                    || focused == native::hwnd(window)
                    || !unsafe { IsChild(native::hwnd(&lab.parent), focused) }.as_bool()
                {
                    return Err("Could not establish WebView2 focus before input probe".into());
                }
                let (x, y) = value.split_once(',').ok_or("Missing input coordinates")?;
                let scale = window.scale_factor();
                self.point = (
                    (x.parse::<f64>().map_err(|e| e.to_string())? * scale) as i32,
                    (y.parse::<f64>().map_err(|e| e.to_string())? * scale) as i32,
                );
                click(window, self.point, false)?;
            }
            Step::Focus => {
                expect(&value, "input focus")?;
                key(window, 0x41, 0x1e, false)?;
            }
            Step::Type => {
                if !value.starts_with("[\"a\",") {
                    return Err(format!("Native typing diagnostics: {value}"));
                }
                key(window, 0x41, 0x1e, true)?;
                key(window, 0x08, 0x0e, false)?;
            }
            Step::Backspace => {
                expect(&value, "Backspace")?;
                key(window, 0x08, 0x0e, true)?;
                unsafe {
                    PostMessageW(
                        Some(native::hwnd(window)),
                        WM_IME_ENDCOMPOSITION,
                        WPARAM(0),
                        LPARAM(0),
                    )
                    .map_err(|e| e.to_string())?;
                    PostMessageW(
                        Some(native::hwnd(window)),
                        WM_CHAR,
                        WPARAM('é' as usize),
                        LPARAM(1),
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            Step::Ime => {
                expect(&value, "committed accented text")?;
                key(window, 0x08, 0x0e, false)?;
            }
            Step::ImeBackspace => {
                expect(&value, "Backspace after committed text")?;
                key(window, 0x08, 0x0e, true)?;
                // Scroll over ordinary page content, not an editable control.
                let mut point = POINT { x: 40, y: 100 };
                unsafe {
                    let _ = ClientToScreen(native::hwnd(window), &mut point);
                    PostMessageW(
                        Some(native::hwnd(window)),
                        WM_MOUSEWHEEL,
                        WPARAM(((-120i16 as u16 as u32) << 16) as usize),
                        packed(point.x, point.y),
                    )
                    .map_err(|e| e.to_string())?;
                }
            }
            Step::Scroll => {
                let distance = value.parse::<f64>().map_err(|e| e.to_string())?;
                let expected = native::wheel_notch_pixels(window) / window.scale_factor();
                if (distance - expected).abs() > 3.0 {
                    return Err(format!(
                        "Wheel scrolled {distance} CSS px; expected {expected}"
                    ));
                }
                eprintln!("Input probe: one wheel notch scrolled {distance} CSS px");
                click(window, (40, 100), true)?;
            }
            Step::Menu => {
                expect(&value, "input retained on focus transfer")?;
                let focused = unsafe { GetFocus() };
                if focused.0.is_null()
                    || focused == native::hwnd(window)
                    || !unsafe { IsChild(native::hwnd(&lab.parent), focused) }.as_bool()
                {
                    return Err("Could not return native focus to WebView2".into());
                }
                lab.page
                    .raw()
                    .evaluate_javascript("window.scrollTo(0, 0)", |_| {});
            }
            Step::Recovery => expect(&value, "focus recovery")?,
            _ => return Err("Unexpected input probe result".into()),
        }
        self.step = self.step.next();
        self.pending = false;
        self.due = Instant::now() + Duration::from_millis(400);
        Ok(())
    }
}
fn expect(value: &str, label: &str) -> Result<(), String> {
    if value == "true" {
        Ok(())
    } else {
        Err(format!("Input probe failed: {label}: {value}"))
    }
}
fn packed(x: i32, y: i32) -> LPARAM {
    LPARAM(((x as u16 as u32) | ((y as u16 as u32) << 16)) as isize)
}
fn click(window: &tao::window::Window, point: (i32, i32), right: bool) -> Result<(), String> {
    unsafe {
        for (message, buttons) in [
            (WM_MOUSEMOVE, 0),
            (
                if right {
                    WM_RBUTTONDOWN
                } else {
                    WM_LBUTTONDOWN
                },
                if right { 2 } else { 1 },
            ),
            (if right { WM_RBUTTONUP } else { WM_LBUTTONUP }, 0),
        ] {
            PostMessageW(
                Some(native::hwnd(window)),
                message,
                WPARAM(buttons),
                packed(point.0, point.1),
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}
pub(super) fn key(
    window: &tao::window::Window,
    key: usize,
    scan: u32,
    up: bool,
) -> Result<(), String> {
    unsafe {
        PostMessageW(
            Some(native::hwnd(window)),
            if up { WM_KEYUP } else { WM_KEYDOWN },
            WPARAM(key),
            LPARAM((1u32 | (scan << 16) | if up { 0xc0000000 } else { 0 }) as isize),
        )
        .map_err(|e| e.to_string())
    }
}
