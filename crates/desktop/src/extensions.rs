//! Shell routing for optional engine extension services.
use crate::UserEvent;
use browser_engine::EngineView;
use tao::event_loop::EventLoopProxy;

pub(crate) fn list(view: &dyn EngineView, request: u64, proxy: EventLoopProxy<UserEvent>) {
    let source = view.identity().id;
    let callback_proxy = proxy.clone();
    let result = view
        .extensions()
        .ok_or_else(|| "this engine does not support extensions".to_string())
        .and_then(|service| {
            service.list(Box::new(move |result| {
                let _ = callback_proxy.send_event(UserEvent::ExtensionsListed {
                    request,
                    view: source,
                    result,
                });
            }))
        });
    if let Err(error) = result {
        let _ = proxy.send_event(UserEvent::ExtensionsListed {
            request,
            view: source,
            result: Err(error),
        });
    }
}

pub(crate) fn set_enabled(view: &dyn EngineView, id: String, enabled: bool) -> Result<(), String> {
    view.extensions()
        .ok_or("this engine does not support extensions")?
        .set_enabled(id, enabled)
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
