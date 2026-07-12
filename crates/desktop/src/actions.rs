//! The browser **action layer** — one registry of named, described, parameterized
//! operations that change browser state or data, and one [`App::perform`] entry
//! point that runs them. This is the foundation for letting the `:ai` assistant
//! "mess with the browser" without a config panel or a command per knob: the `:`
//! command bar routes `:clear cookies 15m` here, and the assistant is handed
//! [`groq_tools`] (built from this same registry) as its tool menu, so a model
//! choice like `clear(what=history, period=24h)` dispatches through the exact same
//! code and descriptions. One surface, one source of truth.
//!
//! Actions take named string arguments drawn from fixed `values` lists, so the same
//! [`ActionSpec`] serves command autocomplete, the `:commands` help, and the OpenAI
//! tool schema with no bespoke schema language. As the surface grows (tabs, toggles,
//! navigation, chrome customization…) those verbs migrate here too; for now it hosts
//! the privacy/data actions.

use serde_json::Value;

use crate::app::now_epoch;
use crate::data::DataKind;
use crate::panes::SplitDir;
use crate::App;

/// One argument of an action: its `name`, the `values` it accepts, whether it's
/// `required`, and a `desc` written to guide the assistant.
pub(crate) struct ParamSpec {
    pub(crate) name: &'static str,
    pub(crate) values: &'static [&'static str],
    pub(crate) required: bool,
    pub(crate) desc: &'static str,
}

/// One invokable browser action: a stable `name`, a one-line `summary` (written to
/// read as an instruction to the assistant as much as to a person), and its `params`.
pub(crate) struct ActionSpec {
    pub(crate) name: &'static str,
    pub(crate) summary: &'static str,
    pub(crate) params: &'static [ParamSpec],
}

