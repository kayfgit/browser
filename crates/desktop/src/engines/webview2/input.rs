//! Trusted input through the DevTools protocol. A click dispatched from page script is
//! untrusted, so the page refuses it anything that needs a user gesture (writing the
//! clipboard, opening a pop-up); `Input.dispatchMouseEvent` arrives as real input.
use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
use webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2;
use windows_core::HSTRING;
use wry::{WebView, WebViewExtWindows};

use browser_engine::EngineResult;

/// Move the pointer to `(x, y)`, CSS pixels in the viewport, then press and release the
/// left button there. Each event is sent once the previous one has been dispatched, so
/// they can't be reordered. The move comes first because some controls only accept a
/// click once hovered (and captcha widgets look for a pointer that got there).
pub(crate) fn click(webview: &WebView, x: f64, y: f64) -> EngineResult {
    let core = unsafe { webview.controller().CoreWebView2() }.map_err(|e| e.to_string())?;
    let event = |kind: &str, button: &str, buttons: u8| {
        format!(
            r#"{{"type":"{kind}","x":{x},"y":{y},"button":"{button}","buttons":{buttons},"clickCount":1}}"#
        )
    };
    dispatch(
        core,
        vec![
            event("mouseMoved", "none", 0),
            event("mousePressed", "left", 1),
            event("mouseReleased", "left", 0),
        ],
    )
    .map_err(|e| e.to_string())
}

/// Send `events` (in order) through `Input.dispatchMouseEvent`, each one from the
/// completion of the one before. Stops at the first event the page refuses.
fn dispatch(core: ICoreWebView2, mut events: Vec<String>) -> windows_core::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let params = HSTRING::from(events.remove(0));
    let next = core.clone();
    let done = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |hr, _| {
        if hr.is_ok() {
            dispatch(next, events)?;
        }
        Ok(())
    }));
    let method = HSTRING::from("Input.dispatchMouseEvent");
    unsafe { core.CallDevToolsProtocolMethod(&method, &params, &done) }
}
