//! WebView2 profile data clearing.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::visited::in_site;
use browser_engine::{Completion, DataKind};
use webview2_com::Microsoft::Web::WebView2::Win32::{
    ICoreWebView2, ICoreWebView2Profile2, ICoreWebView2_13, COREWEBVIEW2_BROWSING_DATA_KINDS,
    COREWEBVIEW2_BROWSING_DATA_KINDS_ALL_PROFILE,
    COREWEBVIEW2_BROWSING_DATA_KINDS_BROWSING_HISTORY,
    COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE, COREWEBVIEW2_BROWSING_DATA_KINDS_COOKIES,
    COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE, COREWEBVIEW2_BROWSING_DATA_KINDS_DOWNLOAD_HISTORY,
};
use webview2_com::{CallDevToolsProtocolMethodCompletedHandler, ClearBrowsingDataCompletedHandler};
use windows_core061::{Interface, HSTRING};
use wry::{WebView, WebViewExtWindows};

fn flags(kind: DataKind) -> COREWEBVIEW2_BROWSING_DATA_KINDS {
    let bits = |a: COREWEBVIEW2_BROWSING_DATA_KINDS, b: COREWEBVIEW2_BROWSING_DATA_KINDS| {
        COREWEBVIEW2_BROWSING_DATA_KINDS(a.0 | b.0)
    };
    match kind {
        DataKind::History => bits(
            COREWEBVIEW2_BROWSING_DATA_KINDS_BROWSING_HISTORY,
            COREWEBVIEW2_BROWSING_DATA_KINDS_DOWNLOAD_HISTORY,
        ),
        DataKind::Cookies => COREWEBVIEW2_BROWSING_DATA_KINDS_COOKIES,
        DataKind::Cache => bits(
            COREWEBVIEW2_BROWSING_DATA_KINDS_DISK_CACHE,
            COREWEBVIEW2_BROWSING_DATA_KINDS_CACHE_STORAGE,
        ),
        DataKind::All => COREWEBVIEW2_BROWSING_DATA_KINDS_ALL_PROFILE,
    }
}
pub(crate) fn clear(
    webview: &WebView,
    kind: DataKind,
    range: Option<(f64, f64)>,
    done: Completion,
) -> Result<(), String> {
    unsafe {
        let core = webview
            .controller()
            .CoreWebView2()
            .map_err(|e| e.to_string())?;
        let core13: ICoreWebView2_13 = core
            .cast()
            .map_err(|_| "this WebView2 runtime is too old to clear browsing data".to_string())?;
        let profile = core13.Profile().map_err(|e| e.to_string())?;
        let profile2: ICoreWebView2Profile2 = profile
            .cast()
            .map_err(|e: windows_core061::Error| e.to_string())?;
        let handler = ClearBrowsingDataCompletedHandler::create(Box::new(move |hr| {
            done(hr.map_err(|e| e.to_string()));
            Ok(())
        }));
        match range {
            Some((start, end)) => profile2
                .ClearBrowsingDataInTimeRange(flags(kind), start, end, &handler)
                .map_err(|e| e.to_string())?,
            None => profile2
                .ClearBrowsingData(flags(kind), &handler)
                .map_err(|e| e.to_string())?,
        }
    }
    Ok(())
}

/// Storage types `Storage.clearDataForOrigin` wipes for a one-site `Cache` clear: the
/// caches a site's own scripts fill. Chromium's HTTP disk cache has no per-origin
/// clear, so that part of `cache` stays profile-wide only.
const SITE_CACHE_TYPES: &str = "cache_storage,service_workers";

/// Erase one site's data through the DevTools protocol, which (unlike the profile
/// API above) can target origins. Cookies are listed and the ones whose domain is
/// `site` or under it are deleted one by one, so `.reddit.com` and `old.reddit.com`
/// go but `notreddit.com` stays. Storage is cleared per origin (scheme, host and
/// port must all match): http(s) on `site`, `www.` and each cookie's host, plus the
/// shell's `origins`, which carry any non-default port.
pub(crate) fn clear_site(
    webview: &WebView,
    kind: DataKind,
    site: &str,
    origins: Vec<String>,
    done: Completion<usize>,
) -> Result<(), String> {
    let core = unsafe { webview.controller().CoreWebView2() }.map_err(|e| e.to_string())?;
    if matches!(kind, DataKind::History) {
        done(Ok(0));
        return Ok(());
    }
    let site = site.to_string();
    let next = core.clone();
    cdp(&core, "Network.getAllCookies", "{}".into(), move |result| {
        let cookies = match result.and_then(|json| site_cookies(&json, &site)) {
            Ok(c) => c,
            Err(e) => return done(Err(e)),
        };
        let removed = cookies.len();
        let join = Join::new(move |r| done(r.map(|()| removed)));
        if matches!(kind, DataKind::Cookies | DataKind::All) {
            for c in &cookies {
                let params =
                    serde_json::json!({"name": c.name, "domain": c.domain, "path": c.path});
                join.run(&next, "Network.deleteCookies", params.to_string());
            }
        }
        if matches!(kind, DataKind::Cache | DataKind::All) {
            let types = if matches!(kind, DataKind::All) {
                "all"
            } else {
                SITE_CACHE_TYPES
            };
            let mut hosts: Vec<String> = vec![site.clone(), format!("www.{site}")];
            hosts.extend(
                cookies
                    .iter()
                    .map(|c| c.domain.trim_start_matches('.').to_ascii_lowercase()),
            );
            let mut all = origins;
            for host in hosts.iter().filter(|h| in_site(h, &site)) {
                all.extend(["https", "http"].map(|scheme| format!("{scheme}://{host}")));
            }
            all.sort();
            all.dedup();
            for origin in all {
                let params = serde_json::json!({"origin": origin, "storageTypes": types});
                join.run(&next, "Storage.clearDataForOrigin", params.to_string());
            }
        }
        join.finish();
    })
}

