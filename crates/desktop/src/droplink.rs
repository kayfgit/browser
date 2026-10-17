//! Links dragged into the browser from anywhere (another browser, a chat app, a
//! file manager's address bar) open in a new tab on the default engine.
//!
//! Two halves cover the window. Web panes are WebView2's own drop targets, so
//! `bridge.js` handles drops there: a link the page doesn't take for itself (an
//! upload zone, a text field) is sent here as `drop-link:` instead of replacing the
//! page. Everywhere else — the tab strip or sidebar, the welcome screen, terminal and
//! read panes — this module's OLE drop target on the main window takes the drop. It
//! replaces tao's, which accepts only files (nothing here uses dropped files).

use tao::event_loop::EventLoopProxy;
use windows::core::{implement, w, Ref};
use windows::Win32::Foundation::{HWND, POINTL};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::DataExchange::RegisterClipboardFormatW;
use windows::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows::Win32::System::Ole::{
    IDropTarget, IDropTarget_Impl, RegisterDragDrop, ReleaseStgMedium, RevokeDragDrop,
    CF_UNICODETEXT, DROPEFFECT, DROPEFFECT_LINK, DROPEFFECT_NONE,
};
use windows::Win32::System::SystemServices::MODIFIERKEYS_FLAGS;

use crate::UserEvent;

/// The address in dragged text, if it is one: its first line when that's a web
/// address (`https://…`, or a bare `example.com/page`). Words and sentences aren't
/// links, so they're refused rather than searched.
pub(crate) fn link_from_text(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    if line.starts_with("javascript:") || browser_core::looks_like_query(line) {
        return None;
    }
    let url = browser_core::normalize_url(line)?;
    matches!(url.split(':').next(), Some("http" | "https" | "file")).then_some(url)
}

/// Put [`DropTarget`] on the main window in place of tao's files-only one. tao has
/// already initialized OLE on this thread (it does so to register its own).
pub(crate) fn install(hwnd: HWND, proxy: EventLoopProxy<UserEvent>) -> windows::core::Result<()> {
    let target: IDropTarget = DropTarget { proxy }.into();
    unsafe {
        // Nothing to revoke when tao didn't register one; that's fine.
        let _ = RevokeDragDrop(hwnd);
        RegisterDragDrop(hwnd, &target)
    }
}

#[implement(IDropTarget)]
struct DropTarget {
    proxy: EventLoopProxy<UserEvent>,
}

impl DropTarget {
    /// The link a drag carries: its URL format (what browsers put on a dragged link)
    /// or, failing that, its text if that's an address.
    fn link(data: &IDataObject) -> Option<String> {
        let url_format = unsafe { RegisterClipboardFormatW(w!("UniformResourceLocatorW")) };
        [url_format as u16, CF_UNICODETEXT.0]
            .into_iter()
            .filter(|&f| f != 0)
            .find_map(|format| read_text(data, format).and_then(|t| link_from_text(&t)))
    }
}

/// The UTF-16 text `data` holds in clipboard `format`, if any.
fn read_text(data: &IDataObject, format: u16) -> Option<String> {
    let request = FORMATETC {
        cfFormat: format,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    unsafe {
        let mut medium = data.GetData(&request).ok()?;
        let global = medium.u.hGlobal;
        let ptr = GlobalLock(global) as *const u16;
        let text = (!ptr.is_null()).then(|| {
            let units = GlobalSize(global) / 2;
            let wide = std::slice::from_raw_parts(ptr, units);
            let end = wide.iter().position(|&c| c == 0).unwrap_or(units);
            String::from_utf16_lossy(&wide[..end])
        });
        let _ = GlobalUnlock(global);
        ReleaseStgMedium(&mut medium);
        text
    }
}

/// Answer a drag: accept it (as a link) only if it carries one.
fn answer(effect: *mut DROPEFFECT, link: bool) {
    if !effect.is_null() {
        unsafe {
            *effect = if link {
                DROPEFFECT_LINK
            } else {
                DROPEFFECT_NONE
            }
        };
    }
}

impl IDropTarget_Impl for DropTarget_Impl {
    fn DragEnter(
        &self,
        data: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        answer(effect, data.as_ref().and_then(DropTarget::link).is_some());
        Ok(())
    }

    fn DragOver(
        &self,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        // DragEnter already said whether this drag carries a link; keep that answer.
        if !effect.is_null() && unsafe { *effect } != DROPEFFECT_NONE {
            answer(effect, true);
        }
        Ok(())
    }

    fn DragLeave(&self) -> windows::core::Result<()> {
        Ok(())
    }

    fn Drop(
        &self,
        data: Ref<IDataObject>,
        _: MODIFIERKEYS_FLAGS,
        _: &POINTL,
        effect: *mut DROPEFFECT,
    ) -> windows::core::Result<()> {
        let link = data.as_ref().and_then(DropTarget::link);
        answer(effect, link.is_some());
        if let Some(url) = link {
            let _ = self.proxy.send_event(UserEvent::DroppedLink(url));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::link_from_text;

    #[test]
    fn dragged_text_is_a_link_only_when_it_is_an_address() {
        assert_eq!(
            link_from_text("https://example.com/a?b=c").as_deref(),
            Some("https://example.com/a?b=c")
        );
        // Bare addresses get a scheme; only the first line counts.
        assert_eq!(
            link_from_text("\n  github.com/kayfgit\nnotes").as_deref(),
            Some("https://github.com/kayfgit")
        );
        // Words, sentences and scripts aren't links.
        assert_eq!(link_from_text("hello"), None);
        assert_eq!(link_from_text("open this please"), None);
        assert_eq!(link_from_text("javascript:alert(1)"), None);
        assert_eq!(link_from_text(""), None);
    }
}
