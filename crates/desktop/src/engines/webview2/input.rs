//! Trusted input through the DevTools protocol. A click dispatched from page script is
//! untrusted, so the page refuses it anything that needs a user gesture (writing the
//! clipboard, opening a pop-up); `Input.dispatchMouseEvent` arrives as real input.
use webview2_com::CallDevToolsProtocolMethodCompletedHandler;
use windows_core::HSTRING;
use wry::{WebView, WebViewExtWindows};

use browser_engine::EngineResult;

/// Press and release the left button at `(x, y)`, CSS pixels in the viewport. The
/// release is sent once the press has been dispatched, so the two can't be reordered.
pub(crate) fn click(webview: &WebView, x: f64, y: f64) -> EngineResult {
    let core = unsafe { webview.controller().CoreWebView2() }.map_err(|e| e.to_string())?;
    let method = HSTRING::from("Input.dispatchMouseEvent");
    let event = |kind: &str| {
        HSTRING::from(format!(
            r#"{{"type":"{kind}","x":{x},"y":{y},"button":"left","buttons":1,"clickCount":1}}"#
        ))
    };
    let release = event("mouseReleased");
    let release_core = core.clone();
    let after_press = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |hr, _| {
        if hr.is_ok() {
            let method = HSTRING::from("Input.dispatchMouseEvent");
            let done = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(|_, _| Ok(())));
            unsafe { release_core.CallDevToolsProtocolMethod(&method, &release, &done) }?;
        }
        Ok(())
    }));
    unsafe { core.CallDevToolsProtocolMethod(&method, &event("mousePressed"), &after_press) }
        .map_err(|e| e.to_string())
}