struct Cookie {
    name: String,
    domain: String,
    path: String,
}

/// The cookies in a `Network.getAllCookies` result that belong to `site`.
fn site_cookies(json: &str, site: &str) -> Result<Vec<Cookie>, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| e.to_string())?;
    let list = v["cookies"]
        .as_array()
        .ok_or("the engine returned no cookie list")?;
    let field = |c: &serde_json::Value, k: &str| c[k].as_str().unwrap_or("").to_string();
    Ok(list
        .iter()
        .map(|c| Cookie {
            name: field(c, "name"),
            domain: field(c, "domain"),
            path: field(c, "path"),
        })
        .filter(|c| in_site(&c.domain.trim_start_matches('.').to_ascii_lowercase(), site))
        .collect())
}

/// Call one DevTools method; `then` gets its JSON result or the failure.
fn cdp(
    core: &ICoreWebView2,
    method: &str,
    params: String,
    then: impl FnOnce(Result<String, String>) + 'static,
) -> Result<(), String> {
    let handler = CallDevToolsProtocolMethodCompletedHandler::create(Box::new(move |hr, json| {
        then(hr.map(|()| json).map_err(|e| e.to_string()));
        Ok(())
    }));
    let method = HSTRING::from(method);
    let params = HSTRING::from(params);
    unsafe { core.CallDevToolsProtocolMethod(&method, &params, &handler) }
        .map_err(|e| e.to_string())
}

/// Waits for a batch of DevTools calls and reports once, with the first failure if
/// any. Completions run on the UI thread, so `Rc`. One slot is held until
/// [`Join::finish`], so the report can't fire while calls are still being sent.
struct Join(Rc<JoinState>);

struct JoinState {
    pending: Cell<usize>,
    error: RefCell<Option<String>>,
    done: RefCell<Option<Completion>>,
}

impl Join {
    fn new(done: impl FnOnce(Result<(), String>) + 'static) -> Self {
        Join(Rc::new(JoinState {
            pending: Cell::new(1),
            error: RefCell::new(None),
            done: RefCell::new(Some(Box::new(done))),
        }))
    }

    fn run(&self, core: &ICoreWebView2, method: &str, params: String) {
        let state = self.0.clone();
        state.pending.set(state.pending.get() + 1);
        let sent = cdp(core, method, params, {
            let state = state.clone();
            move |r| JoinState::settle(&state, r.map(|_| ()))
        });
        if let Err(e) = sent {
            JoinState::settle(&state, Err(e));
        }
    }

    fn finish(self) {
        JoinState::settle(&self.0, Ok(()));
    }
}

impl JoinState {
    fn settle(state: &Rc<JoinState>, result: Result<(), String>) {
        if let Err(e) = result {
            state.error.borrow_mut().get_or_insert(e);
        }
        state.pending.set(state.pending.get() - 1);
        if state.pending.get() == 0 {
            if let Some(done) = state.done.borrow_mut().take() {
                done(match state.error.borrow_mut().take() {
                    Some(e) => Err(e),
                    None => Ok(()),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_cookies_keep_only_that_sites_domains() {
        let json = r#"{"cookies":[
            {"name":"a","domain":".reddit.com","path":"/"},
            {"name":"b","domain":"old.reddit.com","path":"/r"},
            {"name":"c","domain":"notreddit.com","path":"/"},
            {"name":"d","domain":".google.com","path":"/"}
        ]}"#;
        let got = site_cookies(json, "reddit.com").unwrap();
        let names: Vec<&str> = got.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(got[1].path, "/r");
        assert!(site_cookies("{}", "reddit.com").is_err());
    }
}
