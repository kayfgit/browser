//! WebView2's `AcceleratorKeyPressed`: the shell's keys that must work while a page has
//! keyboard focus, inside iframes too (see [`crate::shellkeys`]).
//!
//! WebView2 raises the event in the host for Esc, function keys and Ctrl/Alt chords
//! before the page sees them, wherever focus is inside the web view. Marking one
//! handled keeps it from the page.

use webview2_com::AcceleratorKeyPressedEventHandler;
use webview2_com::Microsoft::Web::WebView2::Win32::{
    COREWEBVIEW2_KEY_EVENT_KIND, COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN,
    COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN,
};
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use wry::{WebView, WebViewExtWindows};

use crate::shellkeys;

/// Whether a key is down right now. The event reaches the host asynchronously, so the
/// UI thread's synchronous key state may not reflect it yet.
fn down(vk: i32) -> bool {
    // SAFETY: `GetAsyncKeyState` only reads the key state.
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

/// Register the handler on a freshly built content web view. Best-effort: if the
/// registration fails, the page script still covers the main frame.
pub(crate) fn install(webview: &WebView, proxy: crate::engines::PageEventProxy) {
    let controller = webview.controller();
    let handler = AcceleratorKeyPressedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = COREWEBVIEW2_KEY_EVENT_KIND::default();
        let mut vk = 0u32;
        // SAFETY: out-parameters of a live event-args object.
        unsafe {
            args.KeyEventKind(&mut kind)?;
            args.VirtualKey(&mut vk)?;
        }
        if kind != COREWEBVIEW2_KEY_EVENT_KIND_KEY_DOWN
            && kind != COREWEBVIEW2_KEY_EVENT_KIND_SYSTEM_KEY_DOWN
        {
            return Ok(());
        }
        let (ctrl, alt, shift) = (down(0x11), down(0x12), down(0x10));
        let mode = shellkeys::mode();
        let decision = shellkeys::accelerator(mode, vk, ctrl, alt, shift);
        shellkeys::debug_log(|| {
            format!(
                "accelerator vk={vk:#x} ctrl={ctrl} alt={alt} shift={shift} mode={mode} -> {}",
                if decision.is_some() { "shell" } else { "page" }
            )
        });
        if let Some(event) = decision {
            // SAFETY: a live event-args object.
            unsafe { args.SetHandled(true)? };
            let _ = proxy.send_event(event);
        }
        Ok(())
    }));
    let mut token = 0i64;
    // SAFETY: registering a handler on the web view's own controller.
    let _ = unsafe { controller.add_AcceleratorKeyPressed(&handler, &mut token) };
}
