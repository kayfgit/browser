//! The `:buttons` page — the clickable buttons on the tab strip (or the sidebar's
//! top row) in an engine-free vim tab (`browser://buttons`), plus a catalog of
//! ready-made ones to add. Your buttons come first: Enter runs the row (like a
//! click), `d` removes it, `K`/`J` move it up/down. Below them, Enter on a "more
//! buttons" row adds that one to the end. Every change goes through the same
//! `button` action as `:button`/`:unbutton` and the assistant, and is saved at once.

use crate::config::Button;
use crate::tabs::{TabContent, TabNav};
use crate::{vim, App, Tab};

const URL: &str = "browser://buttons";

/// A ready-made button: its label, the command it runs, and what that does. Labels
/// stick to glyphs the UI font (or its symbol fallback) draws.
pub(crate) struct Preset {
    pub(crate) label: &'static str,
    pub(crate) run: &'static str,
    pub(crate) what: &'static str,
}

/// The "more buttons" catalog on the `:buttons` page, in the order it lists them.
pub(crate) const PRESETS: &[Preset] = &[
    Preset {
        label: "←",
        run: "back",
        what: "go back",
    },
    Preset {
        label: "→",
        run: "forward",
        what: "go forward",
    },
    Preset {
        label: "↻",
        run: "reload",
        what: "reload the page",
    },
    Preset {
        label: "+",
        run: "tabopen",
        what: "new tab (asks for the address)",
    },
    Preset {
        label: "×",
        run: "close",
        what: "close this tab",
    },
    Preset {
        label: "↶",
        run: "reopen",
        what: "reopen the last closed tab",
    },
    Preset {
        label: "☆",
        run: "save",
        what: "save this page",
    },
    Preset {
        label: "★",
        run: "saved",
        what: "your saved pages",
    },
    Preset {
        label: "≡",
        run: "history",
        what: "history",
    },
    Preset {
        label: "⤓",
        run: "downloads",
        what: "downloads",
    },
    Preset {
        label: "◫",
        run: "vsplit",
        what: "split side by side",
    },
    Preset {
        label: "⤢",
        run: "fullscreen",
        what: "fullscreen",
    },
    Preset {
        label: "✦",
        run: "ai",
        what: "the assistant",
    },
    Preset {
        label: ">_",
        run: "te",
        what: "a terminal",
    },
    Preset {
        label: "?",
        run: "help",
        what: "every command",
    },
];

/// What a line of the page stands for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Row {
    /// A header, hint or blank line.
    Text,
    /// Your button at this index.
    Current(usize),
    /// The [`PRESETS`] entry at this index.
    Preset(usize),
}

