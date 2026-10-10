//! The yes/no questions the bar asks for the engine: a site asking for a permission
//! (camera, location, "download multiple files", …) and whether to save a download.
//! They replace Edge's own prompts — native bubbles that the keyboard and hint mode
//! can't reach. Permission questions come first; each is answered with y/Enter or n/Esc.
use std::collections::VecDeque;

use tao::event::KeyEvent;
use tao::keyboard::Key;

use crate::{draw, App, ModeKind};

/// A site's pending permission request.
pub(crate) struct PermissionAsk {
    pub(crate) id: u64,
    /// The asking site's host (`github.com`).
    pub(crate) site: String,
    /// What it wants, finishing "`<site>` wants to …".
    pub(crate) what: String,
}

/// The permission questions still to ask, oldest first.
#[derive(Default)]
pub(crate) struct Permissions {
    pub(crate) asking: VecDeque<PermissionAsk>,
}

impl App {
    /// Whether any question is waiting for an answer.
    pub(crate) fn has_question(&self) -> bool {
        !self.permissions.asking.is_empty() || self.downloads.question().is_some()
    }

    /// A site asked for a permission (see `engines::webview2::permissions`).
    pub(crate) fn on_permission_ask(&mut self, id: u64, url: &str, what: String) {
        let site = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_else(|| url.to_string());
        self.permissions
            .asking
            .push_back(PermissionAsk { id, site, what });
        self.ask_next();
        self.window.request_redraw();
    }

    /// Show the next waiting question, unless you're busy: one that arrives while you
    /// type a command (or resize, pick a hint, …) waits until you're back in Normal mode.
    /// Typing into a page doesn't count as busy — the question came from the page.
    pub(crate) fn ask_next(&mut self) {
        if !self.has_question() {
            return;
        }
        match self.mode {
            ModeKind::Ask => return,
            ModeKind::Normal => {}
            ModeKind::Insert => self.exit_to_normal(),
            ModeKind::Passthrough if !self.active_is_term() && !self.active_is_ai() => {
                self.exit_to_normal()
            }
            _ => return,
        }
        self.mode = ModeKind::Ask;
        self.reclaim_shell_focus();
        self.window.request_redraw();
    }

    /// Keys while a question is showing: y/Enter says yes, n/Esc says no.
    pub(crate) fn key_ask(&mut self, key: &KeyEvent) {
        let yes = match &key.logical_key {
            Key::Enter => true,
            Key::Escape => false,
            Key::Character(c) if c.eq_ignore_ascii_case("y") => true,
            Key::Character(c) if c.eq_ignore_ascii_case("n") => false,
            _ => return,
        };
        if let Some(ask) = self.permissions.asking.pop_front() {
            crate::engines::answer_permission(ask.id, yes);
            self.set_status(if yes {
                format!("allowed {} to {}", ask.site, ask.what)
            } else {
                format!("blocked {} from asking to {}", ask.site, ask.what)
            });
        } else {
            self.answer_download(yes);
        }
        if !self.has_question() {
            self.mode = ModeKind::Normal;
        }
        self.window.request_redraw();
    }

    /// Leave the question prompt if what it was asking about went away meanwhile.
    pub(crate) fn drop_stale_question(&mut self) {
        if self.mode == ModeKind::Ask && !self.has_question() {
            self.mode = ModeKind::Normal;
        }
    }

    /// The bar while a question is showing. Keys come before the source site, so a long
    /// file name or host can't push them off a narrow bar.
    pub(crate) fn ask_segments(&self) -> Vec<(String, draw::Rgb)> {
        let (accent, fg) = (self.theme.accent, self.theme.bar_fg);
        if let Some(ask) = self.permissions.asking.front() {
            return vec![
                ("[PERMISSION]".into(), accent),
                (format!(" {} wants to {}", ask.site, ask.what), fg),
                ("   y/Enter allow · n/Esc block".into(), draw::DIM),
            ];
        }
        let Some(d) = self.downloads.question() else {
            return vec![("[ASK]".into(), accent)];
        };
        let size = d
            .total
            .map(|n| format!(" ({})", crate::downloads::fmt_size(n)))
            .unwrap_or_default();
        let from = url::Url::parse(&d.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        let mut segs = vec![("[DOWNLOAD]".into(), accent)];
        if d.risky {
            segs.push((
                " program/installer, only if you trust it:".into(),
                draw::ERR,
            ));
        }
        segs.push((format!(" {}{size}", d.name), fg));
        segs.push(("   y/Enter save · n/Esc cancel".into(), draw::DIM));
        segs.push((format!("   from {from}"), draw::DIM));
        segs
    }
}
