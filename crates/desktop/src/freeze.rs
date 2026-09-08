//! Hide/resume web tabs through the optional engine suspension service.

use crate::App;

impl App {
    /// `:freeze` — hide and suspend every web tab so the browser holds the least RAM
    /// possible while staying open. Idempotent; reports if there are no web tabs to
    /// suspend (read/terminal/AI tabs are already engine-free).
    pub(crate) fn freeze(&mut self) {
        if self.frozen {
            self.set_status("already frozen — :unfreeze to resume");
            return;
        }
        let mut n = 0usize;
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                // Hide first: TrySuspend only suspends a non-visible webview.
                let _ = wv.set_visible(false);
                if let Some(service) = wv.suspension() {
                    let _ = service.suspend();
                }
                n += 1;
            }
        }
        self.frozen = true;
        // refresh_visibility honours `frozen` by keeping every webview hidden, and
        // the draw path paints the frozen notice over the (now empty) content band.
        self.refresh_visibility();
        if n == 0 {
            self.set_status("frozen — no web tabs to suspend (RAM already minimal)");
        } else {
            let plural = if n == 1 { "tab" } else { "tabs" };
            self.set_status(format!("frozen {n} web {plural} — :unfreeze to resume"));
        }
        self.window.request_redraw();
    }

    /// `:unfreeze` — resume every suspended web tab and return to normal rendering.
    pub(crate) fn unfreeze(&mut self) {
        if !self.frozen {
            self.set_status("not frozen");
            return;
        }
        for tab in &self.tabs {
            if let Some(wv) = tab.webview() {
                if let Some(service) = wv.suspension() {
                    let _ = service.resume();
                }
            }
        }
        self.frozen = false;
        self.refresh_visibility(); // re-shows the active web pane(s)
        self.set_status("unfrozen");
        self.window.request_redraw();
    }
}
