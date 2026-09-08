//! Shell routing for optional engine extension services.
use crate::UserEvent;
use browser_engine::EngineView;
use tao::event_loop::EventLoopProxy;

pub(crate) fn list(view: &dyn EngineView, proxy: EventLoopProxy<UserEvent>) {
    let callback_proxy = proxy.clone();
    let result = view
        .extensions()
        .ok_or_else(|| "this engine does not support extensions".to_string())
        .and_then(|service| {
            service.list(Box::new(move |result| {
                let event = match result {
                    Ok(items) => UserEvent::ExtensionsListed(items),
                    Err(error) => UserEvent::EngineOperationFailed(error),
                };
                let _ = callback_proxy.send_event(event);
            }))
        });
    if let Err(error) = result {
        let _ = proxy.send_event(UserEvent::EngineOperationFailed(error));
    }
}

pub(crate) fn set_enabled(view: &dyn EngineView, id: String, enabled: bool) {
    if let Some(service) = view.extensions() {
        let _ = service.set_enabled(id, enabled);
    }
}
pub(crate) fn set_all_enabled(view: &dyn EngineView, enabled: bool) {
    if let Some(service) = view.extensions() {
        let _ = service.set_all_enabled(enabled);
    }
}
pub(crate) fn install_dir(view: &dyn EngineView, dir: &std::path::Path) {
    if let Some(service) = view.extensions() {
        let _ = service.install_dir(dir);
    }
}
