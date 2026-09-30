//! `:inspect` (F12) and `:source` (Ctrl+Shift+U): a page's developer tools and its
//! source, also offered by the page's right-click menu.

use crate::App;

impl App {
    /// Open the developer tools for the active page.
    pub(crate) fn inspect_active(&mut self) {
        match self.active {
            Some(i) if self.tabs[i].webview().is_some() => self.inspect_tab(i),
            _ => self.set_status("no web page to inspect"),
        }
    }

    pub(crate) fn inspect_tab(&mut self, index: usize) {
        let Some(view) = self.tabs.get(index).and_then(|t| t.webview()) else {
            return;
        };
        if let Err(error) = view.open_devtools() {
            self.set_error(format!("can't open developer tools: {error}"));
        }
    }

    /// Open the active page's source in a new tab.
    pub(crate) fn view_source_active(&mut self) {
        match self.active {
            Some(i) if self.tabs[i].webview().is_some() => self.view_source_of(i),
            _ => self.set_status("no web page to view the source of"),
        }
    }

    /// Open tab `index`'s source in a new tab (`view-source:<url>`), with the same
    /// engine and privacy as the page.
    pub(crate) fn view_source_of(&mut self, index: usize) {
        let Some(tab) = self.tabs.get(index) else {
            return;
        };
        let Some(view) = tab.webview() else { return };
        let url = view
            .url()
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| tab.url.clone());
        let url = if url.starts_with("view-source:") {
            url
        } else {
            format!("view-source:{url}")
        };
        let provider = view.identity().provider.clone();
        let private = tab.private;
        self.open_web_provider(&url, false, false, true, private, &provider);
    }
}
