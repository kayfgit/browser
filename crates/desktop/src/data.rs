//! Shell routing for asynchronous profile-data operations.
use crate::UserEvent;
pub(crate) use browser_engine::DataKind;
use browser_engine::EngineView;
use tao::event_loop::EventLoopProxy;

pub(crate) fn clear(
    view: &dyn EngineView,
    kind: DataKind,
    range: Option<(f64, f64)>,
    label: String,
    ai_id: Option<u64>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    let service = view
        .browsing_data()
        .ok_or("this engine cannot clear browsing data")?;
    service.clear(
        kind,
        range,
        Box::new(move |result| {
            let _ = proxy.send_event(UserEvent::DataCleared {
                label,
                ai_id,
                result,
            });
        }),
    )
}

/// Like [`clear`], for one site only. `label` ends up in "cleared {label}"; a count
/// of the cookies removed is appended when the clear covered cookies.
pub(crate) fn clear_site(
    view: &dyn EngineView,
    kind: DataKind,
    site: &str,
    origins: Vec<String>,
    label: String,
    ai_id: Option<u64>,
    proxy: EventLoopProxy<UserEvent>,
) -> Result<(), String> {
    let service = view
        .browsing_data()
        .ok_or("this engine cannot clear browsing data")?;
    let counts_cookies = matches!(kind, DataKind::Cookies | DataKind::All);
    service.clear_site(
        kind,
        site,
        origins,
        Box::new(move |result| {
            let label = match &result {
                Ok(n) if counts_cookies => {
                    let plural = if *n == 1 { "cookie" } else { "cookies" };
                    format!("{label} — {n} {plural} removed")
                }
                _ => label,
            };
            let _ = proxy.send_event(UserEvent::DataCleared {
                label,
                ai_id,
                result: result.map(|_| ()),
            });
        }),
    )
}
