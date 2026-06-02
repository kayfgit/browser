//! Application state, the async event loop, and drawing.

use std::sync::Arc;

use anyhow::Result;
use browser_core::content::{Block, DocumentBuilder, Span};
use browser_core::{parse_command, Command, Config, Document, KeyConfig, Mode};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

/// The single concrete terminal type the app drives.
type Term = Terminal<CrosstermBackend<std::io::Stdout>>;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::dispatch::Dispatcher;
use crate::render::{line_text, render_document};

/// What the bottom bar is currently capturing.
enum InputMode {
    Normal,
    Command(String),
    Find(String),
    Follow(String),
}

/// One visited location, with its rendered document cached for instant back/forward.
struct HistEntry {
    mode: Mode,
    target: String,
    doc: Option<Document>,
}

/// Result of a background fetch, tagged with the generation that requested it.
struct Outcome {
    generation: u64,
    result: Result<Document>,
}

pub struct App {
    dispatcher: Arc<Dispatcher>,
    keys: KeyConfig,

    input: InputMode,
    status: String,
    loading: bool,

    doc: Option<Document>,
    lines: Vec<Line<'static>>,
    scroll: usize,
    last_width: usize,
    viewport_h: usize,

    history: Vec<HistEntry>,
    hist_pos: usize,

    /// Generation counter so stale fetches can be discarded.
    generation: u64,
    fetch_tx: UnboundedSender<Outcome>,
    fetch_rx: Option<UnboundedReceiver<Outcome>>,

    /// In-page find state: last query and matching line indices.
    find_matches: Vec<usize>,

    quit: bool,
    dirty: bool,
}

impl App {
    pub fn new(config: &Config, dispatcher: Arc<Dispatcher>) -> Self {
        let (fetch_tx, fetch_rx) = tokio::sync::mpsc::unbounded_channel();
        App {
            dispatcher,
            keys: config.keys.clone(),
            input: InputMode::Normal,
            status: String::new(),
            loading: false,
            doc: None,
            lines: Vec::new(),
            scroll: 0,
            last_width: 0,
            viewport_h: 0,
            history: Vec::new(),
            hist_pos: 0,
            generation: 0,
            fetch_tx,
            fetch_rx: Some(fetch_rx),
            find_matches: Vec::new(),
            quit: false,
            dirty: true,
        }
    }

    /// Drive the UI until the user quits.
    pub async fn run(
        mut self,
        terminal: &mut Term,
        initial: Option<String>,
        mut input_rx: UnboundedReceiver<Event>,
    ) -> Result<()> {
        let mut fetch_rx = self.fetch_rx.take().expect("run called once");
        match initial {
            // A leading ':' lets `browser :s query` reuse command parsing.
            Some(line) if line.trim_start().starts_with(':') => self.run_command(&line),
            Some(target) => {
                let mode = self.dispatcher.resolve_mode(&target);
                self.navigate_new(mode, target);
            }
            None => self.show_welcome(),
        }

        while !self.quit {
            self.prepare(terminal)?;
            terminal.draw(|f| self.draw(f))?;

            tokio::select! {
                Some(event) = input_rx.recv() => self.handle_event(event),
                Some(outcome) = fetch_rx.recv() => self.handle_outcome(outcome),
                else => break,
            }
        }
        Ok(())
    }

    /// (Re)render lines to the current width and clamp scroll, before drawing.
    fn prepare(&mut self, terminal: &mut Term) -> Result<()> {
        let size = terminal.size()?;
        let width = size.width as usize;
        self.viewport_h = size.height.saturating_sub(1) as usize;

        if self.dirty || width != self.last_width {
            self.lines = match &self.doc {
                Some(doc) => render_document(doc, width),
                None => Vec::new(),
            };
            self.last_width = width;
            self.dirty = false;
        }
        let max = self.max_scroll();
        if self.scroll > max {
            self.scroll = max;
        }
        Ok(())
    }

    fn draw(&self, f: &mut Frame) {
        let chunks = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
        let content = Paragraph::new(self.lines.clone()).scroll((self.scroll as u16, 0));
        f.render_widget(content, chunks[0]);
        f.render_widget(Paragraph::new(self.bar_line()), chunks[1]);
    }

