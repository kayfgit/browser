//! WebView2 downloads, driven by the shell instead of Edge's own (hidden) download UI.
//!
//! A download starts PAUSED at a question: `DownloadStarting` takes a deferral, hides the
//! default UI and asks the shell (`DownloadEvent::Ask`). The shell answers with
//! [`answer`] — a save path, or no — and from then on gets progress, completion and
//! failure for it, and can [`cancel`] it. Everything here runs on the UI thread.
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tao::event_loop::EventLoopProxy;
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::{
    take_pwstr, BytesReceivedChangedEventHandler, DownloadStartingEventHandler,
    StateChangedEventHandler,
};
use windows_core061::{Interface, HSTRING, PWSTR};
use wry::{WebView, WebViewExtWindows};

use crate::downloads::DownloadEvent;
use crate::UserEvent;

/// How often a running download reports its progress at most.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// A download waiting for the shell's answer.
struct Asking {
    args: ICoreWebView2DownloadStartingEventArgs,
    deferral: ICoreWebView2Deferral,
    operation: ICoreWebView2DownloadOperation,
    proxy: EventLoopProxy<UserEvent>,
}

thread_local! {
    static ASKING: RefCell<HashMap<u64, Asking>> = RefCell::new(HashMap::new());
    static RUNNING: RefCell<HashMap<u64, ICoreWebView2DownloadOperation>> =
        RefCell::new(HashMap::new());
}

/// Route `webview`'s downloads through the shell (see the module docs). `proxy` is the
/// shell's own: downloads from background tabs must reach it too.
pub(crate) fn install(webview: &WebView, proxy: EventLoopProxy<UserEvent>) {
    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else {
        return;
    };
    let Ok(core4) = core.cast::<ICoreWebView2_4>() else {
        return;
    };
    let handler = DownloadStartingEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else {
            return Ok(());
        };
        let operation = unsafe { args.DownloadOperation() }?;
        let url = read(|p| unsafe { operation.Uri(p) });
        // WebView2's suggested path already carries the server's file name.
        let suggested = PathBuf::from(read(|p| unsafe { args.ResultFilePath(p) }));
        let name = crate::tabs::download_name(&url, &suggested);
        let mut total = 0i64;
        let _ = unsafe { operation.TotalBytesToReceive(&mut total) };
        // Edge's own download bubble stays hidden; the shell shows its own.
        unsafe { args.SetHandled(true) }?;
        let deferral = unsafe { args.GetDeferral() }?;
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        ASKING.with(|a| {
            a.borrow_mut().insert(
                id,
                Asking {
                    args: args.clone(),
                    deferral,
                    operation,
                    proxy: proxy.clone(),
                },
            )
        });
        let risky = crate::tabs::is_risky_download(&url, &suggested);
        send(
            &proxy,
            DownloadEvent::Ask {
                id,
                name,
                url,
                size: u64::try_from(total).ok().filter(|&n| n > 0),
                risky,
            },
        );
        Ok(())
    }));
    let mut token = 0i64;
    let _ = unsafe { core4.add_DownloadStarting(&handler, &mut token) };
}

/// The shell's answer to [`DownloadEvent::Ask`]: save to `path`, or cancel (`None`).
pub(crate) fn answer(id: u64, path: Option<&Path>) {
    let Some(asking) = ASKING.with(|a| a.borrow_mut().remove(&id)) else {
        return;
    };
    let Asking {
        args,
        deferral,
        operation,
        proxy,
    } = asking;
    match path {
        Some(path) => {
            let set = unsafe { args.SetResultFilePath(&HSTRING::from(path.as_os_str())) };
            if let Err(e) = set {
                let _ = unsafe { args.SetCancel(true) };
                let _ = unsafe { deferral.Complete() };
                send(
                    &proxy,
                    DownloadEvent::Failed {
                        id,
                        reason: e.message(),
                    },
                );
                return;
            }
            watch(id, &operation, proxy);
        }
        None => {
            let _ = unsafe { args.SetCancel(true) };
        }
    }
    let _ = unsafe { deferral.Complete() };
}

