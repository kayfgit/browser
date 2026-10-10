//! WebView2 permission requests (camera, microphone, location, notifications, "download
//! multiple files", …), answered from the shell's bar instead of Edge's own prompt — a
//! native bubble that hint mode and the keyboard can't reach.
//!
//! A request is held with a deferral while the shell asks (`UserEvent::PermissionAsk`);
//! [`answer`] allows or blocks it. Chromium remembers the answer for that site, like any
//! browser. Everything here runs on the UI thread.
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{take_pwstr, PermissionRequestedEventHandler};
use windows_core::PWSTR;
use wry::{WebView, WebViewExtWindows};

use crate::UserEvent;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static ASKING: RefCell<HashMap<u64, (ICoreWebView2PermissionRequestedEventArgs, ICoreWebView2Deferral)>> =
        RefCell::new(HashMap::new());
}

/// Route `webview`'s permission requests through the shell.
pub(crate) fn install(webview: &WebView, proxy: EventLoopProxy<UserEvent>) {
    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else {
        return;
    };
    let handler = PermissionRequestedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = COREWEBVIEW2_PERMISSION_KIND::default();
        unsafe { args.PermissionKind(&mut kind) }?;
        let mut p = PWSTR::null();
        unsafe { args.Uri(&mut p) }?;
        let url = take_pwstr(p);
        let deferral = unsafe { args.GetDeferral() }?;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        ASKING.with(|a| a.borrow_mut().insert(id, (args.clone(), deferral)));
        let _ = proxy.send_event(UserEvent::PermissionAsk {
            id,
            url,
            what: describe(kind).to_string(),
        });
        Ok(())
    }));
    let mut token = 0i64;
    let _ = unsafe { core.add_PermissionRequested(&handler, &mut token) };
}

/// The shell's answer to [`UserEvent::PermissionAsk`].
pub(crate) fn answer(id: u64, allow: bool) {
    let Some((args, deferral)) = ASKING.with(|a| a.borrow_mut().remove(&id)) else {
        return;
    };
    let state = if allow {
        COREWEBVIEW2_PERMISSION_STATE_ALLOW
    } else {
        COREWEBVIEW2_PERMISSION_STATE_DENY
    };
    let _ = unsafe { args.SetState(state) };
    let _ = unsafe { deferral.Complete() };
}

/// What a permission lets the site do, finishing "`<site>` wants to …".
fn describe(kind: COREWEBVIEW2_PERMISSION_KIND) -> &'static str {
    match kind {
        COREWEBVIEW2_PERMISSION_KIND_MICROPHONE => "use your microphone",
        COREWEBVIEW2_PERMISSION_KIND_CAMERA => "use your camera",
        COREWEBVIEW2_PERMISSION_KIND_GEOLOCATION => "know your location",
        COREWEBVIEW2_PERMISSION_KIND_NOTIFICATIONS => "show notifications",
        COREWEBVIEW2_PERMISSION_KIND_OTHER_SENSORS => "use your device's sensors",
        COREWEBVIEW2_PERMISSION_KIND_CLIPBOARD_READ => "read your clipboard",
        COREWEBVIEW2_PERMISSION_KIND_MULTIPLE_AUTOMATIC_DOWNLOADS => "download multiple files",
        COREWEBVIEW2_PERMISSION_KIND_FILE_READ_WRITE => "read and edit files on your computer",
        COREWEBVIEW2_PERMISSION_KIND_AUTOPLAY => "play media automatically",
        COREWEBVIEW2_PERMISSION_KIND_LOCAL_FONTS => "use the fonts installed on your computer",
        COREWEBVIEW2_PERMISSION_KIND_MIDI_SYSTEM_EXCLUSIVE_MESSAGES => "control your MIDI devices",
        COREWEBVIEW2_PERMISSION_KIND_WINDOW_MANAGEMENT => "manage windows on your screens",
        _ => "use a browser feature",
    }
}
