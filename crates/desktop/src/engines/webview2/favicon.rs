//! Native favicon acquisition.
use crate::favicon::{decode_png, SharedIcon};
use std::sync::Arc;

/// Ask WebView2 for this webview's current favicon, and for every later change.
///
/// Best-effort, exactly like the other COM glue: an engine handle we can't get, an
/// interface the installed runtime is too old for, or a failed registration simply
/// means this tab shows no icon. Each delivery decodes into `slot` and posts
/// [`UserEvent::Redraw`](crate::UserEvent::Redraw) so the strip repaints.
#[cfg(windows)]
pub(crate) fn install(
    webview: &wry::WebView,
    slot: SharedIcon,
    proxy: crate::engines::PageEventProxy,
) {
    use webview2_com::FaviconChangedEventHandler;
    use webview2_com::Microsoft::Web::WebView2::Win32::{ICoreWebView2, ICoreWebView2_15};
    use windows_core061::Interface;
    use wry::WebViewExtWindows;

    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else {
        return;
    };
    let Ok(c15) = core.cast::<ICoreWebView2_15>() else {
        return;
    };
    // The page may already have one (a restored/cached document fires no change event).
    fetch(&c15, &slot, &proxy);
    let handler =
        FaviconChangedEventHandler::create(Box::new(move |sender: Option<ICoreWebView2>, _| {
            if let Some(c15) = sender.and_then(|s| s.cast::<ICoreWebView2_15>().ok()) {
                fetch(&c15, &slot, &proxy);
            }
            Ok(())
        }));
    let mut token = 0i64;
    let _ = unsafe { c15.add_FaviconChanged(&handler, &mut token) };
}

/// One favicon read: clear the slot when the page declares no icon, else pull the PNG
/// and decode it. Async — the completion handler runs later on the UI thread.
#[cfg(windows)]
fn fetch(
    c15: &webview2_com::Microsoft::Web::WebView2::Win32::ICoreWebView2_15,
    slot: &SharedIcon,
    proxy: &crate::engines::PageEventProxy,
) {
    use webview2_com::Microsoft::Web::WebView2::Win32::COREWEBVIEW2_FAVICON_IMAGE_FORMAT_PNG;
    use webview2_com::{take_pwstr, GetFaviconCompletedHandler};
    use windows_core061::PWSTR;

    // No declared icon → drop the previous page's, rather than leaving it on a site
    // that has none.
    let uri = unsafe {
        let mut p = PWSTR::null();
        if c15.FaviconUri(&mut p).is_err() {
            return;
        }
        take_pwstr(p)
    };
    if uri.is_empty() {
        if let Ok(mut g) = slot.lock() {
            if g.take().is_some() {
                let _ = proxy.send_event(crate::UserEvent::Redraw);
            }
        }
        return;
    }
    let (slot, proxy) = (slot.clone(), proxy.clone());
    let handler = GetFaviconCompletedHandler::create(Box::new(move |hr, stream| {
        let Some(icon) = hr
            .is_ok()
            .then_some(stream)
            .flatten()
            .and_then(|s| decode_png(&read_stream(&s)))
        else {
            return Ok(());
        };
        if let Ok(mut g) = slot.lock() {
            *g = Some(Arc::new(icon));
        }
        let _ = proxy.send_event(crate::UserEvent::Redraw);
        Ok(())
    }));
    let _ = unsafe { c15.GetFavicon(COREWEBVIEW2_FAVICON_IMAGE_FORMAT_PNG, &handler) };
}

/// Drain a COM stream into a byte vector. Capped so a hostile/huge "icon" can't be
/// read into memory without bound — real favicons are a few KB.
#[cfg(windows)]
fn read_stream(stream: &windows061::Win32::System::Com::IStream) -> Vec<u8> {
    const CAP: usize = 1 << 20;
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let mut read = 0u32;
        let hr = unsafe {
            stream.Read(
                chunk.as_mut_ptr() as *mut core::ffi::c_void,
                chunk.len() as u32,
                Some(&mut read),
            )
        };
        if hr.is_err() || read == 0 {
            break;
        }
        out.extend_from_slice(&chunk[..read as usize]);
        if out.len() >= CAP {
            break;
        }
    }
    out
}