/// Stop a running download (or decline one still waiting for an answer).
pub(crate) fn cancel(id: u64) {
    if ASKING.with(|a| a.borrow().contains_key(&id)) {
        return answer(id, None);
    }
    if let Some(operation) = RUNNING.with(|r| r.borrow().get(&id).cloned()) {
        let _ = unsafe { operation.Cancel() };
    }
}

/// Report a running download's progress and its end.
fn watch(id: u64, operation: &ICoreWebView2DownloadOperation, proxy: EventLoopProxy<UserEvent>) {
    RUNNING.with(|r| r.borrow_mut().insert(id, operation.clone()));
    let last = Cell::new(None::<Instant>);
    let progress_proxy = proxy.clone();
    let progress = BytesReceivedChangedEventHandler::create(Box::new(move |op, _| {
        let Some(op) = op else { return Ok(()) };
        if last.get().is_some_and(|t| t.elapsed() < PROGRESS_EVERY) {
            return Ok(());
        }
        last.set(Some(Instant::now()));
        let (mut received, mut total) = (0i64, 0i64);
        let _ = unsafe { op.BytesReceived(&mut received) };
        let _ = unsafe { op.TotalBytesToReceive(&mut total) };
        send(
            &progress_proxy,
            DownloadEvent::Progress {
                id,
                received: u64::try_from(received).unwrap_or(0),
                total: u64::try_from(total).ok().filter(|&n| n > 0),
            },
        );
        Ok(())
    }));
    let state = StateChangedEventHandler::create(Box::new(move |op, _| {
        let Some(op) = op else { return Ok(()) };
        let mut state = COREWEBVIEW2_DOWNLOAD_STATE::default();
        let _ = unsafe { op.State(&mut state) };
        if state == COREWEBVIEW2_DOWNLOAD_STATE_COMPLETED {
            let path = PathBuf::from(read(|p| unsafe { op.ResultFilePath(p) }));
            RUNNING.with(|r| r.borrow_mut().remove(&id));
            send(&proxy, DownloadEvent::Done { id, path });
        } else if state == COREWEBVIEW2_DOWNLOAD_STATE_INTERRUPTED {
            let mut reason = COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON::default();
            let _ = unsafe { op.InterruptReason(&mut reason) };
            RUNNING.with(|r| r.borrow_mut().remove(&id));
            send(
                &proxy,
                DownloadEvent::Failed {
                    id,
                    reason: interrupt_reason(reason).to_string(),
                },
            );
        }
        Ok(())
    }));
    let mut token = 0i64;
    unsafe {
        let _ = operation.add_BytesReceivedChanged(&progress, &mut token);
        let _ = operation.add_StateChanged(&state, &mut token);
    }
}

fn send(proxy: &EventLoopProxy<UserEvent>, event: DownloadEvent) {
    let _ = proxy.send_event(UserEvent::Download(event));
}

fn read(f: impl FnOnce(*mut PWSTR) -> windows_core061::Result<()>) -> String {
    let mut p = PWSTR::null();
    if f(&mut p).is_ok() {
        take_pwstr(p)
    } else {
        String::new()
    }
}

/// Why WebView2 stopped a download, in a few words.
fn interrupt_reason(reason: COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON) -> &'static str {
    match reason {
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_CANCELED => "cancelled",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_USER_SHUTDOWN => "the browser closed",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_FILE_NO_SPACE => "the disk is full",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_FILE_ACCESS_DENIED => "no permission to write there",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_FILE_NAME_TOO_LONG => "the file name is too long",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_FILE_MALICIOUS
        | COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_FILE_SECURITY_CHECK_FAILED => {
            "Windows flagged it as unsafe"
        }
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_FILE_BLOCKED_BY_POLICY => "blocked by policy",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_NETWORK_DISCONNECTED
        | COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_NETWORK_FAILED
        | COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_NETWORK_TIMEOUT
        | COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_NETWORK_SERVER_DOWN => "the connection dropped",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_SERVER_FORBIDDEN
        | COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_SERVER_UNAUTHORIZED => "the server refused it",
        COREWEBVIEW2_DOWNLOAD_INTERRUPT_REASON_DOWNLOAD_PROCESS_CRASHED => "the download crashed",
        _ => "the download failed",
    }
}
