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
/// Bring the bundled extensions under `dir` to `enabled` in `view`'s storage context (see
/// [`Extensions::sync_bundled`](browser_engine::Extensions::sync_bundled)). `done` runs on
/// the UI thread with the outcome — also when the engine has no extension support or
/// the request couldn't be dispatched, so a caller waiting on it is never left hanging.
pub(crate) fn sync_bundled(
    view: &dyn EngineView,
    dir: &std::path::Path,
    enabled: bool,
    done: browser_engine::Completion,
) {
    let Some(service) = view.extensions() else {
        return done(Ok(()));
    };
    // A dispatch error means the engine will never call `done`; hand it one ourselves.
    let done = std::rc::Rc::new(std::cell::RefCell::new(Some(done)));
    let engine_done = done.clone();
    // Never a refresh: that's for the builds that just unpacked new files.
    let result = service.sync_bundled(
        dir,
        enabled,
        false,
        Box::new(move |result| {
            if let Some(done) = engine_done.borrow_mut().take() {
                done(result);
            }
        }),
    );
    if let Err(error) = result {
        if let Some(done) = done.borrow_mut().take() {
            done(Err(error));
        }
    }
}
