//! **Profiles** — named, switchable workspaces. A profile is just a
//! [`Session`](crate::session::Session) under a name: the open tabs, the pane/split
//! layout, the window geometry and the UI state (zoom, ad blocker, search engine,
//! shell, page toggles). `:saveprofile work` writes one and makes it current;
//! `:profile work` swaps the whole browser over to it.
//!
//! **Where things live.** Your live session is always `session.toml` (`scratch.toml`
//! while `:scratch` is on) — that's what `:w` writes and what the next launch
//! restores. A profile is a **snapshot** in `profiles/<key>.toml` (see
//! [`session::profile_key`]), written by `:saveprofile` and ONLY by `:saveprofile`:
//! `:w` never touches one, so a profile stays exactly as you last saved it until you
//! deliberately save over it. `config.toml` remembers which profile you're in (as a
//! label) and the `:scratch` state.
//!
//! **A switch can't lose your layout.** Before loading anything, the switch writes
//! what's currently open to the LIVE SESSION file (never to a profile). So leaving a
//! profile without saving keeps the profile snapshot untouched, while the tabs you
//! actually had are still in `session.toml` — and that's what makes the `:scratch`
//! round-trip exact.
//!
//! **`:scratch`** is the emacs-`*scratch*` escape hatch: it parks everything you had
//! (in its own profile file) and hands you an empty browser; toggling it off drops the
//! scratch tabs and brings the parked layout back. Entering is always a clean slate —
//! `scratch.toml` is truncated on the way in — but while you're there it behaves like
//! any other profile, so `:w` inside scratch survives a restart.
//!
//! **Not per-profile:** the visited-URL history (global — switching profiles must not
//! change what `:open <partial>` completes), the customization config, saved `:ai`
//! chats, and the `:ai` tab itself (a windowless singleton, dropped on a switch and
//! re-summonable with `:ai`, its conversations kept in `:aihist`).

use std::path::PathBuf;

use crate::{session, App};

/// What a switch is heading for.
#[derive(Clone, PartialEq)]
pub(crate) enum Target {
    /// The live session (`session.toml`) — where `:w` writes and where the layout
    /// parked on the way into `:scratch` waits.
    Default,
    /// A saved profile snapshot (`profiles/<key>.toml`).
    Named(String),
    /// The scratch slate: an empty browser, with the current layout parked in the
    /// live session file so toggling back restores it.
    Scratch,
}

/// Profile names the user can't take, because a command already means them.
const RESERVED: &[&str] = &["scratch", "default", "none"];

impl App {
    /// The session file `:w` writes and a restart reloads. Deliberately NEVER a
    /// profile: profiles are snapshots that only `:saveprofile` writes, so `:w` in a
    /// profile records your live session without silently editing the snapshot.
    pub(crate) fn current_session_path(&self) -> Option<PathBuf> {
        if self.config.scratch {
            session::scratch_path()
        } else {
            session::session_path()
        }
    }

    /// What the status bar shows for the current profile: `Some("scratch")`,
    /// `Some("<name>")`, or `None` on the unnamed default session.
    pub(crate) fn profile_label(&self) -> Option<&str> {
        if self.config.scratch {
            Some("scratch")
        } else {
            self.config.profile.as_deref()
        }
    }