/// Every action the browser exposes through [`App::perform`] — the single list the
/// command bar, the help page, and the assistant all read from. A param with an
/// empty `values` list is free text (no fixed choices).
pub(crate) const ACTIONS: &[ActionSpec] = &[
    ActionSpec {
        name: "clear",
        summary: "Erase stored browsing data, optionally only from a recent time window. \
                  Use when the user asks to clear/delete/wipe history, cookies, cache, or \
                  everything (e.g. \"clear the last 24h history\", \"wipe the last 15 min cache\").",
        params: &[
            ParamSpec {
                name: "what",
                values: &["history", "cookies", "cache", "all"],
                required: true,
                desc: "Which data to erase: history (the visited-URL list), cookies \
                       (sign-ins/sessions), cache (cached files), or all (a fresh profile).",
            },
            ParamSpec {
                name: "period",
                values: &["15m", "1h", "24h", "7d", "all"],
                required: false,
                desc: "How far back to erase; defaults to all time. 15m = last 15 minutes, \
                       1h = last hour, 24h = last day, 7d = last week, all = everything.",
            },
        ],
    },
    ActionSpec {
        name: "alias",
        summary: "Create or update a command alias — a short name typed after ':' that \
                  expands to a full command line (e.g. name 'gh' → 'open github.com', so \
                  ':gh' opens GitHub). Use when the user asks for a shortcut/alias.",
        params: &[
            ParamSpec {
                name: "name",
                values: &[],
                required: true,
                desc: "The alias name: a single word, no spaces, typed after ':'.",
            },
            ParamSpec {
                name: "expansion",
                values: &[],
                required: true,
                desc: "The command line it expands to, WITHOUT the leading ':' \
                       (e.g. 'open github.com', 'research -t', 'clear cache 15m').",
            },
        ],
    },
    ActionSpec {
        name: "unalias",
        summary: "Remove a command alias the user no longer wants.",
        params: &[ParamSpec {
            name: "name",
            values: &[],
            required: true,
            desc: "The alias name to remove.",
        }],
    },
    ActionSpec {
        name: "restore",
        summary: "Reset ALL customization (aliases, appearance, and any other tunable \
                  settings) back to defaults. Use when the user asks to restore/reset \
                  defaults or undo their customizations.",
        params: &[],
    },
    ActionSpec {
        name: "theme",
        summary: "Customize the browser's appearance: the command/status bar HEIGHT, the \
                  interface COLOURS (the bar background and text, the accent/highlight \
                  colour, and the page background), and the `:te` TERMINAL's font, font \
                  size, and colours. Use for requests like 'make the command bar 25% \
                  taller', 'set the bar background to green', 'change the terminal font \
                  to Cascadia Code', 'use the dracula terminal colours'. Only set the \
                  fields the user mentions; leave the rest out to keep them unchanged. \
                  The value 'default' resets a field to its built-in default.",
        params: &[
            ParamSpec {
                name: "bar_height_pct",
                values: &[],
                required: false,
                desc: "Command/status bar height as a percent of default: 100 = default, \
                       125 = 25% taller, 75 = shorter. Range 50–300.",
            },
            ParamSpec {
                name: "bar_bg",
                values: &[],
                required: false,
                desc: "Command/status (and tab) bar background colour: a name like 'green' \
                       or a hex like '#0a2a0a'.",
            },
            ParamSpec {
                name: "bar_fg",
                values: &[],
                required: false,
                desc: "Command/status bar text colour (name or hex).",
            },
            ParamSpec {
                name: "accent",
                values: &[],
                required: false,
                desc: "Accent/highlight colour — the active tab, caret, and focused-pane \
                       border (name or hex).",
            },
            ParamSpec {
                name: "bg",
                values: &[],
                required: false,
                desc: "Page and welcome-screen background colour (name or hex).",
            },
            ParamSpec {
                name: "term_font",
                values: &[],
                required: false,
                desc: "Terminal (`:te`) font: an installed font's name (e.g. 'Cascadia \
                       Code', 'JetBrains Mono') or a .ttf/.otf path. 'default' returns \
                       to the built-in monospace.",
            },
            ParamSpec {
                name: "term_font_px",
                values: &[],
                required: false,
                desc: "Terminal font size in px at zoom 1.0 (e.g. 14). Range 6-72; \
                       'default' matches the interface font size again.",
            },
            ParamSpec {
                name: "term_scheme",
                values: &[],
                required: false,
                desc: "Terminal colour scheme (the 16 ANSI colours + default fg/bg). \
                       Built in: campbell (the default), dracula, gruvbox, nord, \
                       solarized, onedark, monokai. Any scheme installed with \
                       install_scheme works too.",
            },
            ParamSpec {
                name: "term_bg",
                values: &[],
                required: false,
                desc: "Terminal background colour (name or hex) — overrides the scheme's.",
            },
            ParamSpec {
                name: "term_fg",
                values: &[],
                required: false,
                desc: "Terminal default text colour (name or hex) — overrides the scheme's.",
            },
        ],
    },
    ActionSpec {
        name: "install_scheme",
        summary: "Download and install a `:te` terminal colour scheme by name from the \
                  published iTerm2-Color-Schemes and Gogh collections (hundreds of \
                  schemes — e.g. IBM 3270, Catppuccin Mocha, Tokyo Night, Rosé Pine), \
                  then apply it. Use when the user wants a terminal scheme that is \
                  neither built in nor installed — but ALWAYS ask the user to confirm \
                  first (it downloads from the internet); only call this after they \
                  agree. The download finishes in the background: a follow-up message \
                  reports the real outcome, so never claim success yourself. If the \
                  name matches several schemes, the result lists them: ask the user \
                  which one they want.",
        params: &[ParamSpec {
            name: "name",
            values: &[],
            required: true,
            desc: "The scheme to search the collection for, e.g. 'ibm 3270' or \
                   'catppuccin mocha'.",
        }],
    },
    ActionSpec {
        name: "open",
        summary: "Open a URL or search query — in the current tab/pane, or a new tab. \
                  For a side-by-side layout, 'split' first, then 'open' to fill the new pane.",
        params: &[
            ParamSpec {
                name: "target",
                values: &[],
                required: true,
                desc: "A URL (e.g. github.com) or a search query (anything not a URL is searched).",
            },
            ParamSpec {
                name: "where",
                values: &["current", "new"],
                required: false,
                desc: "Where to open it: the current tab/pane (default), or a new tab.",
            },
        ],
    },
    ActionSpec {
        name: "split",
        summary: "Split the layout into tmux-style panes so two pages show at once. The \
                  NEW pane becomes focused, so a following 'open' fills it — that's how to \
                  put two sites side by side.",
        params: &[ParamSpec {
            name: "direction",
            values: &["vertical", "horizontal"],
            required: false,
            desc: "vertical = panes side by side (default); horizontal = stacked top/bottom.",
        }],
    },
    ActionSpec {
        name: "close",
        summary: "Close the current tab (or focused pane).",
        params: &[],
    },
    ActionSpec {
        name: "reopen",
        summary: "Reopen the most recently closed tab.",
        params: &[],
    },
    ActionSpec {
        name: "tab",
        summary: "Switch which tab is focused, to the next or previous one.",
        params: &[ParamSpec {
            name: "to",
            values: &["next", "previous"],
            required: false,
            desc: "Which tab to focus; defaults to next.",
        }],
    },
    ActionSpec {
        name: "navigate",
        summary: "Navigate the current page: go back or forward in its history, or reload it.",
        params: &[ParamSpec {
            name: "direction",
            values: &["back", "forward", "reload"],
            required: true,
            desc: "back or forward in history, or reload the current page.",
        }],
    },
];

