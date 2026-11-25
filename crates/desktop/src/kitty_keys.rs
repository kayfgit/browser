//! The kitty keyboard protocol: how a terminal reports keys to a program that asks for
//! more than the legacy encoding can say (Ctrl+I apart from Tab, Ctrl+Shift chords,
//! key releases). <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>
//!
//! A program pushes the enhancements it wants (`CSI > flags u`); alacritty_terminal
//! keeps that stack and answers the program's query (`CSI ? u`), and [`encode`] turns
//! a key into the matching sequence. ConPTY passes both the negotiation and the
//! encoded keys through untouched (checked against Windows' conhost and the bundled
//! ConPTY), so it works for Windows programs and WSL alike.

use alacritty_terminal::term::TermMode;
use tao::keyboard::Key;

/// The enhancements a program asked for.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    /// 1: Esc and modified keys get unambiguous `CSI … u` codes.
    pub(crate) disambiguate: bool,
    /// 2: repeats and releases are reported too.
    pub(crate) event_types: bool,
    /// 4: the shifted key is reported alongside the base one.
    pub(crate) alternate_keys: bool,
    /// 8: every key, text keys included, is an escape code.
    pub(crate) all_as_escapes: bool,
    /// 16: with 8, the text a key types rides along.
    pub(crate) associated_text: bool,
}

impl Flags {
    pub(crate) fn from_mode(mode: TermMode) -> Self {
        Flags {
            disambiguate: mode.contains(TermMode::DISAMBIGUATE_ESC_CODES),
            event_types: mode.contains(TermMode::REPORT_EVENT_TYPES),
            alternate_keys: mode.contains(TermMode::REPORT_ALTERNATE_KEYS),
            all_as_escapes: mode.contains(TermMode::REPORT_ALL_KEYS_AS_ESC),
            associated_text: mode.contains(TermMode::REPORT_ASSOCIATED_TEXT),
        }
    }

    /// Whether the program asked for anything (else the legacy encoding applies).
    pub(crate) fn any(self) -> bool {
        self.disambiguate
            || self.event_types
            || self.alternate_keys
            || self.all_as_escapes
            || self.associated_text
    }
}

/// The modifier keys held.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Mods {
    pub(crate) shift: bool,
    pub(crate) alt: bool,
    pub(crate) ctrl: bool,
    pub(crate) meta: bool,
}

impl Mods {
    /// The protocol's modifier number: 1 + shift 1, alt 2, ctrl 4, super 8.
    fn value(self) -> u32 {
        1 + self.shift as u32
            + ((self.alt as u32) << 1)
            + ((self.ctrl as u32) << 2)
            + ((self.meta as u32) << 3)
    }

    /// Whether a key held with these still just types its text (nothing but Shift).
    fn types_text(self) -> bool {
        !self.alt && !self.ctrl && !self.meta
    }
}

/// What happened to the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Press,
    Repeat,
    Release,
}

impl Event {
    fn code(self) -> u32 {
        match self {
            Event::Press => 1,
            Event::Repeat => 2,
            Event::Release => 3,
        }
    }
}

/// How a key is written.
enum Form {
    /// `CSI code[:shifted] ; mods[:event] [; text] u`
    Unicode { code: u32, shifted: Option<u32> },
    /// `CSI 1 ; mods[:event] X`, or `CSI X` with nothing to add (arrows, Home, End,
    /// F1, F2, F4).
    Letter(char),
    /// `CSI n ; mods[:event] ~` (Insert, Delete, Page Up/Down, F3, F5–F12).
    Tilde(u32),
}

