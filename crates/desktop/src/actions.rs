//! The browser **action layer** — one registry of named, described operations that
//! change browser state or data, and one [`App::perform`] entry point that runs
//! them. This is the foundation for letting the `:ai` assistant "mess with the
//! browser" without a config panel or a command per knob: today the `:` command bar
//! routes `:clear cookies` here; next, the assistant is handed [`ACTIONS`] as its
//! tool menu and a model choice like `clear(cookies)` is dispatched through the
//! exact same code and descriptions. One surface, one source of truth.
//!
//! Specs are deliberately flat — an action takes zero or one argument, drawn from a
//! fixed `values` list — so the same [`ActionSpec`] serves command autocomplete, the
//! `:commands` help, and (soon) the assistant's tool schema with no bespoke schema
//! language. As the surface grows (open/close tabs, toggles, navigation…) those
//! verbs migrate here too; for now it hosts the privacy/data actions, which had no
//! command home before.

use crate::data::DataKind;
use crate::App;

/// One invokable browser action: a stable `name`, the argument `values` it accepts
/// (empty for a no-argument action), and a one-line `summary`. The summary is
/// written to read as an instruction to the assistant as much as to a person.
pub(crate) struct ActionSpec {
    pub(crate) name: &'static str,
    pub(crate) values: &'static [&'static str],
    pub(crate) summary: &'static str,
}

/// Every action the browser exposes through [`App::perform`]. The single list the
/// command bar, the help page, and the assistant all read from.
pub(crate) const ACTIONS: &[ActionSpec] = &[ActionSpec {
    name: "clear",
    values: &["history", "cookies", "cache", "all"],
    summary: "Erase stored browsing data — visited history, cookies (sign-ins/sessions), \
              the disk cache, or everything (a fresh profile).",
}];

/// Render the registry as `(invocation, summary)` rows for the `:commands` help —
/// the same list that will become the assistant's tool menu, so the help and the AI
/// can never drift apart. A no-argument action shows as `:name`; one taking values
/// shows them inline, e.g. `:clear <history|cookies|cache|all>`.
pub(crate) fn help_rows() -> Vec<(String, String)> {
    ACTIONS
        .iter()
        .map(|a| {
            let inv = if a.values.is_empty() {
                format!(":{}", a.name)
            } else {
                format!(":{} <{}>", a.name, a.values.join("|"))
            };
            (inv, a.summary.to_string())
        })
        .collect()
}

impl App {
    /// Run an action by `name` + `arg` — the one entry point shared by the command
    /// bar and (soon) the assistant. Returns a status line on success or a
    /// human-readable error; unknown names/args are reported, never panic. Anything
    /// asynchronous (e.g. an engine data wipe) reports its real completion later via
    /// its own event; the returned line is the "started" acknowledgement.
    pub(crate) fn perform(&mut self, name: &str, arg: &str) -> Result<String, String> {
        match name {
            "clear" => self.action_clear(arg.trim()),
            other => Err(format!("unknown action: {other}")),
        }
    }

    /// Run an action and report the outcome in the status bar — the command-bar
    /// convenience over [`perform`](App::perform). (The assistant will call
    /// `perform` directly so it can read the result text back.)
    pub(crate) fn run_action(&mut self, name: &str, arg: &str) {
        match self.perform(name, arg) {
            Ok(msg) => self.set_status(msg),
            Err(e) => self.set_error(e),
        }
        self.window.request_redraw();
    }

    /// `clear <what>` — wipe a slice of stored data. `history` clears the shell's own
    /// visited list and reopen stack immediately (and the engine's history if a page
    /// is open); `cookies` / `cache` / `all` go through the WebView2 profile and so
    /// need at least one web tab open to reach the engine.
    fn action_clear(&mut self, what: &str) -> Result<String, String> {
        match what {
            "history" | "hist" => {
                let n = self.history.len();
                self.history.clear();
                self.closed_tabs.clear();
                // Best-effort: also drop the engine's own history if a page is live.
                self.clear_engine(DataKind::History, "history");
                self.window.request_redraw();
                Ok(format!("cleared {n} history entries"))
            }
            "cookies" => self.clear_engine_required(DataKind::Cookies, "cookies"),
            "cache" => self.clear_engine_required(DataKind::Cache, "cache"),
            "all" | "everything" => self.clear_engine_required(DataKind::All, "all browsing data"),
            "" => Err("clear what? — history, cookies, cache, or all".into()),
            other => Err(format!("can't clear '{other}' — try history, cookies, cache, or all")),
        }
    }

    /// Fire an engine data-clear if a web tab exists, ignoring the no-engine case
    /// (used for the bonus engine-history wipe alongside the shell's own list).
    fn clear_engine(&self, kind: DataKind, _label: &str) {
        if let Some(wv) = self.any_webview() {
            let _ = crate::data::clear(wv, kind, String::new(), self.proxy.clone());
        }
    }

    /// Fire an engine data-clear, requiring a live web tab to reach the shared
    /// profile. Returns the "clearing…" acknowledgement; the real confirmation
    /// arrives later as [`UserEvent::DataCleared`](crate::UserEvent::DataCleared).
    fn clear_engine_required(&self, kind: DataKind, label: &str) -> Result<String, String> {
        let Some(wv) = self.any_webview() else {
            return Err(format!("open a page first, then clear {label}"));
        };
        crate::data::clear(wv, kind, label.to_string(), self.proxy.clone())?;
        Ok(format!("clearing {label}…"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_rows_render_values_inline_and_cover_the_registry() {
        let rows = help_rows();
        assert_eq!(rows.len(), ACTIONS.len());
        let clear = rows.iter().find(|(inv, _)| inv.starts_with(":clear")).expect("clear listed");
        assert_eq!(clear.0, ":clear <history|cookies|cache|all>");
    }
}