/// The registry as an OpenAI-style `tools` array for the Groq request: each action
/// becomes a `function` with an object schema whose properties are its params
/// (string + `enum` of accepted values). The assistant picks one and supplies args,
/// which [`App::perform`] runs.
pub(crate) fn groq_tools() -> Value {
    let tools: Vec<Value> = ACTIONS
        .iter()
        .map(|a| {
            let mut props = serde_json::Map::new();
            let mut required = Vec::new();
            for p in a.params {
                let mut schema = serde_json::Map::new();
                schema.insert("type".into(), Value::String("string".into()));
                schema.insert("description".into(), Value::String(p.desc.into()));
                // Only constrain to an enum when the param has fixed choices.
                if !p.values.is_empty() {
                    schema.insert("enum".into(), serde_json::json!(p.values));
                }
                props.insert(p.name.to_string(), Value::Object(schema));
                if p.required {
                    required.push(p.name);
                }
            }
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": a.name,
                    "description": a.summary,
                    "parameters": { "type": "object", "properties": props, "required": required },
                }
            })
        })
        .collect();
    Value::Array(tools)
}

/// Whether `name` is a real action in the registry — used to reject a model's
/// mangled/hallucinated tool name before it's echoed back to Groq (which 400s a
/// `tool_calls` entry naming a tool that wasn't in the request).
pub(crate) fn is_action(name: &str) -> bool {
    ACTIONS.iter().any(|a| a.name == name)
}

/// Whether an action targets the focused CONTENT tab/pane — so the AI dispatch must
/// move focus off the chat before running it (an `open`/`split` on the AI tab would
/// clobber the chat). Settings/data actions (theme, alias, clear, install_scheme, …)
/// don't touch the pane layout and must NOT trigger that blank-tab dance.
pub(crate) fn targets_content(name: &str) -> bool {
    matches!(name, "open" | "split" | "close" | "reopen" | "tab" | "navigate")
}

/// Build an args object by zipping whitespace tokens in `rest` onto `names`, so a
/// command line like `:clear cache 15m` (`names = ["what","period"]`) becomes
/// `{"what":"cache","period":"15m"}` — the same shape the assistant's tool-calls use.
pub(crate) fn positional(names: &[&str], rest: &str) -> Value {
    let mut map = serde_json::Map::new();
    let mut toks = rest.split_whitespace();
    for n in names {
        match toks.next() {
            Some(t) => {
                map.insert((*n).to_string(), Value::String(t.to_string()));
            }
            None => break,
        }
    }
    Value::Object(map)
}