    /// `:saveprofile <name>` — snapshot the current tabs/layout/state into a named
    /// profile. **The only thing that ever writes a profile file**: `:w` records your
    /// live session, so a snapshot stays exactly as you left it until you save over it
    /// on purpose. Also makes `<name>` the profile you're "in", which is a label (shown
    /// in the status bar) and nothing more.
    pub(crate) fn save_profile(&mut self, name: &str) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("usage: :saveprofile <name>  (e.g. :saveprofile work)".into());
        }
        let key = session::profile_key(name);
        if key.is_empty() {
            return Err(format!("'{name}' isn't a usable profile name — use letters or digits"));
        }
        if RESERVED.contains(&key.as_str()) {
            return Err(format!("'{key}' is reserved — :scratch is the temporary clean slate"));
        }
        let Some(path) = session::profile_path(name) else {
            return Err("no data directory available to save profiles in".into());
        };
        let existed = path.is_file();
        // Saving out of the scratch slate promotes those tabs to a real profile, so
        // the detour is over: there's nothing left to toggle back to.
        self.config.scratch = false;
        self.config.scratch_return = None;
        self.config.profile = Some(name.to_string());
        crate::config::save(&self.config);
        let mut snap = self.snapshot_session();
        snap.name = name.to_string();
        session::save_to(&path, &snap);
        self.window.request_redraw();
        Ok(if existed {
            format!("profile '{name}' saved over")
        } else {
            format!("profile '{name}' saved — :profile {name} brings it back")
        })
    }

    /// `:profile <name>` — load a saved snapshot over the whole browser. `:profile
    /// default` returns to the live session (what `:w` writes). The layout you're
    /// leaving is written to the live session first, so it's never lost even though
    /// the profile snapshot you were in stays untouched.
    pub(crate) fn load_profile(&mut self, name: &str) -> Result<String, String> {
        let name = name.trim();
        let key = session::profile_key(name);
        if key == "scratch" {
            return Ok(self.toggle_scratch());
        }
        if key == "default" || key == "none" {
            self.switch_to(Target::Default);
            self.config.profile = None;
            crate::config::save(&self.config);
            return Ok("switched to the live session".into());
        }
        if !session::profile_exists(name) {
            let known = session::list_profiles();
            return Err(if known.is_empty() {
                format!("no profile '{name}' — :saveprofile {name} creates it")
            } else {
                format!("no profile '{name}' — saved: {}", known.join(", "))
            });
        }
        self.switch_to(Target::Named(name.to_string()));
        Ok(format!("switched to profile '{name}'"))
    }

    /// `:delprofile <name>` — delete a saved profile's snapshot. The tabs stay on
    /// screen (they're your live session); only the saved copy goes, and the status
    /// label with it if that's the profile you were in.
    pub(crate) fn delete_profile(&mut self, name: &str) -> Result<String, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("usage: :delprofile <name>".into());
        }
        if !session::profile_exists(name) {
            return Err(format!("no profile '{name}'"));
        }
        if !session::delete_profile(name) {
            return Err(format!("couldn't delete profile '{name}'"));
        }
        let key = session::profile_key(name);
        let was_active = self
            .config
            .profile
            .as_deref()
            .is_some_and(|p| session::profile_key(p) == key);
        if was_active {
            self.config.profile = None;
        }
        // A scratch return-path pointing at the deleted profile would strand you.
        if self.config.scratch_return.as_deref().is_some_and(|p| session::profile_key(p) == key) {
            self.config.scratch_return = None;
        }
        crate::config::save(&self.config);
        self.window.request_redraw();
        Ok(if was_active {
            format!("deleted profile '{name}' — these tabs stay open as your live session")
        } else {
            format!("deleted profile '{name}'")
        })
    }

    /// `:scratch` — toggle the clean slate. Going IN parks everything you have in the
    /// live session file and leaves you with an empty browser; coming OUT drops the
    /// scratch tabs and restores exactly what was parked — the tabs you actually had,
    /// which is why the return reads the LIVE SESSION and not the profile snapshot
    /// (that may be older, and `:scratch` must not silently save over it).
    pub(crate) fn toggle_scratch(&mut self) -> String {
        if self.config.scratch {
            let back = self.config.scratch_return.clone();
            self.switch_to(Target::Default);
            // Restore the label too, so the bar reads `[work]` again rather than
            // pretending you'd landed on the unnamed session.
            self.config.profile = back.clone();
            crate::config::save(&self.config);
            match back {
                Some(name) => format!("left scratch — back in profile '{name}'"),
                None => "left scratch — your layout is back".to_string(),
            }
        } else {
            self.switch_to(Target::Scratch);
            "scratch: clean slate — :scratch again to bring your layout back".to_string()
        }
    }

    /// Move the whole browser to another session file: save what's open now (to the
    /// file it belongs to), tear every tab down, repoint the config, then restore the
    /// target's tabs/layout/state. The visited history is global, so it survives the
    /// swap untouched.
    pub(crate) fn switch_to(&mut self, target: Target) {
        // 0. Pin the engine open BEFORE the tabs go, while its browser process is still
        //    running, so the tabs we rebuild in a moment attach to the SAME process
        //    instead of cold-starting one: that keeps even session cookies and a warm
        //    cache across the swap. Only worth it when there are tabs to rebuild — going
        //    INTO `:scratch` ends with an empty browser, and pinning an engine for an
        //    empty browser is exactly the resident-memory bloat this project exists to
        //    avoid. See `App::hold_engine`.
        if target != Target::Scratch && self.tabs.iter().any(|t| t.webview().is_some()) {
            self.hold_engine();
        }
        // 1. Save the outgoing layout — to the LIVE SESSION file, never to a profile.
        //    That's the "a switch can't lose your layout" guarantee without ever editing
        //    a snapshot behind your back (and it's where `:scratch` parks it).
        self.save_session();
        // 2. Empty the browser: shut terminals down, drop webviews, clear the layout.
        self.close_everything();
        // 3. Repoint at the new file and persist the pointer, so a restart lands here.
        match &target {
            Target::Default => {
                self.config.profile = None;
                self.config.scratch = false;
                self.config.scratch_return = None;
            }
            Target::Named(name) => {
                self.config.profile = Some(name.clone());
                self.config.scratch = false;
                self.config.scratch_return = None;
            }
            Target::Scratch => {
                // Remember where to return; the profile pointer stays put so leaving
                // scratch (and `:profiles`) knows the way back.
                self.config.scratch_return = self.config.profile.take();
                self.config.scratch = true;
            }
        }
        crate::config::save(&self.config);
        // 4. Load the target: a profile reads its SNAPSHOT, everything else reads the
        //    live session (which is where the layout parked on the way into scratch is).
        //    Scratch itself always OPENS empty — that's the point — and its file is
        //    truncated to the empty layout now on screen, so a quit-and-relaunch inside
        //    scratch stays clean instead of resurrecting an older one.
        let source = match &target {
            Target::Default => session::session_path(),
            Target::Named(name) => session::profile_path(name),
            Target::Scratch => None,
        };
        if target == Target::Scratch {
            self.save_session();
        } else if let Some(s) = source.as_deref().and_then(session::load_from) {
            self.apply_session(s);
        }
        // 5. Hand the engine over to the rebuilt tabs (`refresh_visibility` usually does
        //    this the moment the first one exists) and drop the pin. With no web tabs to
        //    hand it to, the engine shuts down here — the intended engine-free idle, at
        //    no lasting cost. What's lost when it does is only what Chromium itself
        //    treats as throwaway: session cookies and cache warmth. Persistent cookies —
        //    every real login, including the tokens Google rotates every few minutes —
        //    are flushed to the profile on the way down and come back (measured).
        self.release_engine();
        self.clear_status();
        self.window.request_redraw();
    }

    /// Restore a session over the LIVE app (as opposed to at startup): apply the saved
    /// window geometry, then reopen the tabs/layout/UI state. The visited history is
    /// global — it belongs to the browser, not to a profile — so it's carried across
    /// the restore rather than replaced by the file's copy.
    pub(crate) fn apply_session(&mut self, s: session::Session) {
        if let Some(g) = &s.window {
            self.window.set_outer_position(tao::dpi::PhysicalPosition::new(g.x, g.y));
            self.window.set_inner_size(tao::dpi::PhysicalSize::new(g.w, g.h));
        }
        let history = std::mem::take(&mut self.history);
        let history_at = std::mem::take(&mut self.history_at);
        self.restore_session(s);
        self.history = history;
        self.history_at = history_at;
    }

    /// Close every tab and forget the layout, leaving the welcome screen — the live
    /// half of [`teardown`](App::teardown) (terminals shut down deterministically,
    /// webviews dropped), but with the app still running.
    ///
    /// The `:ai` tab survives: it's a windowless background singleton (no pane, never
    /// in a session), and it may be the very thing driving the switch — dropping it
    /// mid-turn would throw away the conversation that asked for it.
    pub(crate) fn close_everything(&mut self) {
        // Read this BEFORE the tabs move: indices shift as the others are dropped.
        let was_on_ai = self.active_is_ai();
        for tab in &mut self.tabs {
            if tab.ai().is_some() {
                continue;
            }
            if let Some(session) = tab.take_term() {
                session.shutdown();
            }
        }
        self.tabs.retain(|t| t.ai().is_some());
        // Pane trees index into `tabs`; drop them in lock-step or a redraw indexes
        // an empty list through a stale window. (The retained AI tab has no window.)
        self.windows.clear();
        // Stay on the AI tab if that's where we were — it's the only survivor — else
        // fall back to the welcome screen until the new profile's tabs land.
        self.active =
            was_on_ai.then(|| self.tabs.iter().position(|t| t.ai().is_some())).flatten();
        self.ai_prev_active = None;
        // Tabs from the profile we're leaving must not be reopenable in the next one.
        self.closed_tabs.clear();
        self.find_reset();
        self.mode = crate::ModeKind::Normal;
        self.native_hints.clear();
        self.refresh_visibility();
    }
}

