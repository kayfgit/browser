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