impl App {
    /// Run an action by `name` with a JSON `args` object — the one entry point shared
    /// by the command bar and the assistant. Returns a status line on success or a
    /// human-readable error; unknown names/args are reported, never panic. Anything
    /// asynchronous (an engine data wipe) reports its real completion later via its
    /// own event; the returned line is the "started" acknowledgement.
    pub(crate) fn perform(&mut self, name: &str, args: &Value) -> Result<String, String> {
        let str_arg = |k: &str| args.get(k).and_then(|v| v.as_str()).unwrap_or("").trim();
        match name {
            "clear" => {
                let period = args
                    .get("period")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty());
                self.action_clear(str_arg("what"), period)
            }
            "alias" => self.set_alias(str_arg("name"), str_arg("expansion")),
            "unalias" => self.remove_alias(str_arg("name")),
            "restore" => Ok(self.restore_defaults()),
            "theme" => self.set_theme(args),
            "install_scheme" => {
                let name = str_arg("name");
                if name.is_empty() {
                    return Err("which scheme? give a name, e.g. 3270-Dark".into());
                }
                if let Some(cur) = &self.installing_scheme {
                    return Err(format!("already downloading '{cur}' — wait for its result"));
                }
                self.installing_scheme = Some(name.to_string());
                crate::schemes::spawn_install(name.to_string(), self.acting_ai, self.proxy.clone());
                // Shown verbatim in the chat AND fed to the model as the tool result,
                // so keep it user-appropriate; the "don't claim success, don't call
                // again" guidance lives in the action's summary instead.
                Ok(format!(
                    "downloading terminal scheme '{name}' — the result will be posted in \
                     this chat shortly"
                ))
            }
            "open" => {
                let target = str_arg("target");
                if target.is_empty() {
                    return Err("open what? — give a URL or a search query".into());
                }
                let new_tab = str_arg("where") == "new";
                self.open_tab(target, self.nojs, new_tab);
                Ok(format!("opened {target}{}", if new_tab { " in a new tab" } else { "" }))
            }
            "split" => {
                let horizontal = matches!(str_arg("direction"), "horizontal" | "stacked");
                self.split_pane(if horizontal { SplitDir::Col } else { SplitDir::Row });
                Ok(format!(
                    "split the layout {}",
                    if horizontal { "top/bottom" } else { "side by side" }
                ))
            }
            "close" => {
                self.close_active();
                Ok("closed the tab".into())
            }
            "reopen" => {
                self.reopen_closed();
                Ok("reopened the last closed tab".into())
            }
            "tab" => {
                let prev = matches!(str_arg("to"), "previous" | "prev" | "left" | "back");
                self.switch_tab(if prev { -1 } else { 1 });
                Ok(format!("switched to the {} tab", if prev { "previous" } else { "next" }))
            }
            "navigate" => match str_arg("direction") {
                "forward" => {
                    self.history(true);
                    Ok("went forward".into())
                }
                "reload" | "refresh" => {
                    self.reload_active();
                    Ok("reloaded the page".into())
                }
                "back" | "" => {
                    self.history(false);
                    Ok("went back".into())
                }
                other => Err(format!("can't navigate '{other}' — try back, forward, or reload")),
            },
            other => Err(format!("unknown action: {other}")),
        }
    }

    /// Define/replace a command alias `name` → `expansion` and persist it. Guards the
    /// name (single word; never `restore`, so the panic button can't be shadowed) and
    /// requires a non-empty expansion. The expansion is stored without a leading `:`.
    pub(crate) fn set_alias(&mut self, name: &str, expansion: &str) -> Result<String, String> {
        let name = name.trim();
        let expansion = expansion.trim().trim_start_matches(':').trim();
        if name.is_empty() || expansion.is_empty() {
            return Err("usage: :alias <name> <command>  (e.g. :alias gh open github.com)".into());
        }
        if name.split_whitespace().count() != 1 {
            return Err("an alias name must be a single word (no spaces)".into());
        }
        if name == "restore" {
            return Err("'restore' is reserved — it's the reset-to-defaults command".into());
        }
        self.config.aliases.insert(name.to_string(), expansion.to_string());
        crate::config::save(&self.config);
        Ok(format!("alias :{name} → {expansion}"))
    }

    /// Remove a command alias, persisting the change. Errors if it didn't exist.
    pub(crate) fn remove_alias(&mut self, name: &str) -> Result<String, String> {
        let name = name.trim();
        if self.config.aliases.remove(name).is_some() {
            crate::config::save(&self.config);
            Ok(format!("removed alias :{name}"))
        } else {
            Err(format!("no alias :{name}"))
        }
    }

    /// Reset ALL customization to defaults and persist the empty config — the safety
    /// net behind `:restore` and the `Ctrl+Alt+Shift+R` hook chord. Re-applies every
    /// config-driven surface live (today: chrome appearance), so a bad theme is undone
    /// without a restart. Returns the status message.
    pub(crate) fn restore_defaults(&mut self) -> String {
        self.config = crate::config::Config::default();
        crate::config::save(&self.config);
        self.rebuild_theme();
        self.rebuild_term_style();
        self.refresh_visibility(); // bar height may have changed → reflow content/webviews
        self.window.request_redraw();
        "restored default settings".to_string()
    }

    /// `theme {bar_height_pct, bar_bg, bar_fg, accent, bg, term_font, term_font_px,
    /// term_scheme, term_bg, term_fg}` — apply the appearance overrides the user named
    /// (others untouched), persist them, and re-resolve the live theme / terminal
    /// style. Everything is validated up front (a typo is reported, with nothing
    /// partially applied); the value `default` resets a field. A height change reflows
    /// the content band. The `Ctrl+Alt+Shift+R` chord / `:restore` undo it all if a
    /// choice turns out unreadable.
    pub(crate) fn set_theme(&mut self, args: &Value) -> Result<String, String> {
        let arg = |k: &str| {
            args.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
        };
        let is_default =
            |s: &str| matches!(s.to_ascii_lowercase().as_str(), "default" | "reset" | "none");
        // Validate everything BEFORE mutating, so a bad value aborts cleanly.
        for key in ["bar_bg", "bar_fg", "accent", "bg", "term_bg", "term_fg"] {
            if let Some(s) = arg(key) {
                if !is_default(s) && crate::draw::parse_color(s).is_none() {
                    return Err(format!(
                        "'{s}' isn't a colour I recognise — use a name like green or a hex like #0a2a0a"
                    ));
                }
            }
        }
        if let Some(s) = arg("term_scheme") {
            if !is_default(s)
                && crate::pty_term::scheme(s).is_none()
                && crate::config::load_custom_scheme(s).is_none()
            {
                let installed = crate::config::custom_scheme_names();
                let installed = if installed.is_empty() {
                    String::new()
                } else {
                    format!(" Installed: {}.", installed.join(", "))
                };
                return Err(format!(
                    "the terminal scheme '{s}' isn't installed. Built in: {}.{installed} \
                     install_scheme (`:theme install <name>`) can download it from the \
                     published scheme collections — ask the user before installing.",
                    crate::pty_term::SCHEMES.join(", ")
                ));
            }
        }
        let mut term_font_file = String::new();
        if let Some(s) = arg("term_font") {
            if !is_default(s) {
                match crate::draw::find_font(s) {
                    Some(p) => {
                        term_font_file =
                            p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
                    }
                    None => {
                        return Err(format!(
                            "no installed font matches '{s}' — use the font's name (as in \
                             Settings > Fonts) or a .ttf/.otf path"
                        ))
                    }
                }
            }
        }
        // Sets an Option<String> field: `default` clears the override.
        fn set_opt(slot: &mut Option<String>, s: &str, clear: bool) {
            *slot = if clear { None } else { Some(s.to_string()) };
        }
        let mut changed: Vec<String> = Vec::new();
        let mut height_changed = false;
        let mut term_changed = false;
        if let Some(v) = args.get("bar_height_pct") {
            // Accept a JSON number, a string like "125" / "125%", or `default`.
            let s = v.as_str().map(str::trim).unwrap_or("");
            let pct = v.as_u64().map(|n| n as u32).or_else(|| {
                s.trim_end_matches('%').trim().parse().ok()
            });
            if !s.is_empty() && is_default(s) {
                self.config.theme.bar_height_pct = None;
                changed.push("bar height to default".into());
                height_changed = true;
            } else if let Some(p) = pct {
                let p = p.clamp(50, 300);
                self.config.theme.bar_height_pct = Some(p);
                changed.push(format!("bar height to {p}%"));
                height_changed = true;
            } else {
                return Err("bar_height_pct must be a number like 125 (percent of default)".into());
            }
        }
        if let Some(s) = arg("bar_bg") {
            set_opt(&mut self.config.theme.bar_bg, s, is_default(s));
            changed.push(format!("bar background to {s}"));
        }
        if let Some(s) = arg("bar_fg") {
            set_opt(&mut self.config.theme.bar_fg, s, is_default(s));
            changed.push(format!("bar text to {s}"));
        }
        if let Some(s) = arg("accent") {
            set_opt(&mut self.config.theme.accent, s, is_default(s));
            changed.push(format!("accent to {s}"));
        }
        if let Some(s) = arg("bg") {
            set_opt(&mut self.config.theme.bg, s, is_default(s));
            changed.push(format!("background to {s}"));
        }
        if let Some(s) = arg("term_font") {
            set_opt(&mut self.config.term.font, s, is_default(s));
            if term_font_file.is_empty() {
                changed.push("terminal font to default".into());
            } else {
                changed.push(format!("terminal font to {s} ({term_font_file})"));
            }
            term_changed = true;
        }
        if let Some(v) = args.get("term_font_px") {
            // Accept a JSON number, a numeric string, or `default` to clear.
            let s = v.as_str().map(str::trim).unwrap_or("");
            if !s.is_empty() && is_default(s) {
                self.config.term.font_px = None;
                changed.push("terminal font size to default".into());
            } else {
                match v.as_f64().or_else(|| s.trim_end_matches("px").trim().parse().ok()) {
                    Some(px) => {
                        let px = (px as f32).clamp(6.0, 72.0);
                        self.config.term.font_px = Some(px);
                        changed.push(format!("terminal font size to {px}px"));
                    }
                    None => return Err("term_font_px must be a number like 14".into()),
                }
            }
            term_changed = true;
        }
        if let Some(s) = arg("term_scheme") {
            let name = s.to_ascii_lowercase();
            set_opt(&mut self.config.term.scheme, &name, is_default(s) || name == "campbell");
            changed.push(format!("terminal scheme to {name}"));
            term_changed = true;
        }
        if let Some(s) = arg("term_bg") {
            set_opt(&mut self.config.term.bg, s, is_default(s));
            changed.push(format!("terminal background to {s}"));
            term_changed = true;
        }
        if let Some(s) = arg("term_fg") {
            set_opt(&mut self.config.term.fg, s, is_default(s));
            changed.push(format!("terminal text to {s}"));
            term_changed = true;
        }
        if changed.is_empty() {
            return Err("tell me what to change — bar height, bar background/text, accent, \
                        background, or the terminal's font/font size/scheme/colours"
                .into());
        }
        crate::config::save(&self.config);
        self.rebuild_theme();
        if term_changed {
            self.rebuild_term_style(); // re-fits terminal grids if the cell size moved
        }
        if height_changed {
            self.refresh_visibility(); // content band moved → reposition webviews/panes
        }
        self.window.request_redraw();
        Ok(format!("set {}", changed.join(", ")))
    }

    /// Bare `:theme`: open config.toml in a terminal editor tab for direct editing.
    /// The file is (re)written first so it exists and shows the live state. When the
    /// editor terminal closes, the config is re-read and applied — edit → save →
    /// quit is the whole loop. `$EDITOR`/`$VISUAL` wins; else the first terminal
    /// editor found on PATH; with none installed it falls back to notepad (detached)
    /// plus `:theme reload` to apply by hand.
    pub(crate) fn edit_theme_config(&mut self) {
        crate::config::save(&self.config);
        let Some(path) = crate::config::config_path() else {
            self.set_error("no config directory available");
            return;
        };
        let path = path.to_string_lossy().into_owned();
        let editor = std::env::var("EDITOR")
            .or_else(|_| std::env::var("VISUAL"))
            .ok()
            .map(|e| e.split_whitespace().map(String::from).collect::<Vec<_>>())
            .filter(|v| !v.is_empty() && crate::program_exists(&v[0]))
            .or_else(|| {
                ["nvim", "vim", "hx", "helix", "nano", "vi"]
                    .iter()
                    .find(|e| crate::program_exists(e))
                    .map(|e| vec![e.to_string()])
            });
        match editor {
            Some(mut cmd) => {
                cmd.push(path);
                if let Some(id) = self.open_terminal_cmd(cmd) {
                    self.config_edit_term = Some(id);
                    self.set_status("editing config.toml — save & quit the editor to apply");
                }
            }
            None => {
                let _ = std::process::Command::new("notepad").arg(&path).spawn();
                self.set_status("opened config.toml in notepad — save, then :theme reload to apply");
            }
        }
        self.window.request_redraw();
    }

    /// Re-read config.toml from disk and apply it live — `:theme reload`, and the
    /// automatic apply when the `:theme` editor terminal closes.
    pub(crate) fn reload_config(&mut self) {
        self.config = crate::config::load();
        self.rebuild_theme();
        self.rebuild_term_style();
        self.refresh_visibility(); // the bar height may have changed → reflow panes
        self.set_status("config reloaded");
        self.window.request_redraw();
    }

    /// A background scheme download finished ([`UserEvent::SchemeInstalled`]): apply
    /// it on success, report either way — into the initiating `:ai` chat when there
    /// was one (falling back to the status bar when that chat isn't on screen).
    pub(crate) fn finish_scheme_install(
        &mut self,
        ai_id: Option<u64>,
        result: Result<String, String>,
    ) {
        self.installing_scheme = None;
        match result {
            Ok(name) => {
                self.config.term.scheme = Some(crate::config::scheme_key(&name));
                crate::config::save(&self.config);
                self.rebuild_term_style();
                let in_chat = ai_id
                    .is_some_and(|id| self.ai_note(id, &format!("Done — installed the {name} terminal scheme and applied it.")));
                if !in_chat {
                    self.set_status(format!("installed terminal scheme {name} and applied it"));
                }
            }
            Err(e) => {
                let in_chat =
                    ai_id.is_some_and(|id| self.ai_note(id, &format!("Scheme install failed: {e}")));
                if !in_chat {
                    self.set_error(format!("scheme install: {e}"));
                }
            }
        }
        self.window.request_redraw();
    }

    /// One-line summary of the current appearance overrides, for `:theme show` —
    /// each customized field as `key=value`, or a hint when everything is default.
    pub(crate) fn theme_summary(&self) -> String {
        let t = &self.config.theme;
        let tc = &self.config.term;
        let mut parts: Vec<String> = Vec::new();
        if let Some(p) = t.bar_height_pct {
            parts.push(format!("bar_height_pct={p}"));
        }
        for (k, v) in [
            ("bar_bg", &t.bar_bg),
            ("bar_fg", &t.bar_fg),
            ("accent", &t.accent),
            ("bg", &t.bg),
            ("term_font", &tc.font),
            ("term_scheme", &tc.scheme),
            ("term_bg", &tc.bg),
            ("term_fg", &tc.fg),
        ] {
            if let Some(v) = v {
                parts.push(format!("{k}={v}"));
            }
        }
        if let Some(px) = tc.font_px {
            parts.push(format!("term_font_px={px}"));
        }
        if parts.is_empty() {
            "theme: all defaults — :theme <key> <value> to customize".into()
        } else {
            format!("theme: {}", parts.join("  "))
        }
    }

    /// Expand a leading command alias, up to a small depth so chained aliases work
    /// without an infinite loop. Returns the rewritten line, or `None` if the head
    /// word isn't an alias. Typed args after the alias are appended to its expansion.
    pub(crate) fn expand_alias(&self, line: &str) -> Option<String> {
        let mut cur = line.to_string();
        let mut changed = false;
        for _ in 0..8 {
            let (verb, rest) = match cur.split_once(char::is_whitespace) {
                Some((v, r)) => (v.to_string(), r.trim().to_string()),
                None => (cur.clone(), String::new()),
            };
            let Some(exp) = self.config.aliases.get(&verb) else { break };
            cur = if rest.is_empty() { exp.clone() } else { format!("{exp} {rest}") };
            changed = true;
        }
        changed.then_some(cur)
    }

    /// Run an action and report the outcome in the status bar — the command-bar
    /// convenience over [`perform`](App::perform). (The assistant calls `perform`
    /// directly so it can show the result text in the chat.)
    pub(crate) fn run_action(&mut self, name: &str, args: Value) {
        match self.perform(name, &args) {
            Ok(msg) => self.set_status(msg),
            Err(e) => self.set_error(e),
        }
        self.window.request_redraw();
    }

    /// `clear <what> [period]` — wipe a slice of stored data, optionally only the
    /// part from a recent time window. `history` clears the shell's own visited list
    /// (windowed by visit time) and reopen stack, plus the engine's history;
    /// `cookies`/`cache`/`all` go through the WebView2 profile and so need a web tab
    /// open. `period` omitted (or `all`) means all time.
    fn action_clear(&mut self, what: &str, period: Option<&str>) -> Result<String, String> {
        let (secs, disp) = resolve_period(period)?;
        let now = now_epoch();
        // The engine wants an absolute [start, end] window in epoch seconds; `None`
        // = all time. The shell history uses the same cutoff as a `u64`.
        let range = secs.map(|s| (now.saturating_sub(s) as f64, now as f64));
        let cutoff = secs.map(|s| now.saturating_sub(s));
        match what {
            "history" | "hist" => {
                let removed = self.clear_history_window(cutoff);
                // Best-effort: also drop the engine's own history for the same window.
                self.clear_engine(DataKind::History, range);
                self.window.request_redraw();
                let plural = if removed == 1 { "entry" } else { "entries" };
                Ok(format!("cleared {removed} history {plural} ({disp})"))
            }
            "cookies" => self.clear_engine_required(DataKind::Cookies, range, &format!("cookies ({disp})")),
            "cache" => self.clear_engine_required(DataKind::Cache, range, &format!("cache ({disp})")),
            "all" | "everything" => {
                // A full wipe takes the shell's own lists with it (windowed too), then
                // the engine profile. With no web tab we can't reach the engine, but
                // the shell part still happened — report that rather than erroring.
                let removed = self.clear_history_window(cutoff);
                self.window.request_redraw();
                match self.clear_engine_required(DataKind::All, range, &format!("all browsing data ({disp})")) {
                    Ok(msg) => Ok(msg),
                    Err(_) => Ok(format!(
                        "cleared {removed} local history ({disp}); open a page to also wipe cookies & cache"
                    )),
                }
            }
            "" => Err("clear what? — history, cookies, cache, or all".into()),
            other => Err(format!("can't clear '{other}' — try history, cookies, cache, or all")),
        }
    }

    /// Drop visited-history entries inside the time window: `None` cutoff wipes
    /// everything (and the reopen stack); `Some(cutoff)` removes only entries
    /// stamped at-or-after `cutoff` (entries with an unknown time of `0`, e.g.
    /// restored from an old session, survive any windowed clear). Returns the count
    /// removed; keeps [`history`](App::history)/[`history_at`](App::history_at) aligned.
    fn clear_history_window(&mut self, cutoff: Option<u64>) -> usize {
        match cutoff {
            None => {
                let n = self.history.len();
                self.history.clear();
                self.history_at.clear();
                self.closed_tabs.clear();
                n
            }
            Some(c) => {
                let mut urls = Vec::with_capacity(self.history.len());
                let mut times = Vec::with_capacity(self.history_at.len());
                let mut removed = 0;
                for (u, &at) in self.history.iter().zip(self.history_at.iter()) {
                    if at >= c {
                        removed += 1;
                    } else {
                        urls.push(u.clone());
                        times.push(at);
                    }
                }
                self.history = urls;
                self.history_at = times;
                removed
            }
        }
    }

    /// Fire an engine data-clear if a web tab exists, ignoring the no-engine case
    /// (used for the bonus engine-history wipe alongside the shell's own list).
    fn clear_engine(&self, kind: DataKind, range: Option<(f64, f64)>) {
        if let Some(wv) = self.any_webview() {
            let _ = crate::data::clear(wv, kind, range, String::new(), self.acting_ai, self.proxy.clone());
        }
    }

    /// Fire an engine data-clear, requiring a live web tab to reach the shared
    /// profile. Returns the "clearing…" acknowledgement; the real confirmation
    /// arrives later as [`UserEvent::DataCleared`](crate::UserEvent::DataCleared).
    fn clear_engine_required(
        &self,
        kind: DataKind,
        range: Option<(f64, f64)>,
        label: &str,
    ) -> Result<String, String> {
        let Some(wv) = self.any_webview() else {
            return Err(format!("open a page first, then clear {label}"));
        };
        crate::data::clear(wv, kind, range, label.to_string(), self.acting_ai, self.proxy.clone())?;
        Ok(format!("clearing {label}…"))
    }
}

