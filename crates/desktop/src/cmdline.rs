//! The command bar's editable line (`:` commands and `/` searches): its text, the
//! caret and the selection. Offsets are bytes, always on char boundaries.

#[derive(Default)]
pub(crate) struct LineEdit {
    text: String,
    cursor: usize,
    /// Selection anchor. When it differs from the caret, the text between them is
    /// selected (Shift-movement extends it; typing replaces it).
    anchor: Option<usize>,
}

impl LineEdit {
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    /// Whether the caret is at the end with nothing selected — where autocomplete
    /// can offer a suggestion.
    pub(crate) fn at_end(&self) -> bool {
        self.cursor == self.text.len() && self.anchor.is_none()
    }

    /// The selection as an ordered byte range, or `None` if empty.
    pub(crate) fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        let c = self.cursor;
        (a != c).then(|| (a.min(c), a.max(c)))
    }

    pub(crate) fn selected(&self) -> Option<&str> {
        self.selection().map(|(a, b)| &self.text[a..b])
    }

    /// Replace the whole line, caret at the end, nothing selected.
    pub(crate) fn set(&mut self, text: impl Into<String>) {
        self.text = text.into();
        self.cursor = self.text.len();
        self.anchor = None;
    }

    /// Empty the line, returning what it held.
    pub(crate) fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.anchor = None;
        text
    }

    pub(crate) fn clear(&mut self) {
        self.take();
    }

    pub(crate) fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }

    /// Move the caret to `pos`. `extend` keeps or starts a selection (Shift held);
    /// otherwise the selection is dropped. A zero-width selection is normalized away.
    pub(crate) fn move_to(&mut self, pos: usize, extend: bool) {
        if extend {
            self.anchor.get_or_insert(self.cursor);
        } else {
            self.anchor = None;
        }
        self.cursor = pos;
        if self.anchor == Some(pos) {
            self.anchor = None;
        }
    }

    /// Left arrow: a plain press with a selection collapses to its start; otherwise
    /// the caret moves one char.
    pub(crate) fn left(&mut self, extend: bool) {
        match self.selection() {
            Some((a, _)) if !extend => self.move_to(a, false),
            _ => self.move_to(self.prev_char(), extend),
        }
    }

    /// Right arrow: the mirror of [`left`](Self::left).
    pub(crate) fn right(&mut self, extend: bool) {
        match self.selection() {
            Some((_, b)) if !extend => self.move_to(b, false),
            _ => self.move_to(self.next_char(), extend),
        }
    }

    pub(crate) fn word_left(&mut self, extend: bool) {
        self.move_to(prev_word_boundary(&self.text, self.cursor), extend);
    }

    pub(crate) fn word_right(&mut self, extend: bool) {
        self.move_to(next_word_boundary(&self.text, self.cursor), extend);
    }

    pub(crate) fn home(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    pub(crate) fn end(&mut self, extend: bool) {
        self.move_to(self.text.len(), extend);
    }

    /// Replace the selection (if any) with `text`, then place the caret after it.
    /// Control characters (e.g. newlines from a paste) are dropped — it's one line.
    pub(crate) fn insert(&mut self, text: &str) {
        self.delete_selection();
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }

    /// Remove the selection if there is one; returns whether anything was deleted.
    pub(crate) fn delete_selection(&mut self) -> bool {
        let sel = self.selection();
        self.anchor = None;
        match sel {
            Some((a, b)) => {
                self.text.replace_range(a..b, "");
                self.cursor = a;
                true
            }
            None => false,
        }
    }

    /// Delete the char before the caret (or the selection).
    pub(crate) fn backspace(&mut self) {
        if !self.delete_selection() {
            self.delete_back_to(self.prev_char());
        }
    }

    /// Delete the char after the caret (or the selection).
    pub(crate) fn delete_forward(&mut self) {
        if !self.delete_selection() {
            self.delete_forward_to(self.next_char());
        }
    }

    /// Delete the word before the caret (or the selection).
    pub(crate) fn delete_word_back(&mut self) {
        if !self.delete_selection() {
            self.delete_back_to(prev_word_boundary(&self.text, self.cursor));
        }
    }

    /// Delete the word after the caret (or the selection).
    pub(crate) fn delete_word_forward(&mut self) {
        if !self.delete_selection() {
            self.delete_forward_to(next_word_boundary(&self.text, self.cursor));
        }
    }

    /// Delete from the caret back to the start of the line (Ctrl+U).
    pub(crate) fn delete_to_start(&mut self) {
        self.anchor = None;
        self.delete_back_to(0);
    }

    fn delete_back_to(&mut self, start: usize) {
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    fn delete_forward_to(&mut self, end: usize) {
        self.text.replace_range(self.cursor..end, "");
    }

    /// Byte offset of the char before the caret (or the caret, at the start).
    fn prev_char(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .next_back()
            .map_or(self.cursor, |(i, _)| i)
    }

    /// Byte offset just after the char at the caret (or the caret, at the end).
    fn next_char(&self) -> usize {
        self.text[self.cursor..]
            .chars()
            .next()
            .map_or(self.cursor, |c| self.cursor + c.len_utf8())
    }
}

/// A "word" character for command-bar word motions (`Ctrl+W`, `Ctrl+←/→`):
/// alphanumerics and `_`. Everything else — `/ . : - ? & = # @ ~ + …` — is a
/// separator, so word jumps/deletes stop at URL and path boundaries.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte offset of the start of the word before `pos`: skip trailing separators,
/// then the word run. So `Ctrl+W` on `…/foo/bar` erases `bar` (then `/`, then
/// `foo`), not the entire URL.
fn prev_word_boundary(s: &str, pos: usize) -> usize {
    let trimmed = s[..pos].trim_end_matches(|c| !is_word_char(c));
    trimmed
        .char_indices()
        .rev()
        .find(|(_, c)| !is_word_char(*c))
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0)
}