/// The `:profiles` picker's lines: a header, then one row per saved profile with the
/// active one marked. `active` is what [`App::profile_label`] returns, so the scratch
/// row is marked while `:scratch` is on.
pub(crate) fn profile_lines(names: &[String], active: Option<&str>, scratch_return: Option<&str>) -> Vec<String> {
    let mut lines = vec![
        format!("{} saved profile{}   —   Enter switches · d deletes", names.len(), if names.len() == 1 { "" } else { "s" }),
        String::new(),
    ];
    let mark = |row_key: &str| -> &'static str {
        match active {
            Some(a) if session::profile_key(a) == row_key => "▸ ",
            _ => "  ",
        }
    };
    // The always-present rows: the unnamed session and the scratch slate.
    let default_active = active.is_none();
    lines.push(format!("{}default   (the unnamed session)", if default_active { "▸ " } else { "  " }));
    let scratch_note = match scratch_return {
        Some(back) if active == Some("scratch") => format!("   (clean slate — back to '{back}')"),
        _ => "   (temporary clean slate)".to_string(),
    };
    lines.push(format!("{}scratch{scratch_note}", mark("scratch")));
    if !names.is_empty() {
        lines.push(String::new());
    }
    for n in names {
        lines.push(format!("{}{n}", mark(&session::profile_key(n))));
    }
    lines
}

