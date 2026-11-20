//! Sound: whether a page is playing audio (for the tab strip's speaker icon), and
//! muting one page on its own. Both are WebView2's own (`ICoreWebView2_8`): it knows
//! about every source (media elements, Web Audio, iframes), and its mute silences the
//! page without touching the page's own player state.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Keep `audible` in step with whether the page is making sound, posting a redraw on
/// each change so the strip's icon follows. Best-effort like the other COM glue: a
/// runtime too old for the interface just never shows the icon.
#[cfg(windows)]
pub(crate) fn install(
    webview: &wry::WebView,
    audible: Arc<AtomicBool>,
    proxy: crate::engines::PageEventProxy,
) {
    use webview2_com::IsDocumentPlayingAudioChangedEventHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2, ICoreWebView2_8};
    use windows_core061::Interface;

    let Some(c8) = core8(webview) else {
        return;
    };
    let handler = IsDocumentPlayingAudioChangedEventHandler::create(Box::new(
        move |sender: Option<ICoreWebView2>, _| {
            if let Some(c8) = sender.and_then(|s| s.cast::<ICoreWebView2_8>().ok()) {
                let playing = playing(&c8);
                if audible.swap(playing, Ordering::Relaxed) != playing {
                    let _ = proxy.send_event(crate::UserEvent::Redraw);
                }
            }
            Ok(())
        },
    ));
    let mut token = 0i64;
    let _ = unsafe { c8.add_IsDocumentPlayingAudioChanged(&handler, &mut token) };
}

/// Mute or unmute this page alone.
#[cfg(windows)]
pub(crate) fn set_muted(webview: &wry::WebView, muted: bool) -> Result<(), String> {
    let c8 = core8(webview).ok_or("this WebView2 runtime can't mute a single tab")?;
    unsafe { c8.SetIsMuted(muted) }.map_err(|e| e.to_string())
}

#[cfg(windows)]
fn core8(
    webview: &wry::WebView,
) -> Option<webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_8> {
    use windows_core061::Interface;
    use wry::WebViewExtWindows;
    let core = unsafe { webview.controller().CoreWebView2() }.ok()?;
    core.cast().ok()
}

#[cfg(windows)]
fn playing(c8: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_8) -> bool {
    let mut value = windows_core061::BOOL::default();
    unsafe { c8.IsDocumentPlayingAudio(&mut value) }.is_ok() && value.as_bool()
}
