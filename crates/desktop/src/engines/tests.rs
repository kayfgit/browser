//! Exercise the shell boundary without starting an engine or opening a window.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::panes::PaneRect;
use crate::tabs::{engine_rect, nav_entry, NavKind, PageState, Tab, TabContent};
use browser_engine::{EngineResult, EngineView, RectPx};

struct FakeView {
    url: RefCell<String>,
    dropped: Rc<Cell<bool>>,
    bounds: Rc<Cell<Option<RectPx>>>,
}

impl Drop for FakeView {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

impl EngineView for FakeView {
    fn load_url(&self, url: &str) -> EngineResult {
        if url == "test:failure" {
            return Err("navigation rejected".into());
        }
        *self.url.borrow_mut() = url.to_string();
        Ok(())
    }
    fn url(&self) -> EngineResult<String> {
        Ok(self.url.borrow().clone())
    }
    fn set_bounds(&self, rect: RectPx) -> EngineResult {
        self.bounds.set(Some(rect));
        Ok(())
    }
    fn reload(&self) -> EngineResult {
        Ok(())
    }
    fn set_visible(&self, _: bool) -> EngineResult {
        Ok(())
    }
    fn zoom(&self, _: f64) -> EngineResult {
        Ok(())
    }
    fn focus(&self) -> EngineResult {
        Ok(())
    }
    fn focus_parent(&self) -> EngineResult {
        Ok(())
    }
    fn evaluate_script(&self, _: &str) -> EngineResult {
        Ok(())
    }
}

#[test]
fn shell_tab_owns_an_independent_engine_and_releases_it_on_replacement() {
    let dropped = Rc::new(Cell::new(false));
    let bounds = Rc::new(Cell::new(None));
    let mut tab = Tab::blank();
    assert!(tab.webview().is_none());
    assert!(tab.page_state().is_none());
    tab.content = TabContent::Web(
        Box::new(FakeView {
            url: RefCell::new("https://example.com".into()),
            dropped: dropped.clone(),
            bounds: bounds.clone(),
        }),
        PageState::default(),
    );

    // Use the same erased reference the shell's navigation and layout code uses.
    let view = tab.webview().unwrap();
    view.load_url("https://example.org").unwrap();
    assert_eq!(view.url().unwrap(), "https://example.org");
    assert_eq!(view.load_url("test:failure").unwrap_err(), "navigation rejected");
    assert_eq!(view.url().unwrap(), "https://example.org");
    view.set_bounds(engine_rect(PaneRect { x: 17, y: 29, w: 300, h: 200 })).unwrap();
    assert_eq!(bounds.get(), Some(RectPx { x: 17, y: 29, w: 300, h: 200 }));
    assert!(view.history().is_none());
    assert!(view.extensions().is_none());
    assert!(view.browsing_data().is_none());
    assert!(view.suspension().is_none());
    assert!(!dropped.get());

    tab.content = TabContent::Blank;
    assert!(dropped.get());
    assert!(tab.webview().is_none());
    assert!(tab.page_state().is_none());
}

#[test]
fn shell_history_keeps_private_and_research_flags_with_an_erased_engine() {
    let mut tab = Tab::blank();
    tab.url = "https://example.com".into();
    tab.private = true;
    tab.research = true;
    tab.content = TabContent::Web(
        Box::new(FakeView {
            url: RefCell::new(tab.url.clone()),
            dropped: Rc::new(Cell::new(false)),
            bounds: Rc::new(Cell::new(None)),
        }),
        PageState::default(),
    );
    let entry = nav_entry(&tab).unwrap();
    assert!(entry.private);
    assert!(entry.kind == NavKind::Research);
    assert_eq!(entry.url, tab.url);
}

#[test]
fn collapsed_pane_bounds_stay_valid_for_child_surfaces() {
    assert_eq!(
        engine_rect(PaneRect { x: -2, y: 10, w: 0, h: -4 }),
        RectPx { x: -2, y: 10, w: 1, h: 1 }
    );
}

#[test]
fn native_engine_access_stays_inside_the_provider() {
    fn check(dir: &std::path::Path, provider: &std::path::Path, test: &std::path::Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path == provider || path == test {
                continue;
            }
            if path.is_dir() {
                check(&path, provider, test);
                continue;
            }
            if path.extension().is_none_or(|ext| ext != "rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for line in source.lines().filter(|line| !line.trim_start().starts_with("//")) {
                // Restrict imports/paths, not explanatory references in comments.
                for native in ["wry::", "webview2_com::", "windows061::"] {
                    assert!(
                        !line.contains(native),
                        "native engine access outside adapter: {}: {line}",
                        path.display()
                    );
                }
            }
        }
    }
    // This file contains the forbidden tokens as test data, so scan the other files.
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    check(&src, &src.join("engines/webview2"), &src.join("engines/tests.rs"));
}