/// Map a `:profiles` row to what it selects: the row text minus the marker, or
/// `None` for the header/blank rows. Row 2 is `default`, row 3 `scratch`, and the
/// rest are profile names (the trailing parenthetical note is stripped).
pub(crate) fn profile_at_row(lines: &[String], row: usize) -> Option<String> {
    let line = lines.get(row)?;
    let text = line.trim_start_matches(['▸', ' ']).trim();
    if row < 2 || text.is_empty() {
        return None;
    }
    let name = match text.split_once("   (") {
        Some((n, _)) => n.trim(),
        None => text,
    };
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["personal".to_string(), "work".to_string()]
    }

    #[test]
    fn picker_marks_the_active_profile() {
        let lines = profile_lines(&names(), Some("work"), None);
        // Header + blank, then default/scratch, blank, then the saved names.
        assert!(lines[0].starts_with("2 saved profiles"));
        assert!(lines[2].starts_with("  default"));
        assert!(lines[3].starts_with("  scratch"));
        assert!(lines[5].starts_with("  personal"));
        assert!(lines[6].starts_with("▸ work"));
    }

    #[test]
    fn picker_marks_default_and_scratch() {
        let lines = profile_lines(&names(), None, None);
        assert!(lines[2].starts_with("▸ default"));
        let lines = profile_lines(&names(), Some("scratch"), Some("work"));
        assert!(lines[3].starts_with("▸ scratch"));
        assert!(lines[3].contains("back to 'work'"));
    }

    #[test]
    fn rows_map_back_to_names() {
        let lines = profile_lines(&names(), Some("work"), None);
        assert_eq!(profile_at_row(&lines, 0), None); // header
        assert_eq!(profile_at_row(&lines, 1), None); // blank
        assert_eq!(profile_at_row(&lines, 2).as_deref(), Some("default"));
        assert_eq!(profile_at_row(&lines, 3).as_deref(), Some("scratch"));
        assert_eq!(profile_at_row(&lines, 4), None); // separator
        assert_eq!(profile_at_row(&lines, 5).as_deref(), Some("personal"));
        assert_eq!(profile_at_row(&lines, 6).as_deref(), Some("work"));
        assert_eq!(profile_at_row(&lines, 99), None);
    }

    #[test]
    fn picker_without_saved_profiles_still_offers_default_and_scratch() {
        let lines = profile_lines(&[], None, None);
        assert_eq!(lines.len(), 4);
        assert_eq!(profile_at_row(&lines, 2).as_deref(), Some("default"));
        assert_eq!(profile_at_row(&lines, 3).as_deref(), Some("scratch"));
    }
}