/// Byte offset of the end of the word after `pos`: skip leading separators, then
/// the word run.
fn next_word_boundary(s: &str, pos: usize) -> usize {
    let rest = &s[pos..];
    let after_sep = rest.trim_start_matches(|c| !is_word_char(c));
    let sep = rest.len() - after_sep.len();
    let word = after_sep
        .find(|c| !is_word_char(c))
        .unwrap_or(after_sep.len());
    pos + sep + word
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(text: &str) -> LineEdit {
        let mut l = LineEdit::default();
        l.set(text);
        l
    }

    #[test]
    fn typing_replaces_the_selection_and_drops_control_chars() {
        let mut l = line("open example.com");
        l.word_left(true);
        assert_eq!(l.selected(), Some("com"));
        l.insert("org\n");
        assert_eq!(l.text(), "open example.org");
        assert!(l.at_end());
    }

    #[test]
    fn arrows_collapse_a_selection_to_its_edge() {
        let mut l = line("abcd");
        l.move_to(1, false);
        l.move_to(3, true);
        l.left(false);
        assert_eq!((l.cursor(), l.selection()), (1, None));
        l.move_to(3, true);
        l.right(false);
        assert_eq!((l.cursor(), l.selection()), (3, None));
        l.right(true);
        assert_eq!(l.selected(), Some("d"));
    }

    #[test]
    fn a_zero_width_selection_is_no_selection() {
        let mut l = line("ab");
        l.left(true);
        l.right(true);
        assert!(l.at_end());
        assert!(!l.delete_selection());
        assert_eq!(l.text(), "ab");
    }

    #[test]
    fn deletes_step_over_multibyte_chars() {
        let mut l = line("aé€b");
        l.backspace();
        l.backspace();
        assert_eq!(l.text(), "aé");
        l.home(false);
        l.delete_forward();
        assert_eq!((l.text(), l.cursor()), ("é", 0));
        l.end(false);
        l.backspace();
        l.backspace();
        assert_eq!(l.text(), "");
    }

    #[test]
    fn word_and_line_deletes() {
        let mut l = line("open a.test/foo/bar");
        l.delete_word_back();
        assert_eq!(l.text(), "open a.test/foo/");
        l.home(false);
        l.delete_word_forward();
        assert_eq!(l.text(), " a.test/foo/");
        l.end(false);
        l.word_left(false);
        l.delete_to_start();
        assert_eq!((l.text(), l.cursor()), ("foo/", 0));
    }

    #[test]
    fn select_all_then_take_empties_the_line() {
        let mut l = line("abc");
        l.select_all();
        assert_eq!(l.selected(), Some("abc"));
        assert_eq!(l.take(), "abc");
        assert_eq!((l.text(), l.cursor(), l.selection()), ("", 0, None));
    }

    #[test]
    fn ctrl_w_deletes_one_url_segment_at_a_time() {
        let url = "https://example.com/foo/bar";
        // Caret at the end: prev word is `bar`, leaving the trailing slash.
        let p1 = prev_word_boundary(url, url.len());
        assert_eq!(&url[..p1], "https://example.com/foo/");
        // Again from there: skip the `/`, delete `foo`.
        let p2 = prev_word_boundary(url, p1);
        assert_eq!(&url[..p2], "https://example.com/");
        // Not the whole thing in one go.
        assert_ne!(p1, 0);
    }

    #[test]
    fn word_motions_step_over_separators() {
        let s = "ab.cd";
        assert_eq!(next_word_boundary(s, 0), 2); // end of `ab`
        assert_eq!(next_word_boundary(s, 2), 5); // skip `.`, end of `cd`
        assert_eq!(prev_word_boundary(s, 5), 3); // start of `cd`
    }
}