/// The sequence for a key under `flags`. `Some(bytes)` is what to send (empty: send
/// nothing, e.g. a release nobody asked for); `None` means the legacy encoding is
/// already right for this key, so the caller uses that. `unmodified` is the key the
/// layout gives without modifiers (its base code); `text` what the key types.
pub(crate) fn encode(
    key: &Key,
    unmodified: &Key,
    text: Option<&str>,
    mods: Mods,
    event: Event,
    flags: Flags,
) -> Option<Vec<u8>> {
    if event != Event::Press && !flags.event_types {
        return Some(Vec::new());
    }
    let legacy_ok = |modified: bool| !flags.all_as_escapes && !modified && event != Event::Release;
    let plain = mods.value() == 1;
    let form = match key {
        // Esc is the one key the legacy encoding can't keep apart (from Alt+key and
        // the start of every sequence), so it always gets a code.
        Key::Escape => Form::Unicode {
            code: 27,
            shifted: None,
        },
        // Enter, Tab and Backspace stay legacy unless modified.
        Key::Enter | Key::Tab | Key::Backspace => {
            if legacy_ok(!plain) {
                return None;
            }
            let code = match key {
                Key::Enter => 13,
                Key::Tab => 9,
                _ => 127,
            };
            Form::Unicode {
                code,
                shifted: None,
            }
        }
        Key::ArrowUp
        | Key::ArrowDown
        | Key::ArrowRight
        | Key::ArrowLeft
        | Key::Home
        | Key::End
        | Key::F1
        | Key::F2
        | Key::F4 => {
            if legacy_ok(!plain) {
                return None;
            }
            Form::Letter(match key {
                Key::ArrowUp => 'A',
                Key::ArrowDown => 'B',
                Key::ArrowRight => 'C',
                Key::ArrowLeft => 'D',
                Key::Home => 'H',
                Key::End => 'F',
                Key::F1 => 'P',
                Key::F2 => 'Q',
                _ => 'S',
            })
        }
        Key::Insert
        | Key::Delete
        | Key::PageUp
        | Key::PageDown
        | Key::F3
        | Key::F5
        | Key::F6
        | Key::F7
        | Key::F8
        | Key::F9
        | Key::F10
        | Key::F11
        | Key::F12 => {
            if legacy_ok(!plain) {
                return None;
            }
            Form::Tilde(match key {
                Key::Insert => 2,
                Key::Delete => 3,
                Key::PageUp => 5,
                Key::PageDown => 6,
                Key::F3 => 13,
                Key::F5 => 15,
                Key::F6 => 17,
                Key::F7 => 18,
                Key::F8 => 19,
                Key::F9 => 20,
                Key::F10 => 21,
                Key::F11 => 23,
                _ => 24,
            })
        }
        Key::Character(_) | Key::Space => {
            let typed = match key {
                Key::Character(s) => s.chars().next()?,
                _ => ' ',
            };
            // A key that just types (no Ctrl/Alt/Super) stays text, unless every key
            // is to be an escape code; its release isn't reported then either.
            if !flags.all_as_escapes && mods.types_text() {
                return if event == Event::Release {
                    Some(Vec::new())
                } else {
                    None
                };
            }
            // The base code is the key's own, unshifted, lowercase.
            let base = match unmodified {
                Key::Character(s) => s.chars().next().unwrap_or(typed),
                Key::Space => ' ',
                _ => typed,
            };
            let base = base.to_lowercase().next().unwrap_or(base);
            let shifted =
                (flags.alternate_keys && mods.shift && typed != base).then_some(typed as u32);
            Form::Unicode {
                code: base as u32,
                shifted,
            }
        }
        // Modifier keys on their own, and keys with no terminal meaning: not reported.
        _ => return Some(Vec::new()),
    };

    // `mods[:event]`, left out when it says nothing beyond a plain press.
    let mods_field = if event != Event::Press {
        Some(format!("{}:{}", mods.value(), event.code()))
    } else if !plain {
        Some(mods.value().to_string())
    } else {
        None
    };
    let mut out = String::from("\x1b[");
    match form {
        Form::Unicode { code, shifted } => {
            out.push_str(&code.to_string());
            if let Some(s) = shifted {
                out.push_str(&format!(":{s}"));
            }
            // The text a key types, as code points, with flags 8 + 16.
            let text = text.filter(|t| {
                flags.all_as_escapes
                    && flags.associated_text
                    && event != Event::Release
                    && mods.types_text()
                    && !t.chars().any(char::is_control)
            });
            match (mods_field, text) {
                (m, Some(t)) => {
                    out.push(';');
                    out.push_str(&m.unwrap_or_else(|| "1".into()));
                    out.push(';');
                    let cps: Vec<String> = t.chars().map(|c| (c as u32).to_string()).collect();
                    out.push_str(&cps.join(":"));
                }
                (Some(m), None) => {
                    out.push(';');
                    out.push_str(&m);
                }
                (None, None) => {}
            }
            out.push('u');
        }
        Form::Letter(c) => {
            if let Some(m) = mods_field {
                out.push_str(&format!("1;{m}"));
            }
            out.push(c);
        }
        Form::Tilde(n) => {
            out.push_str(&n.to_string());
            if let Some(m) = mods_field {
                out.push_str(&format!(";{m}"));
            }
            out.push('~');
        }
    }
    Some(out.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DISAMBIGUATE: Flags = Flags {
        disambiguate: true,
        event_types: false,
        alternate_keys: false,
        all_as_escapes: false,
        associated_text: false,
    };

    fn ch(s: &'static str) -> Key<'static> {
        Key::Character(s)
    }

    fn enc(key: &Key, unmodified: &Key, mods: Mods, event: Event, flags: Flags) -> Option<String> {
        let text = match key {
            Key::Character(s) => Some(*s),
            _ => None,
        };
        encode(key, unmodified, text, mods, event, flags).map(|b| String::from_utf8(b).unwrap())
    }

    const NONE: Mods = Mods {
        shift: false,
        alt: false,
        ctrl: false,
        meta: false,
    };
    const CTRL: Mods = Mods { ctrl: true, ..NONE };
    const SHIFT: Mods = Mods {
        shift: true,
        ..NONE
    };
    const CTRL_SHIFT: Mods = Mods {
        ctrl: true,
        shift: true,
        ..NONE
    };

    #[test]
    fn disambiguate_gives_esc_and_chords_their_own_codes() {
        let p = Event::Press;
        assert_eq!(
            enc(&Key::Escape, &Key::Escape, NONE, p, DISAMBIGUATE).unwrap(),
            "\x1b[27u"
        );
        // Ctrl+i is no longer Tab, Ctrl+Shift+a no longer Ctrl+a.
        assert_eq!(
            enc(&ch("i"), &ch("i"), CTRL, p, DISAMBIGUATE).unwrap(),
            "\x1b[105;5u"
        );
        assert_eq!(
            enc(&ch("A"), &ch("a"), CTRL_SHIFT, p, DISAMBIGUATE).unwrap(),
            "\x1b[97;6u"
        );
        // Text keys and plain Enter/Tab/arrows keep their legacy bytes.
        assert_eq!(enc(&ch("a"), &ch("a"), NONE, p, DISAMBIGUATE), None);
        assert_eq!(enc(&ch("A"), &ch("a"), SHIFT, p, DISAMBIGUATE), None);
        assert_eq!(enc(&Key::Enter, &Key::Enter, NONE, p, DISAMBIGUATE), None);
        assert_eq!(
            enc(&Key::ArrowUp, &Key::ArrowUp, NONE, p, DISAMBIGUATE),
            None
        );
        // Modified, they get codes.
        assert_eq!(
            enc(&Key::Enter, &Key::Enter, SHIFT, p, DISAMBIGUATE).unwrap(),
            "\x1b[13;2u"
        );
        assert_eq!(
            enc(&Key::Tab, &Key::Tab, SHIFT, p, DISAMBIGUATE).unwrap(),
            "\x1b[9;2u"
        );
        assert_eq!(
            enc(&Key::ArrowUp, &Key::ArrowUp, CTRL, p, DISAMBIGUATE).unwrap(),
            "\x1b[1;5A"
        );
        assert_eq!(
            enc(&Key::F3, &Key::F3, CTRL, p, DISAMBIGUATE).unwrap(),
            "\x1b[13;5~"
        );
        assert_eq!(
            enc(&ch(" "), &ch(" "), CTRL, p, DISAMBIGUATE).unwrap(),
            "\x1b[32;5u"
        );
    }

    #[test]
    fn event_types_report_repeats_and_releases() {
        let f = Flags {
            event_types: true,
            ..DISAMBIGUATE
        };
        assert_eq!(
            enc(&ch("a"), &ch("a"), CTRL, Event::Repeat, f).unwrap(),
            "\x1b[97;5:2u"
        );
        assert_eq!(
            enc(&ch("a"), &ch("a"), CTRL, Event::Release, f).unwrap(),
            "\x1b[97;5:3u"
        );
        assert_eq!(
            enc(&Key::Escape, &Key::Escape, NONE, Event::Release, f).unwrap(),
            "\x1b[27;1:3u"
        );
        assert_eq!(
            enc(&Key::ArrowLeft, &Key::ArrowLeft, NONE, Event::Release, f).unwrap(),
            "\x1b[1;1:3D"
        );
        assert_eq!(
            enc(&Key::Delete, &Key::Delete, NONE, Event::Release, f).unwrap(),
            "\x1b[3;1:3~"
        );
        // A text key's release isn't reported, and nothing is without flag 2.
        assert_eq!(
            enc(&ch("a"), &ch("a"), NONE, Event::Release, f).unwrap(),
            ""
        );
        assert_eq!(
            enc(&ch("a"), &ch("a"), CTRL, Event::Release, DISAMBIGUATE).unwrap(),
            ""
        );
    }

    #[test]
    fn all_keys_as_escapes_with_alternates_and_text() {
        let f = Flags {
            all_as_escapes: true,
            alternate_keys: true,
            associated_text: true,
            ..DISAMBIGUATE
        };
        let p = Event::Press;
        assert_eq!(
            enc(&ch("a"), &ch("a"), NONE, p, f).unwrap(),
            "\x1b[97;1;97u"
        );
        assert_eq!(
            enc(&ch("A"), &ch("a"), SHIFT, p, f).unwrap(),
            "\x1b[97:65;2;65u"
        );
        assert_eq!(
            enc(&Key::Enter, &Key::Enter, NONE, p, f).unwrap(),
            "\x1b[13u"
        );
        assert_eq!(
            enc(&Key::ArrowUp, &Key::ArrowUp, NONE, p, f).unwrap(),
            "\x1b[A"
        );
        // Without associated text, just the code.
        let f = Flags {
            associated_text: false,
            ..f
        };
        assert_eq!(enc(&ch("a"), &ch("a"), NONE, p, f).unwrap(), "\x1b[97u");
    }

    #[test]
    fn the_base_code_comes_from_the_unmodified_key() {
        // Shift+1 types '!' on a US layout, but its code is still '1'.
        let f = Flags {
            alternate_keys: true,
            ..DISAMBIGUATE
        };
        assert_eq!(
            enc(&ch("!"), &ch("1"), CTRL_SHIFT, Event::Press, f).unwrap(),
            "\x1b[49:33;6u"
        );
    }

    #[test]
    fn flags_come_from_the_terminal_mode() {
        assert!(!Flags::from_mode(TermMode::empty()).any());
        let f = Flags::from_mode(TermMode::DISAMBIGUATE_ESC_CODES | TermMode::REPORT_EVENT_TYPES);
        assert!(f.disambiguate && f.event_types && !f.all_as_escapes);
    }
}
