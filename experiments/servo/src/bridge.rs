//! Replaceable public-API transport for Servo 0.5, which has no native IPC callback.
//! The native navigation epoch scopes replies; the page token scopes acknowledgements.
use super::Message;
use browser_servo_lab::bridge_protocol::Receiver;
use servo::{JSValue, LoadStatus, WebView};
use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant},
};
use tao::event_loop::EventLoopProxy;

pub const PRELUDE: &str = include_str!("../../../crates/desktop/src/engines/servo/bridge.js");
pub const SHELL: &str = include_str!("../../../crates/desktop/scripts/bridge.js");
pub const HINTS: &str = include_str!("../../../crates/desktop/scripts/hints.js");

#[derive(Default)]
pub struct Navigation {
    pub epoch: Cell<u64>,
    ready: Cell<bool>,
}
impl Navigation {
    pub fn status(&self, status: LoadStatus) {
        match status {
            LoadStatus::Started => {
                self.epoch.set(
                    self.epoch
                        .get()
                        .checked_add(1)
                        .expect("navigation epoch exhausted"),
                );
                self.ready.set(false);
            }
            LoadStatus::HeadParsed | LoadStatus::Complete => self.ready.set(true),
        }
    }
}

pub struct Bridge {
    navigation: Rc<Navigation>,
    receiver: Receiver,
    pending: Option<(u64, Instant)>,
    due: Instant,
    faulted: bool,
    period: Duration,
}
impl Bridge {
    pub fn new(navigation: Rc<Navigation>) -> Self {
        Self {
            navigation,
            receiver: Receiver::default(),
            pending: None,
            due: Instant::now(),
            faulted: false,
            period: Duration::from_millis(16),
        }
    }
    pub fn wakeup(&self) -> Instant {
        if self.pending.is_some() || self.faulted || !self.navigation.ready.get() {
            Instant::now() + Duration::from_millis(250)
        } else {
            self.due
        }
    }
    pub fn epoch(&self) -> u64 {
        self.navigation.epoch.get()
    }
    pub fn tick(&mut self, view: &WebView, proxy: &EventLoopProxy<Message>, focused: bool) {
        let now = Instant::now();
        self.period = Duration::from_millis(if focused { 16 } else { 100 });
        if self.receiver.epoch != self.navigation.epoch.get() {
            self.receiver.reset(self.navigation.epoch.get());
            self.pending = None;
            self.faulted = false;
            self.due = now;
        }
        if !self.faulted
            && self
                .pending
                .is_some_and(|(_, at)| now.duration_since(at) > Duration::from_secs(10))
        {
            self.faulted = true;
            let _ = proxy.send_event(Message::BridgeFault {
                epoch: self.receiver.epoch,
                error: "Servo shell bridge timed out; reload to retry".into(),
            });
        }
        if self.faulted || self.pending.is_some() || !self.navigation.ready.get() || now < self.due
        {
            return;
        }
        let epoch = self.receiver.epoch;
        self.pending = Some((epoch, now));
        let proxy = proxy.clone();
        view.evaluate_javascript(self.receiver.script(), move |result| {
            let result = match result {
                Ok(JSValue::String(text)) => Ok(text),
                other => Err(format!("bridge evaluation: {other:?}")),
            };
            let _ = proxy.send_event(Message::BridgeReply { epoch, result });
        });
    }
    pub fn reply(
        &mut self,
        epoch: u64,
        result: Result<String, String>,
    ) -> Result<Vec<String>, String> {
        if self.pending.as_ref().map(|p| p.0) != Some(epoch) {
            return Ok(Vec::new());
        }
        self.pending = None;
        self.due = Instant::now() + self.period;
        let current = self.navigation.epoch.get();
        if self.receiver.epoch != current {
            self.receiver.reset(current);
        }
        if self.faulted || epoch != current || !self.navigation.ready.get() {
            return Ok(Vec::new());
        }
        let accepted = result.and_then(|raw| self.receiver.accept(epoch, &raw));
        if accepted.is_err() {
            self.faulted = true;
        }
        accepted
    }
}
