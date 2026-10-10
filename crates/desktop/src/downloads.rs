//! Downloads, shell side: the list of this session's downloads, the "save it?" question
//! queue, and how they read in the status bar and on the `:downloads` page. The engine
//! (`engines::webview2::downloads`) reports [`DownloadEvent`]s and takes the answers.
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// What the engine reports about a download.
#[derive(Debug)]
pub(crate) enum DownloadEvent {
    /// A page started a download; it waits until the shell answers.
    Ask {
        id: u64,
        name: String,
        url: String,
        size: Option<u64>,
        /// An executable or installer: asked about with a louder warning.
        risky: bool,
    },
    Progress {
        id: u64,
        received: u64,
        total: Option<u64>,
    },
    Done {
        id: u64,
        path: PathBuf,
    },
    Failed {
        id: u64,
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum State {
    Asking,
    Running,
    Done,
    Declined,
    Failed(String),
}

#[derive(Clone, Debug)]
pub(crate) struct Download {
    pub(crate) id: u64,
    pub(crate) name: String,
    pub(crate) url: String,
    pub(crate) risky: bool,
    pub(crate) path: Option<PathBuf>,
    pub(crate) received: u64,
    pub(crate) total: Option<u64>,
    pub(crate) state: State,
}

/// This session's downloads, newest first, and the questions still to ask.
#[derive(Default)]
pub(crate) struct Downloads {
    pub(crate) list: Vec<Download>,
    asking: VecDeque<u64>,
}

impl Downloads {
    pub(crate) fn get(&self, id: u64) -> Option<&Download> {
        self.list.iter().find(|d| d.id == id)
    }

    fn get_mut(&mut self, id: u64) -> Option<&mut Download> {
        self.list.iter_mut().find(|d| d.id == id)
    }

    /// Record an engine event. Returns the download it concerns, after the update.
    pub(crate) fn apply(&mut self, event: DownloadEvent) -> Option<&Download> {
        match event {
            DownloadEvent::Ask {
                id,
                name,
                url,
                size,
                risky,
            } => {
                self.list.insert(
                    0,
                    Download {
                        id,
                        name,
                        url,
                        risky,
                        path: None,
                        received: 0,
                        total: size,
                        state: State::Asking,
                    },
                );
                self.asking.push_back(id);
                self.get(id)
            }
            DownloadEvent::Progress {
                id,
                received,
                total,
            } => {
                let d = self.get_mut(id)?;
                d.received = received;
                d.total = total.or(d.total);
                Some(&*d)
            }
            DownloadEvent::Done { id, path } => {
                let d = self.get_mut(id)?;
                d.state = State::Done;
                d.received = d.total.unwrap_or(d.received);
                d.path = Some(path);
                Some(&*d)
            }
            DownloadEvent::Failed { id, reason } => {
                self.asking.retain(|&a| a != id);
                let d = self.get_mut(id)?;
                d.state = State::Failed(reason);
                Some(&*d)
            }
        }
    }

    /// The download the prompt is asking about now.
    pub(crate) fn question(&self) -> Option<&Download> {
        self.asking.front().and_then(|&id| self.get(id))
    }

    /// Settle the current question: saving to `path`, or declined (`None`). Returns its id.
    pub(crate) fn settle(&mut self, path: Option<PathBuf>) -> Option<u64> {
        let id = self.asking.pop_front()?;
        if let Some(d) = self.get_mut(id) {
            d.state = if path.is_some() {
                State::Running
            } else {
                State::Declined
            };
            d.path = path;
        }
        Some(id)
    }

    pub(crate) fn running(&self) -> impl Iterator<Item = &Download> {
        self.list.iter().filter(|d| d.state == State::Running)
    }

    /// The status-bar note while downloads run: `↓ 45% report.pdf`, or a count.
    pub(crate) fn status(&self) -> Option<String> {
        let running: Vec<_> = self.running().collect();
        match running.as_slice() {
            [] => None,
            [d] => Some(format!("↓ {} {}", progress(d), short(&d.name, 32))),
            many => Some(format!("↓ {} downloads", many.len())),
        }
    }

    /// Forget a finished download (the file stays on disk).
    pub(crate) fn remove(&mut self, id: u64) {
        self.list.retain(|d| d.id != id);
    }
}

/// `45%`, or the bytes so far when the size is unknown.
fn progress(d: &Download) -> String {
    match d.total {
        Some(total) if total > 0 => format!("{}%", d.received * 100 / total),
        _ => crate::procmon::fmt_bytes(d.received),
    }
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// Where a download named `name` is saved in `dir`: the name itself, or `name (1).ext`,
/// `name (2).ext`, … when that's taken. Path separators in `name` are dropped so a
/// server can't steer the file out of `dir`.
pub(crate) fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let clean: String = name
        .chars()
        .map(|c| match c {
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let clean = clean.trim().trim_matches('.');
    let clean = if clean.is_empty() { "download" } else { clean };
    let first = dir.join(clean);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match clean.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (clean, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("an unused file name")
}

/// How many lines the `:downloads` page puts above the first download.
pub(crate) const PAGE_HEADER: usize = 3;

/// The `:downloads` page: a header, then one row per download (newest first).
pub(crate) fn page_lines(list: &[Download], dir: &Path) -> Vec<String> {
    let mut lines = vec![
        format!(
            "downloads — {}   (Enter open · e show in folder · d cancel / forget)",
            list.len()
        ),
        format!(
            "saved to {}   (:downloads dir <path> to change)",
            dir.display()
        ),
        String::new(),
    ];
    debug_assert_eq!(lines.len(), PAGE_HEADER);
    if list.is_empty() {
        lines.push("nothing downloaded yet this session".into());
    }
    for d in list {
        let (state, size) = match &d.state {
            State::Asking => ("asking".to_string(), size_of(d)),
            State::Running => (
                format!("↓ {}", progress(d)),
                format!(
                    "{}/{}",
                    crate::procmon::fmt_bytes(d.received),
                    d.total.map(crate::procmon::fmt_bytes).unwrap_or("?".into())
                ),
            ),
            State::Done => ("done".into(), size_of(d)),
            State::Declined => ("declined".into(), size_of(d)),
            State::Failed(reason) => (format!("failed: {reason}"), size_of(d)),
        };
        lines.push(format!("{state:<24} {size:>18}   {}", d.name));
    }
    lines
}

fn size_of(d: &Download) -> String {
    d.total
        .or((d.received > 0).then_some(d.received))
        .map(crate::procmon::fmt_bytes)
        .unwrap_or_else(|| "—".into())
}

/// The download on `:downloads` page row `row`, if that row is one.
pub(crate) fn at_row(list: &[Download], row: usize) -> Option<&Download> {
    row.checked_sub(PAGE_HEADER).and_then(|i| list.get(i))
}

impl crate::App {
    /// Where downloads are saved: `:downloads dir`, else the Windows Downloads folder.
    pub(crate) fn download_dir(&self) -> PathBuf {
        if let Some(dir) = &self.config.download_dir {
            return dir.clone();
        }
        directories::UserDirs::new()
            .and_then(|u| u.download_dir().map(Path::to_path_buf))
            .or_else(|| directories::BaseDirs::new().map(|b| b.home_dir().join("Downloads")))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// An engine reported on a download.
    pub(crate) fn on_download(&mut self, event: DownloadEvent) {
        let ended = matches!(
            event,
            DownloadEvent::Done { .. } | DownloadEvent::Failed { .. }
        );
        let Some(d) = self.downloads.apply(event).cloned() else {
            return;
        };
        match &d.state {
            State::Asking => self.ask_download(),
            State::Done if ended => {
                self.set_status(format!("downloaded {} — :downloads", short(&d.name, 60)))
            }
            State::Failed(reason) if reason == "cancelled" => {
                self.set_status(format!("cancelled {}", short(&d.name, 60)))
            }
            State::Failed(reason) => self.set_error(format!(
                "download of {} failed: {reason}",
                short(&d.name, 60)
            )),
            _ => {}
        }
        if self.mode == crate::ModeKind::DownloadAsk && self.downloads.question().is_none() {
            // The question it was showing went away (the download failed meanwhile).
            self.mode = crate::ModeKind::Normal;
        }
        self.refresh_downloads_page();
        self.window.request_redraw();
    }

    /// Show the pending "save it?" question, unless you're busy: a question that arrives
    /// while you type a command (or resize, pick a hint, …) waits until you're back in
    /// Normal mode. Typing into a page doesn't count as busy — the download came from it.
    pub(crate) fn ask_download(&mut self) {
        use crate::ModeKind;
        if self.downloads.question().is_none() {
            return;
        }
        match self.mode {
            ModeKind::DownloadAsk => return,
            ModeKind::Normal => {}
            ModeKind::Insert => self.exit_to_normal(),
            ModeKind::Passthrough if !self.active_is_term() && !self.active_is_ai() => {
                self.exit_to_normal()
            }
            _ => return,
        }
        self.mode = ModeKind::DownloadAsk;
        self.reclaim_shell_focus();
        self.window.request_redraw();
    }

    /// Keys while a download question is showing: y/Enter saves it, n/Esc declines.
    pub(crate) fn key_download_ask(&mut self, key: &tao::event::KeyEvent) {
        use tao::keyboard::Key;
        let yes = match &key.logical_key {
            Key::Enter => true,
            Key::Escape => false,
            Key::Character(c) if c.eq_ignore_ascii_case("y") => true,
            Key::Character(c) if c.eq_ignore_ascii_case("n") => false,
            _ => return,
        };
        let Some(d) = self.downloads.question().cloned() else {
            self.mode = crate::ModeKind::Normal;
            return;
        };
        let dir = self.download_dir();
        let path = if yes {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => Some(unique_path(&dir, &d.name)),
                Err(e) => {
                    self.set_error(format!("can't save to {}: {e}", dir.display()));
                    None
                }
            }
        } else {
            None
        };
        self.downloads.settle(path.clone());
        crate::engines::answer_download(d.id, path.as_deref());
        match &path {
            Some(path) => self.set_status(format!(
                "downloading {} → {}",
                short(&d.name, 50),
                path.parent().unwrap_or(&dir).display()
            )),
            None => self.set_status(format!("didn't download {}", short(&d.name, 60))),
        }
        if self.downloads.question().is_none() {
            self.mode = crate::ModeKind::Normal;
        }
        self.refresh_downloads_page();
        self.window.request_redraw();
    }

    /// `:downloads` — this session's downloads in a vim page (Enter opens the file, `e`
    /// shows it in its folder, `d` cancels a running one or forgets a finished one).
    pub(crate) fn open_downloads_page(&mut self) {
        if self.active_url() == Some("browser://downloads") {
            return self.refresh_downloads_page();
        }
        let lines = page_lines(&self.downloads.list, &self.download_dir());
        let mut tab = crate::Tab::blank();
        tab.url = "browser://downloads".into();
        tab.content = crate::tabs::TabContent::Pager(crate::vim::TextBuffer::new(lines));
        self.place_tab(tab, true);
        self.window.set_focus();
        self.clear_status();
    }

    /// Re-render the `:downloads` page in place, if it's the tab on screen.
    pub(crate) fn refresh_downloads_page(&mut self) {
        if self.active_url() != Some("browser://downloads") {
            return;
        }
        let lines = page_lines(&self.downloads.list, &self.download_dir());
        if let Some(buf) = self
            .active
            .and_then(|i| self.tabs.get_mut(i))
            .and_then(|t| t.vim_mut())
        {
            buf.set_lines(lines);
        }
        self.window.request_redraw();
    }

    /// The download on the `:downloads` cursor row.
    fn download_under_cursor(&self) -> Option<Download> {
        if self.active_url() != Some("browser://downloads") {
            return None;
        }
        let row = self
            .active
            .and_then(|i| self.tabs.get(i))
            .and_then(|t| t.vim())?
            .cy;
        at_row(&self.downloads.list, row).cloned()
    }

    /// A key on the `:downloads` page: Enter opens, `e` shows in folder, `d` cancels or
    /// forgets. Returns whether it was one of those.
    pub(crate) fn key_downloads_page(&mut self, key: &tao::event::KeyEvent) -> bool {
        use tao::keyboard::Key;
        if self.active_url() != Some("browser://downloads") {
            return false;
        }
        let action = match &key.logical_key {
            Key::Enter => 'o',
            Key::Character(c) if *c == "e" => 'e',
            Key::Character(c) if *c == "d" => 'd',
            _ => return false,
        };
        let Some(d) = self.download_under_cursor() else {
            return true;
        };
        match (action, &d.state, &d.path) {
            ('o', State::Done, Some(path)) => open_with_shell(path, false),
            ('e', State::Done | State::Running, Some(path)) => open_with_shell(path, true),
            ('o' | 'e', _, _) => self.set_status(format!("{} isn't downloaded", d.name)),
            ('d', State::Asking | State::Running, _) => crate::engines::cancel_download(d.id),
            _ => {
                self.downloads.remove(d.id);
                self.refresh_downloads_page();
            }
        }
        true
    }

    /// `:downloads dir [path]` — show or change where downloads are saved.
    pub(crate) fn set_download_dir(&mut self, arg: &str) {
        let arg = arg.trim();
        if arg.is_empty() {
            return self.set_status(format!("downloads go to {}", self.download_dir().display()));
        }
        let dir = PathBuf::from(arg);
        if !dir.is_dir() {
            return self.set_error(format!("{} isn't a folder", dir.display()));
        }
        self.config.download_dir = Some(dir.clone());
        crate::config::save(&self.config);
        self.set_status(format!("downloads now go to {}", dir.display()));
        self.refresh_downloads_page();
    }
}

/// Open a downloaded file with its program, or (`reveal`) show it selected in Explorer.
fn open_with_shell(path: &Path, reveal: bool) {
    let mut cmd = std::process::Command::new("explorer.exe");
    if reveal {
        let mut arg = std::ffi::OsString::from("/select,");
        arg.push(path.as_os_str());
        cmd.arg(arg);
    } else {
        cmd.arg(path);
    }
    let _ = cmd.spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(d: &mut Downloads, id: u64, name: &str) {
        d.apply(DownloadEvent::Ask {
            id,
            name: name.into(),
            url: format!("https://x.test/{name}"),
            size: Some(200),
            risky: false,
        });
    }

    #[test]
    fn questions_are_asked_in_order_and_settled() {
        let mut d = Downloads::default();
        ask(&mut d, 1, "a.pdf");
        ask(&mut d, 2, "b.zip");
        assert_eq!(d.question().unwrap().name, "a.pdf");
        assert_eq!(d.settle(Some(PathBuf::from("C:/dl/a.pdf"))), Some(1));
        assert_eq!(d.get(1).unwrap().state, State::Running);
        assert_eq!(d.question().unwrap().name, "b.zip");
        assert_eq!(d.settle(None), Some(2));
        assert_eq!(d.get(2).unwrap().state, State::Declined);
        assert!(d.question().is_none());
    }

    #[test]
    fn progress_and_completion_show_in_the_status() {
        let mut d = Downloads::default();
        ask(&mut d, 1, "a.pdf");
        d.settle(Some(PathBuf::from("C:/dl/a.pdf")));
        d.apply(DownloadEvent::Progress {
            id: 1,
            received: 90,
            total: Some(200),
        });
        assert_eq!(d.status().as_deref(), Some("↓ 45% a.pdf"));
        d.apply(DownloadEvent::Done {
            id: 1,
            path: PathBuf::from("C:/dl/a.pdf"),
        });
        assert_eq!(d.status(), None);
        assert_eq!(d.get(1).unwrap().state, State::Done);
    }

    #[test]
    fn a_failure_while_asking_drops_the_question() {
        let mut d = Downloads::default();
        ask(&mut d, 1, "a.pdf");
        d.apply(DownloadEvent::Failed {
            id: 1,
            reason: "the connection dropped".into(),
        });
        assert!(d.question().is_none());
    }

    #[test]
    fn page_rows_map_back_to_downloads() {
        let mut d = Downloads::default();
        ask(&mut d, 1, "old.pdf");
        ask(&mut d, 2, "new.zip");
        let lines = page_lines(&d.list, Path::new("C:/dl"));
        assert!(lines[PAGE_HEADER].ends_with("new.zip"));
        assert_eq!(at_row(&d.list, PAGE_HEADER).unwrap().id, 2);
        assert_eq!(at_row(&d.list, PAGE_HEADER + 1).unwrap().id, 1);
        assert!(at_row(&d.list, 0).is_none());
    }

    #[test]
    fn unique_paths_never_overwrite_or_escape_the_folder() {
        let dir = std::env::temp_dir().join(format!("browser-dl-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(unique_path(&dir, "a.pdf"), dir.join("a.pdf"));
        std::fs::write(dir.join("a.pdf"), "x").unwrap();
        std::fs::write(dir.join("a (1).pdf"), "x").unwrap();
        assert_eq!(unique_path(&dir, "a.pdf"), dir.join("a (2).pdf"));
        assert_eq!(
            unique_path(&dir, "..\\..\\evil.exe"),
            dir.join("_.._evil.exe")
        );
        assert_eq!(unique_path(&dir, ""), dir.join("download"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
