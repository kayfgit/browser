//! What the browser and a Trident helper say to each other: one JSON object per line,
//! commands on the helper's stdin, events on its stdout. Plain data, so both ends and
//! their tests share it.
use serde::{Deserialize, Serialize};

/// Browser → helper.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub(crate) enum Command {
    /// The scripts every top-level document gets (the shell bridge and friends).
    Init {
        script: String,
    },
    Navigate {
        url: String,
    },
    Reload,
    Go {
        forward: bool,
    },
    /// Page zoom in percent (100 = normal).
    Zoom {
        percent: i32,
    },
    /// Give the page the keyboard.
    Focus,
    Eval {
        script: String,
    },
    /// A real (trusted) click at a point in CSS pixels, for hints.
    Click {
        x: f64,
        y: f64,
    },
    Quit,
}

/// Helper → browser.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "ev", rename_all = "kebab-case")]
pub(crate) enum Event {
    /// The helper's window exists; the browser places it in the pane.
    Ready {
        hwnd: i64,
    },
    LoadStart,
    LoadEnd,
    Url {
        url: String,
    },
    /// Whether back / forward have somewhere to go.
    History {
        back: bool,
        forward: bool,
    },
    /// A page script called `window.__post` (see `decode_page_message`).
    Message {
        body: String,
    },
    /// The page asked for a new window (a `target=_blank` link, `window.open`): open it
    /// as a tab instead.
    NewWindow {
        url: String,
    },
    /// Something the user should know (a blocked download, a failed command).
    Notice {
        text: String,
    },
}

/// The most a single line may be; anything longer is dropped, not buffered.
pub(crate) const MAX_LINE: usize = 4 * 1024 * 1024;

pub(crate) fn encode<T: Serialize>(msg: &T) -> String {
    let mut line = serde_json::to_string(msg).unwrap_or_default();
    line.push('\n');
    line
}

pub(crate) fn decode<'a, T: Deserialize<'a>>(line: &'a str) -> Option<T> {
    if line.len() > MAX_LINE {
        return None;
    }
    serde_json::from_str(line.trim_end()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip_as_single_lines() {
        let cmd = Command::Eval {
            script: "a\nb \"q\"".into(),
        };
        let line = encode(&cmd);
        assert_eq!(line.matches('\n').count(), 1, "one line per message");
        assert_eq!(decode::<Command>(&line), Some(cmd));
        let ev = Event::History {
            back: true,
            forward: false,
        };
        assert_eq!(decode::<Event>(&encode(&ev)), Some(ev));
    }

    #[test]
    fn junk_is_ignored() {
        assert_eq!(decode::<Event>("not json"), None);
        assert_eq!(decode::<Event>(r#"{"ev":"format-c"}"#), None);
    }
}
