//! WebView2 process crashes. Pages run in Edge's own processes, so a crash never takes
//! the browser with it, but without this a pane whose engine died just went black, with
//! nothing to reload. A dead page or engine process now gives the pane the same crash
//! page as the other engines (`UserEvent::EngineCrashed`), and `:reload` builds a new
//! view, starting a new engine if the old one is gone.
use webview2_com::Microsoft::Web::WebView2::Win32::*;
use webview2_com::ProcessFailedEventHandler;
use wry::{WebView, WebViewExtWindows};

use super::super::PageEventProxy;
use crate::UserEvent;

/// Report `webview`'s process failures to the shell: a fatal one gets the crash page,
/// the rest (a GPU or helper process that restarted, a page that stopped responding,
/// a crashed frame inside it) are logged as warnings for `:warn`.
pub(crate) fn install(webview: &WebView, proxy: PageEventProxy) {
    let Ok(core) = (unsafe { webview.controller().CoreWebView2() }) else {
        return;
    };
    let handler = ProcessFailedEventHandler::create(Box::new(move |_, args| {
        let Some(args) = args else { return Ok(()) };
        let mut kind = COREWEBVIEW2_PROCESS_FAILED_KIND::default();
        unsafe { args.ProcessFailedKind(&mut kind) }?;
        let _ = proxy.send_event(match describe(kind) {
            Some(reason) => UserEvent::EngineCrashed(reason.into()),
            None => UserEvent::EngineWarning(warning(kind).into()),
        });
        Ok(())
    }));
    let mut token = 0i64;
    let _ = unsafe { core.add_ProcessFailed(&handler, &mut token) };
}

/// What a failure means for the pane, or `None` for those WebView2 recovers from by
/// itself (a GPU or utility process restarts; a frozen page may come back).
fn describe(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> Option<&'static str> {
    match kind {
        COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED => {
            Some("the WebView2 engine stopped")
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED => {
            Some("the page's process stopped")
        }
        _ => None,
    }
}

/// What a failure WebView2 recovers from by itself means, for the `:warn` log.
fn warning(kind: COREWEBVIEW2_PROCESS_FAILED_KIND) -> &'static str {
    match kind {
        COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED => "the GPU process restarted",
        COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE => {
            "the page stopped responding"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_FRAME_RENDER_PROCESS_EXITED => {
            "a frame inside the page crashed"
        }
        COREWEBVIEW2_PROCESS_FAILED_KIND_UTILITY_PROCESS_EXITED => {
            "an engine helper process (audio, network, …) restarted"
        }
        _ => "an engine helper process exited",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_dead_pages_and_engines_get_the_crash_page() {
        assert!(describe(COREWEBVIEW2_PROCESS_FAILED_KIND_BROWSER_PROCESS_EXITED).is_some());
        assert!(describe(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_EXITED).is_some());
        assert!(describe(COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED).is_none());
        assert!(describe(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE).is_none());
    }

    #[test]
    fn recoverable_failures_read_as_warnings() {
        assert_eq!(
            warning(COREWEBVIEW2_PROCESS_FAILED_KIND_GPU_PROCESS_EXITED),
            "the GPU process restarted"
        );
        assert_eq!(
            warning(COREWEBVIEW2_PROCESS_FAILED_KIND_RENDER_PROCESS_UNRESPONSIVE),
            "the page stopped responding"
        );
    }
}