/// Resolve a `period` argument into `(duration_secs, display)`: `None`/`all`/empty
/// → all time (`None` duration); a token like `15m`/`1h`/`24h`/`7d`/`1w` → its
/// length in seconds. Accepts the bare unit words too (`hour`/`day`/`week`). `Err`
/// for an unparseable token, so a typo is reported rather than silently wiping all.
fn resolve_period(period: Option<&str>) -> Result<(Option<u64>, String), String> {
    let raw = period.map(|p| p.trim().to_lowercase());
    let secs = match raw.as_deref() {
        None | Some("") | Some("all") | Some("everything") | Some("alltime") => None,
        Some(p) => Some(parse_duration(p)?),
    };
    let disp = match (&raw, secs) {
        (_, None) => "all time".to_string(),
        (Some(r), Some(_)) => format!("last {r}"),
        (None, Some(_)) => unreachable!(),
    };
    Ok((secs, disp))
}

/// Parse a compact duration token (`15m`, `1h`, `24h`, `7d`, `1w`, or a bare
/// `hour`/`day`/`week`) into seconds.
fn parse_duration(p: &str) -> Result<u64, String> {
    match p {
        "min" | "minute" => return Ok(60),
        "hour" => return Ok(3600),
        "day" => return Ok(86_400),
        "week" => return Ok(604_800),
        _ => {}
    }
    let split = p.find(|c: char| !c.is_ascii_digit()).unwrap_or(p.len());
    let (num, unit) = p.split_at(split);
    let n: u64 = num.parse().map_err(|_| format!("don't understand the time period '{p}'"))?;
    let mult = match unit {
        "m" | "min" | "mins" | "minute" | "minutes" => 60,
        "h" | "hr" | "hrs" | "hour" | "hours" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "wk" | "week" | "weeks" => 604_800,
        other => return Err(format!("unknown time unit '{other}' — use m, h, d, or w")),
    };
    n.checked_mul(mult).ok_or_else(|| "that time period is too large".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_specs_expose_params_for_rendering() {
        // The `:commands` cards and the Groq tool schema both render straight from
        // ACTIONS; sanity-check the param shapes they rely on.
        let spec = |name: &str| ACTIONS.iter().find(|a| a.name == name).unwrap();
        let clear = spec("clear");
        assert!(clear.params[0].required && clear.params[0].values == ["history", "cookies", "cache", "all"]);
        assert!(!clear.params[1].required);
        // Free-text params have no fixed values; their NAME is the placeholder.
        assert!(spec("alias").params.iter().all(|p| p.values.is_empty() && p.required));
        assert!(spec("restore").params.is_empty());
    }

    #[test]
    fn groq_tools_emit_enum_only_for_fixed_params() {
        let tools = groq_tools();
        let by_name = |n: &str| {
            tools
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["function"]["name"] == n)
                .unwrap()["function"]["parameters"]["properties"]
                .clone()
        };
        // `clear.what` is constrained; `alias.name` is free text (no enum).
        assert!(by_name("clear")["what"].get("enum").is_some());
        assert!(by_name("alias")["name"].get("enum").is_none());
        assert_eq!(by_name("alias")["name"]["type"], "string");
    }

    #[test]
    fn periods_parse_to_seconds_or_all_time() {
        assert_eq!(resolve_period(None).unwrap().0, None);
        assert_eq!(resolve_period(Some("all")).unwrap().0, None);
        assert_eq!(resolve_period(Some("15m")).unwrap().0, Some(900));
        assert_eq!(resolve_period(Some("1h")).unwrap().0, Some(3600));
        assert_eq!(resolve_period(Some("24h")).unwrap().0, Some(86_400));
        assert_eq!(resolve_period(Some("7d")).unwrap().0, Some(604_800));
        assert_eq!(resolve_period(Some("2 hours".replace(' ', "").as_str())).unwrap().0, Some(7200));
        assert!(resolve_period(Some("banana")).is_err());
    }

    #[test]
    fn period_display_reads_naturally() {
        assert_eq!(resolve_period(Some("24h")).unwrap().1, "last 24h");
        assert_eq!(resolve_period(None).unwrap().1, "all time");
    }

    #[test]
    fn positional_zips_tokens_onto_names() {
        let v = positional(&["what", "period"], "cache 15m");
        assert_eq!(v["what"], "cache");
        assert_eq!(v["period"], "15m");
        // Missing trailing tokens are simply absent (period defaults to all time).
        let v = positional(&["what", "period"], "cookies");
        assert_eq!(v["what"], "cookies");
        assert!(v.get("period").is_none());
    }
}
