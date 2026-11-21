//! What every engine factory gets to build a view: the same options for each
//! provider, so the shell never needs to know which engine it is talking to.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use browser_engine::{RectPx, Source};
use tao::event_loop::EventLoopProxy;

use crate::UserEvent;

/// The options a tab's view is built with, whichever engine builds it.
pub(crate) struct BuildOptions<'a> {
    pub source: Source,
    pub disable_js: bool,
    pub extra_init: &'a str,
    pub storage: browser_engine::StorageMode,
    pub bounds: RectPx,
    pub adblock: bool,
    pub mute: bool,
    pub no_css: bool,
    pub no_video: bool,
    pub no_scrollbar: bool,
    pub proxy: EventLoopProxy<UserEvent>,
    pub adblock_on: Arc<AtomicBool>,
    pub blocker: crate::blocklist::SharedBlocker,
}