    /// Build the bottom bar (status in Normal mode, otherwise the input prompt).
    fn bar_line(&self) -> Line<'static> {
        let style = Style::default().bg(Color::DarkGray).fg(Color::White);
        match &self.input {
            InputMode::Command(buf) => Line::styled(format!(":{buf}"), style),
            InputMode::Find(buf) => Line::styled(format!("/{buf}"), style),
            InputMode::Follow(buf) => Line::styled(format!("follow [{buf}]"), style),
            InputMode::Normal => {
                let mode = self
                    .history
                    .get(self.hist_pos)
                    .map(|h| h.mode.as_str())
                    .unwrap_or("—");
                let title = self.doc.as_ref().map(|d| d.title.as_str()).unwrap_or("");
                let pct = if self.max_scroll() == 0 {
                    "ALL".to_string()
                } else {
                    format!("{}%", self.scroll * 100 / self.max_scroll())
                };
                let prefix = if self.loading { "⟳ " } else { "" };
                let text = format!(" [{mode}] {prefix}{title}  {}", self.status);
                let pad = " ".repeat(8);
                Line::styled(format!("{text}{pad}{pct} "), style)
            }
        }
    }

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(self.viewport_h)
    }

    // --- event handling -------------------------------------------------------

    fn handle_event(&mut self, event: Event) {
        if let Event::Key(key) = event {
            if key.kind == KeyEventKind::Release {
                return; // Windows emits Release events we must ignore.
            }
            self.handle_key(key);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match &mut self.input {
            InputMode::Normal => self.handle_normal(key),
            InputMode::Command(buf) => match key.code {
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Enter => {
                    let line = std::mem::take(buf);
                    self.input = InputMode::Normal;
                    self.run_command(&line);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            InputMode::Find(buf) => match key.code {
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Enter => {
                    let q = std::mem::take(buf);
                    self.input = InputMode::Normal;
                    self.find(&q);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            },
            InputMode::Follow(buf) => match key.code {
                KeyCode::Esc => self.input = InputMode::Normal,
                KeyCode::Enter => {
                    let n = std::mem::take(buf);
                    self.input = InputMode::Normal;
                    self.follow(&n);
                }
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) if c.is_ascii_digit() => buf.push(c),
                _ => {}
            },
        }
    }

    fn handle_normal(&mut self, key: KeyEvent) {
        // Movement keys that aren't user-remappable.
        match key.code {
            KeyCode::Down => return self.scroll_by(1),
            KeyCode::Up => return self.scroll_by(-1),
            KeyCode::PageDown | KeyCode::Char(' ') => {
                return self.scroll_by(self.viewport_h.max(1) as isize)
            }
            KeyCode::PageUp => return self.scroll_by(-(self.viewport_h.max(1) as isize)),
            KeyCode::Home => {
                self.scroll = 0;
                return;
            }
            KeyCode::End => {
                self.scroll = self.max_scroll();
                return;
            }
            _ => {}
        }

        let KeyCode::Char(c) = key.code else { return };
        let k = c.to_string();
        if k == self.keys.quit {
            self.quit = true;
        } else if k == self.keys.down {
            self.scroll_by(1);
        } else if k == self.keys.up {
            self.scroll_by(-1);
        } else if k == self.keys.top {
            self.scroll = 0;
        } else if k == self.keys.bottom {
            self.scroll = self.max_scroll();
        } else if k == self.keys.back {
            self.go_back();
        } else if k == self.keys.forward {
            self.go_forward();
        } else if k == self.keys.reload {
            self.reload();
        } else if k == self.keys.command {
            self.input = InputMode::Command(String::new());
        } else if k == self.keys.search_in_page {
            self.input = InputMode::Find(String::new());
        } else if k == self.keys.follow {
            self.input = InputMode::Follow(String::new());
        }
    }

    fn scroll_by(&mut self, delta: isize) {
        let max = self.max_scroll() as isize;
        let next = (self.scroll as isize + delta).clamp(0, max);
        self.scroll = next as usize;
    }

    // --- commands & navigation ------------------------------------------------

    fn run_command(&mut self, line: &str) {
        match parse_command(line) {
            Command::Quit => self.quit = true,
            Command::Reload => self.reload(),
            Command::Back => self.go_back(),
            Command::Forward => self.go_forward(),
            Command::Open { mode, target } => {
                let mode = mode.unwrap_or_else(|| self.dispatcher.resolve_mode(&target));
                self.navigate_new(mode, target);
            }
            Command::Unknown(msg) => self.status = msg,
        }
    }

    fn follow(&mut self, digits: &str) {
        let Ok(n) = digits.trim().parse::<usize>() else {
            self.status = "invalid link number".into();
            return;
        };
        match self.doc.as_ref().and_then(|d| d.link_url(n)).map(str::to_string) {
            Some(url) => {
                let mode = self.dispatcher.resolve_mode(&url);
                self.navigate_new(mode, url);
            }
            None => self.status = format!("no link [{n}]"),
        }
    }

    fn find(&mut self, query: &str) {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return;
        }
        self.find_matches = self
            .lines
            .iter()
            .enumerate()
            .filter(|(_, l)| line_text(l).to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();
        match self.find_matches.iter().find(|&&i| i > self.scroll).or(self.find_matches.first()) {
            Some(&i) => {
                self.scroll = i.min(self.max_scroll());
                self.status = format!("{} match(es) for \"{query}\"", self.find_matches.len());
            }
            None => self.status = format!("no match for \"{query}\""),
        }
    }

    /// Start a fresh navigation: truncate any forward history and push an entry.
    fn navigate_new(&mut self, mode: Mode, target: String) {
        if !self.history.is_empty() {
            self.history.truncate(self.hist_pos + 1);
        }
        self.history.push(HistEntry { mode, target: target.clone(), doc: None });
        self.hist_pos = self.history.len() - 1;
        self.start_fetch(mode, target);
    }

    fn go_back(&mut self) {
        if self.hist_pos == 0 || self.history.is_empty() {
            self.status = "at start of history".into();
            return;
        }
        self.hist_pos -= 1;
        self.load_current();
    }

    fn go_forward(&mut self) {
        if self.hist_pos + 1 >= self.history.len() {
            self.status = "at end of history".into();
            return;
        }
        self.hist_pos += 1;
        self.load_current();
    }

    fn reload(&mut self) {
        if let Some(entry) = self.history.get_mut(self.hist_pos) {
            entry.doc = None;
            let (mode, target) = (entry.mode, entry.target.clone());
            self.start_fetch(mode, target);
        }
    }

    /// Load the current history entry from cache, or fetch it if not cached.
    fn load_current(&mut self) {
        let Some(entry) = self.history.get(self.hist_pos) else { return };
        match entry.doc.clone() {
            Some(doc) => {
                self.status.clear();
                self.set_doc(doc);
            }
            None => {
                let (mode, target) = (entry.mode, entry.target.clone());
                self.start_fetch(mode, target);
            }
        }
    }

    fn start_fetch(&mut self, mode: Mode, target: String) {
        self.generation += 1;
        let generation = self.generation;
        self.loading = true;
        self.status = format!("loading {target} …");

        let dispatcher = self.dispatcher.clone();
        let tx = self.fetch_tx.clone();
        tokio::spawn(async move {
            let result = dispatcher.open(mode, &target).await;
            let _ = tx.send(Outcome { generation, result });
        });
    }

    fn handle_outcome(&mut self, outcome: Outcome) {
        if outcome.generation != self.generation {
            return; // a newer navigation superseded this one
        }
        self.loading = false;
        match outcome.result {
            Ok(doc) => {
                if let Some(entry) = self.history.get_mut(self.hist_pos) {
                    entry.doc = Some(doc.clone());
                }
                self.status.clear();
                self.set_doc(doc);
            }
            Err(e) => self.status = format!("error: {e:#}"),
        }
    }

    fn set_doc(&mut self, doc: Document) {
        self.doc = Some(doc);
        self.scroll = 0;
        self.dirty = true;
        self.find_matches.clear();
    }

    fn show_welcome(&mut self) {
        let mut b = DocumentBuilder::new("about:welcome");
        b.title("browser");
        b.push(Block::Heading { level: 1, spans: vec![Span::Text("A modal terminal browser".into())] });
        b.push(Block::Blank);
        b.push(Block::Paragraph {
            spans: vec![Span::Text(
                "Open the command bar with ':' then type a verb and a target.".into(),
            )],
        });
        for (cmd, desc) in [
            (":text <url>", "read a page in reader mode"),
            (":search <query> (or :s)", "search the web"),
            (":open <url> (:o)", "full webview — roadmap"),
            (":video <url> (:v)", "yt-dlp + mpv — roadmap"),
            ("<url or words>", "auto-routed by your config"),
        ] {
            b.push(Block::ListItem {
                ordered: false,
                marker: "•".into(),
                spans: vec![Span::Strong(cmd.into()), Span::Text(format!("— {desc}"))],
            });
        }
        b.push(Block::Blank);
        b.push(Block::Heading { level: 2, spans: vec![Span::Text("Keys".into())] });
        for (key, desc) in [
            ("j / k, Space, PgUp/PgDn", "scroll"),
            ("g / G", "top / bottom"),
            ("f", "follow a link by number"),
            ("/", "find in page"),
            ("H / L", "history back / forward"),
            ("r", "reload"),
            ("q", "quit"),
        ] {
            b.push(Block::ListItem {
                ordered: false,
                marker: "•".into(),
                spans: vec![Span::Strong(key.into()), Span::Text(format!("— {desc}"))],
            });
        }
        let _ = Modifier::BOLD; // styles applied in render
        self.set_doc(b.build());
    }
}