/// The presets not already on the bar (same label running the same command).
fn missing(current: &[Button]) -> impl Iterator<Item = (usize, &'static Preset)> + '_ {
    PRESETS
        .iter()
        .enumerate()
        .filter(move |(_, p)| !current.iter().any(|b| b.label == p.label && b.run == p.run))
}

/// The page text for `current`, and what each of its lines is.
pub(crate) fn page(current: &[Button]) -> (Vec<String>, Vec<Row>) {
    let mut lines = vec![
        "buttons — Enter runs one · d removes it · K/J move it · :button <label> <command> adds your own".to_string(),
        String::new(),
    ];
    let mut rows = vec![Row::Text, Row::Text];
    if current.is_empty() {
        lines.push(
            "  (none — Enter on one below adds it, :buttons reset brings the defaults back)".into(),
        );
        rows.push(Row::Text);
    }
    // The label goes last: a glyph from the symbol fallback face can be wider than
    // a cell, which would push every column after it out of line.
    for (i, b) in current.iter().enumerate() {
        lines.push(format!("  :{:<40} {}", b.run, b.label));
        rows.push(Row::Current(i));
    }
    let more: Vec<_> = missing(current).collect();
    if !more.is_empty() {
        lines.extend([String::new(), "more buttons — Enter adds one".into()]);
        rows.extend([Row::Text, Row::Text]);
        for (i, p) in more {
            lines.push(format!("  {:<30} :{:<11} {}", p.what, p.run, p.label));
            rows.push(Row::Preset(i));
        }
    }
    (lines, rows)
}

impl App {
    /// `:buttons` — open the page, or refresh it in place (keeping the cursor row)
    /// when it's already the active tab.
    pub(crate) fn open_buttons_page(&mut self) {
        let (lines, _) = page(&self.buttons());
        if self.active_url() == Some(URL) {
            self.set_buttons_lines(lines, None);
            return;
        }
        self.place_tab(
            Tab {
                id: crate::layout::TabId::new(),
                url: URL.into(),
                nojs: false,
                read: false,
                research: false,
                private: false,
                nav: TabNav::default(),
                content: TabContent::Pager(vim::TextBuffer::new(lines)),
            },
            true,
        );
        self.window.set_focus();
        self.clear_status();
    }

    /// Re-render the page if it's on screen, after a change made from anywhere (the
    /// page's own keys, `:button`, the assistant).
    pub(crate) fn refresh_buttons_page(&mut self) {
        if self.active_url() == Some(URL) {
            let (lines, _) = page(&self.buttons());
            self.set_buttons_lines(lines, None);
        }
    }

    /// Swap the page's text, putting the cursor on `cy` (or keeping its row), clamped.
    fn set_buttons_lines(&mut self, lines: Vec<String>, cy: Option<usize>) {
        if let Some(buf) = self
            .active
            .and_then(|i| self.tabs.get_mut(i))
            .and_then(|t| t.vim_mut())
        {
            let cy = cy.unwrap_or(buf.cy);
            buf.set_lines(lines);
            buf.anchor = None;
            buf.cy = cy.min(buf.lines.len().saturating_sub(1));
            buf.cx = 0;
        }
        self.window.request_redraw();
    }

    /// The row under the cursor, when the page is the active tab.
    fn buttons_row(&self) -> Option<Row> {
        if self.active_url() != Some(URL) {
            return None;
        }
        let cy = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim())?
            .cy;
        page(&self.buttons()).1.get(cy).copied()
    }

    /// Enter: run your button under the cursor, or add the preset under it.
    pub(crate) fn buttons_page_enter(&mut self) {
        match self.buttons_row() {
            Some(Row::Current(i)) => self.press_button(i),
            Some(Row::Preset(i)) => {
                let p = &PRESETS[i];
                self.run_action(
                    "button",
                    serde_json::json!({ "do": "add", "label": p.label, "command": p.run }),
                );
                // The new button's row pushed the catalog down one: step onto the
                // preset that followed the one just added, ready for the next Enter.
                let (lines, _) = page(&self.buttons());
                let cy = self
                    .active
                    .and_then(|i| self.tabs.get(i))
                    .and_then(|t| t.vim())
                    .map(|b| b.cy + 1);
                self.set_buttons_lines(lines, cy);
            }
            _ => {}
        }
    }

    /// `d`: remove your button under the cursor.
    pub(crate) fn buttons_page_delete(&mut self) {
        if let Some(Row::Current(i)) = self.buttons_row() {
            let label = self.buttons()[i].label.clone();
            self.run_action(
                "button",
                serde_json::json!({ "do": "remove", "label": label }),
            );
        }
    }

    /// `K`/`J`: move your button under the cursor one place up (`-1`) or down (`1`),
    /// the cursor going with it.
    pub(crate) fn buttons_page_move(&mut self, by: isize) {
        let Some(Row::Current(i)) = self.buttons_row() else {
            return;
        };
        let mut buttons = self.buttons();
        let Some(j) = i.checked_add_signed(by).filter(|j| *j < buttons.len()) else {
            return;
        };
        buttons.swap(i, j);
        self.config.buttons = Some(buttons);
        crate::config::save(&self.config);
        self.refresh_visibility();
        let (lines, rows) = page(&self.buttons());
        let cy = rows.iter().position(|r| *r == Row::Current(j));
        self.set_buttons_lines(lines, cy);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(label: &str, run: &str) -> Button {
        Button {
            label: label.into(),
            run: run.into(),
        }
    }

    #[test]
    fn rows_map_to_buttons_then_the_presets_not_on_the_bar() {
        let current = [button("←", "back"), button("✉", "open -t mail.example.com")];
        let (lines, rows) = page(&current);
        assert_eq!(lines.len(), rows.len());
        assert_eq!(rows[2], Row::Current(0));
        assert_eq!(rows[3], Row::Current(1));
        assert!(lines[3].contains(":open -t mail.example.com"));
        // ← back is already on the bar, so the catalog starts at →.
        let presets: Vec<usize> = rows
            .iter()
            .filter_map(|r| match r {
                Row::Preset(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(presets.len(), PRESETS.len() - 1);
        assert_eq!(PRESETS[presets[0]].run, "forward");
    }

    #[test]
    fn an_empty_bar_says_so_and_offers_every_preset() {
        let (lines, rows) = page(&[]);
        assert!(lines[2].contains("none"));
        assert_eq!(rows[2], Row::Text);
        assert_eq!(
            rows.iter().filter(|r| matches!(r, Row::Preset(_))).count(),
            PRESETS.len()
        );
    }

    #[test]
    fn presets_are_valid_buttons() {
        for p in PRESETS {
            assert_eq!(p.label.split_whitespace().count(), 1, "{}", p.label);
            assert!(
                crate::commands::BUILTIN_VERBS.contains(&p.run.split_whitespace().next().unwrap()),
                "{} isn't a command",
                p.run
            );
        }
    }
}
