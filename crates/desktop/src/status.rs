//! The command bar's status message: its text, how it's painted, and when it clears.
//! Every message clears itself after [`STATUS_TIMEOUT`], so the bar never keeps
//! stale text; the event loop wakes at [`Status::clear_at`] to clear it.

use std::time::{Duration, Instant};

use crate::draw::{self, Rgb};

/// How long a status message stays up.
pub(crate) const STATUS_TIMEOUT: Duration = Duration::from_secs(3);

/// How a status message is painted.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) enum Tone {
    /// Ordinary information, dim.
    #[default]
    Info,
    /// A failure or warning, red.
    Error,
    /// A one-off colour, e.g. the `:ai` answer in purple.
    Color(Rgb),
}

#[derive(Default)]
pub(crate) struct Status {
    text: String,
    tone: Tone,
    clear_at: Option<Instant>,
}

impl Status {
    /// Show `text` from `now` until [`STATUS_TIMEOUT`] has passed.
    pub(crate) fn show(&mut self, text: String, tone: Tone, now: Instant) {
        self.text = text;
        self.tone = tone;
        self.clear_at = Some(now + STATUS_TIMEOUT);
    }

    pub(crate) fn clear(&mut self) {
        *self = Self::default();
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn color(&self) -> Rgb {
        match self.tone {
            Tone::Info => draw::DIM,
            Tone::Error => draw::ERR,
            Tone::Color(c) => c,
        }
    }

    /// When the current message clears itself; `None` when there's none.
    pub(crate) fn clear_at(&self) -> Option<Instant> {
        self.clear_at
    }

    /// Whether the current message is due to clear at `now`.
    pub(crate) fn expired(&self, now: Instant) -> bool {
        self.clear_at.is_some_and(|t| now >= t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_expires_after_the_timeout() {
        let now = Instant::now();
        let mut s = Status::default();
        assert!(!s.expired(now));
        s.show("saved".into(), Tone::Info, now);
        assert_eq!(s.text(), "saved");
        assert_eq!(s.color(), draw::DIM);
        assert!(!s.expired(now));
        assert!(s.expired(now + STATUS_TIMEOUT));
        s.clear();
        assert_eq!(s.text(), "");
        assert_eq!(s.clear_at(), None);
    }

    #[test]
    fn tone_picks_the_colour() {
        let mut s = Status::default();
        s.show("no".into(), Tone::Error, Instant::now());
        assert_eq!(s.color(), draw::ERR);
        s.show("ai".into(), Tone::Color(draw::AI), Instant::now());
        assert_eq!(s.color(), draw::AI);
    }
}
