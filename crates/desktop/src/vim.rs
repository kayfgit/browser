//! A tiny read-only vim-style pager used by the `:error` / `:errors` tabs.
//!
//! The error log is shown as plain monospace text you can navigate, visually
//! select, and yank (copy) with familiar motions — but never edit. It is not a
//! full vim; it implements the motions that make "grab that token out of an error
//! message" pleasant: `hjkl`/arrows, `w`/`b`/`e`, `0`/`^`/`$`, `gg`/`G`, half-page
//! `d`/`u`, charwise/linewise visual (`v`/`V`) with `y` to copy, and the operator
//! `y` with motions (`yy`, `yw`, `y$`, …) and text objects (`yiw`, `yi(`, `ya"`, …).

/// A key handed to [`TextBuffer::key`]. The shell translates a raw key event into
/// one of these; anything that doesn't map is never offered to the buffer.
#[derive(Clone, Copy, PartialEq)]
pub enum Key {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    Esc,
    /// Ctrl+D / Ctrl+U — half-page down / up.
    HalfDown,
    HalfUp,
}

/// Result of feeding one key to the buffer.
pub struct KeyResult {
    /// Whether the buffer handled the key (so the shell shouldn't also act on it).
    pub consumed: bool,
    /// Text to place on the clipboard, if a yank happened.
    pub yanked: Option<String>,
}

/// A read-only text buffer with a cursor, optional visual selection, and vim-ish
/// motions. Rendered as a monospace grid by the shell painter.
pub struct TextBuffer {
    /// Lines as char vectors (columns map 1:1 to monospace cells).
    pub lines: Vec<Vec<char>>,
    /// Cursor row / column (column is a char index into `lines[cy]`).
    pub cy: usize,
    pub cx: usize,
    /// Visual-mode anchor (row, col); `None` when not selecting.
    pub anchor: Option<(usize, usize)>,
    /// Linewise visual (`V`) vs charwise (`v`).
    pub linewise: bool,
    /// First visible row (vertical scroll).
    pub top: usize,
    /// First visible column (horizontal scroll).
    pub left: usize,
    /// Pending operator/prefix keys (`"g"`, `"y"`, `"yi"`, `"ya"`, `"f"`, `"yt"`, …).
    pending: String,
    /// Last `f`/`F`/`t`/`T` find (kind, target char), for `;` / `,` to repeat.
    last_find: Option<(char, char)>,
    /// Scratch: did the last [`TextBuffer::dispatch`] handle the key?
    last_consumed: bool,
}

impl TextBuffer {
    pub fn new(lines: Vec<String>) -> Self {
        let lines: Vec<Vec<char>> = if lines.is_empty() {
            vec![Vec::new()]
        } else {
            lines.into_iter().map(|l| l.chars().collect()).collect()
        };
        TextBuffer {
            lines,
            cy: 0,
            cx: 0,
            anchor: None,
            linewise: false,
            top: 0,
            left: 0,
            pending: String::new(),
            last_find: None,
            last_consumed: false,
        }
    }

    /// Place the cursor at `(row, col)` and scroll it into a `rows`×`cols` viewport.
    /// Used by find-in-page to jump to a match.
    pub fn place_cursor(&mut self, row: usize, col: usize, rows: usize, cols: usize) {
        self.cy = row.min(self.lines.len().saturating_sub(1));
        self.cx = col.min(self.line_len(self.cy));
        self.ensure_visible(rows.max(1), cols.max(1));
    }

    /// Status-bar mode tag, if a visual selection is active.
    pub fn mode_label(&self) -> Option<&'static str> {
        self.anchor.map(|_| if self.linewise { "VISUAL LINE" } else { "VISUAL" })
    }

    fn line_len(&self, row: usize) -> usize {
        self.lines.get(row).map(|l| l.len()).unwrap_or(0)
    }

    /// Clamp the cursor column to the current line. `allow_end` permits one column
    /// past the last char (used while building selections / after `$`).
    fn clamp_cx(&mut self, allow_end: bool) {
        let len = self.line_len(self.cy);
        let max = if allow_end { len } else { len.saturating_sub(1) };
        if self.cx > max {
            self.cx = max;
        }
    }

    /// Feed one key. `rows`/`cols` are the current viewport size in cells, used for
    /// half-page motions and to keep the cursor on screen.
    pub fn key(&mut self, k: Key, rows: usize, cols: usize) -> KeyResult {
        let yanked = self.dispatch(k, rows);
        let consumed = self.last_consumed;
        if consumed {
            self.ensure_visible(rows.max(1), cols.max(1));
        }
        KeyResult { consumed, yanked }
    }

    /// Run a key through the state machine, recording whether it was consumed in
    /// `last_consumed` and returning any yanked text.
    fn dispatch(&mut self, k: Key, rows: usize) -> Option<String> {
        self.last_consumed = true;
        // A pending operator/prefix swallows the next key.
        if !self.pending.is_empty() {
            return self.pending_key(k);
        }
        match k {
            Key::Left | Key::Char('h') => self.move_h(-1),
            Key::Right | Key::Char('l') => self.move_h(1),
            Key::Down | Key::Char('j') => self.move_v(1),
            Key::Up | Key::Char('k') => self.move_v(-1),
            Key::Home | Key::Char('0') => self.cx = 0,
            Key::End | Key::Char('$') => self.cx = self.line_len(self.cy).saturating_sub(1),
            Key::Char('^') => self.cx = self.first_nonblank(self.cy),
            Key::Char('w') => self.word_forward(),
            Key::Char('b') => self.word_back(),
            Key::Char('e') => self.word_end(),
            Key::Char('G') => {
                self.cy = self.lines.len().saturating_sub(1);
                self.clamp_cx(false);
            }
            Key::Char('g') => self.pending = "g".into(),
            // Find-char motions: wait for the target char (handled in `pending_key`).
            Key::Char(c @ ('f' | 'F' | 't' | 'T')) => self.pending = c.to_string(),
            // Repeat the last find: `;` same direction, `,` reversed.
            Key::Char(';') => {
                if let Some((k, ch)) = self.last_find {
                    self.find_move(k, ch);
                }
            }
            Key::Char(',') => {
                if let Some((k, ch)) = self.last_find {
                    self.find_move(invert_find(k), ch);
                }
            }
            Key::HalfDown | Key::Char('d') => self.move_v((rows / 2).max(1) as isize),
            Key::HalfUp | Key::Char('u') => self.move_v(-((rows / 2).max(1) as isize)),
            Key::Char('v') => self.toggle_visual(false),
            Key::Char('V') => self.toggle_visual(true),
            Key::Char('y') => {
                if self.anchor.is_some() {
                    let text = self.selection_text();
                    self.anchor = None;
                    return Some(text);
                }
                self.pending = "y".into();
            }
            Key::Esc => {
                if self.anchor.is_some() {
                    self.anchor = None;
                } else {
                    self.last_consumed = false;
                }
            }
            // Swallow `i`/`a` so they don't fall through to the shell's insert mode;
            // on their own (no operator pending) they do nothing in a read-only view.
            Key::Char('i') | Key::Char('a') => {}
            // Swallow `n`/`p` so they don't switch tabs from the pager (use 1–9).
            Key::Char('n') | Key::Char('p') => {}
            _ => self.last_consumed = false,
        }
        None
    }

    /// Handle a key while an operator/prefix is pending.
    fn pending_key(&mut self, k: Key) -> Option<String> {
        // Escape always cancels a pending operator.
        if k == Key::Esc {
            self.pending.clear();
            return None;
        }
        let Key::Char(c) = k else {
            // Non-char keys cancel the pending operator (but are still consumed).
            self.pending.clear();
            return None;
        };
        let pending = std::mem::take(&mut self.pending);
        match pending.as_str() {
            "g" => {
                if c == 'g' {
                    self.cy = 0;
                    self.clamp_cx(false);
                }
                None
            }
            // `f`/`F`/`t`/`T` + target char: move the cursor along the line.
            "f" | "F" | "t" | "T" => {
                let k = pending.chars().next().unwrap();
                self.last_find = Some((k, c));
                self.find_move(k, c);
                None
            }
            "y" => self.yank_operator(c),
            // `y` + find + target char: yank up to (and, for `f`, including) the char.
            "yf" | "yF" | "yt" | "yT" => {
                let k = pending.chars().nth(1).unwrap();
                self.last_find = Some((k, c));
                self.yank_find(k, c)
            }
            "yi" => self.yank_textobject(c, false),
            "ya" => self.yank_textobject(c, true),
            _ => None,
        }
    }

    /// `y` followed by a motion / line / text-object lead-in.
    fn yank_operator(&mut self, c: char) -> Option<String> {
        match c {
            'y' => Some(self.line_text(self.cy)), // yy: whole line
            'i' => {
                self.pending = "yi".into();
                None
            }
            'a' => {
                self.pending = "ya".into();
                None
            }
            'f' | 'F' | 't' | 'T' => {
                self.pending = format!("y{c}");
                None
            }
            '$' => self.yank_cols(self.cy, self.cx, self.line_len(self.cy)),
            '0' => self.yank_cols(self.cy, 0, self.cx),
            '^' => {
                let s = self.first_nonblank(self.cy);
                let (a, b) = (s.min(self.cx), s.max(self.cx));
                self.yank_cols(self.cy, a, b)
            }
            'l' => self.yank_cols(self.cy, self.cx, (self.cx + 1).min(self.line_len(self.cy))),
            'h' => self.yank_cols(self.cy, self.cx.saturating_sub(1), self.cx),
            'w' | 'e' => {
                let end = self.word_target_forward(c == 'e');
                self.yank_cols(self.cy, self.cx, end)
            }
            'b' => {
                let start = self.word_target_back();
                self.yank_cols(self.cy, start, self.cx)
            }
            'j' => {
                let end = (self.cy + 1).min(self.lines.len().saturating_sub(1));
                Some(self.lines_text(self.cy, end))
            }
            'k' => {
                let start = self.cy.saturating_sub(1);
                Some(self.lines_text(start, self.cy))
            }
            _ => None,
        }
    }

    /// `yi<obj>` / `ya<obj>`: yank a text object on the current line.
    fn yank_textobject(&mut self, c: char, around: bool) -> Option<String> {
        let line = self.lines.get(self.cy)?;
        let range = match c {
            'w' => word_object(line, self.cx, around),
            '(' | ')' | 'b' => pair_object(line, self.cx, '(', ')', around),
            '{' | '}' | 'B' => pair_object(line, self.cx, '{', '}', around),
            '[' | ']' => pair_object(line, self.cx, '[', ']', around),
            '<' | '>' => pair_object(line, self.cx, '<', '>', around),
            '"' => quote_object(line, self.cx, '"', around),
            '\'' => quote_object(line, self.cx, '\'', around),
            '`' => quote_object(line, self.cx, '`', around),
            _ => None,
        }?;
        Some(line[range.0..range.1].iter().collect())
    }

    // --- motions --------------------------------------------------------------

    fn move_h(&mut self, d: isize) {
        let len = self.line_len(self.cy);
        let max = len.saturating_sub(1);
        let nx = self.cx as isize + d;
        self.cx = nx.clamp(0, max as isize) as usize;
    }

    fn move_v(&mut self, d: isize) {
        let last = self.lines.len().saturating_sub(1);
        let ny = (self.cy as isize + d).clamp(0, last as isize) as usize;
        self.cy = ny;
        self.clamp_cx(false);
    }

    fn first_nonblank(&self, row: usize) -> usize {
        let line = &self.lines[row];
        line.iter().position(|c| !c.is_whitespace()).unwrap_or(0)
    }

    fn word_forward(&mut self) {
        self.cx = self.word_target_forward(false);
        // `w` can land on the next line's start when at end of line.
        if self.cx >= self.line_len(self.cy) && self.cy + 1 < self.lines.len() {
            self.cy += 1;
            self.cx = self.first_nonblank(self.cy);
        }
    }

    /// Column of the next word start (`e=false`) or word end (`e=true`) on this line.
    fn word_target_forward(&self, end: bool) -> usize {
        let line = &self.lines[self.cy];
        let n = line.len();
        let mut i = self.cx;
        if n == 0 {
            return 0;
        }
        if end {
            i = (i + 1).min(n);
            while i < n && line[i].is_whitespace() {
                i += 1;
            }
            while i + 1 < n && is_word(line[i + 1]) == is_word(line[i]) && !line[i].is_whitespace() {
                i += 1;
            }
            i.min(n.saturating_sub(1))
        } else {
            let start_class = is_word(line[i.min(n - 1)]);
            while i < n && !line[i].is_whitespace() && is_word(line[i]) == start_class {
                i += 1;
            }
            while i < n && line[i].is_whitespace() {
                i += 1;
            }
            i
        }
    }

    fn word_back(&mut self) {
        self.cx = self.word_target_back();
    }

    fn word_target_back(&self) -> usize {
        let line = &self.lines[self.cy];
        if self.cx == 0 {
            return 0;
        }
        let mut i = self.cx - 1;
        while i > 0 && line[i].is_whitespace() {
            i -= 1;
        }
        let class = is_word(line[i]);
        while i > 0 && !line[i - 1].is_whitespace() && is_word(line[i - 1]) == class {
            i -= 1;
        }
        i
    }

    fn word_end(&mut self) {
        self.cx = self.word_target_forward(true);
    }

    // --- find char (`f`/`t`/`F`/`T`) -----------------------------------------

    /// Index of the target char for an `f`/`t`/`F`/`T` search from the cursor (the
    /// actual occurrence; the landing column for `t`/`T` is one cell short of it).
    fn find_index(&self, kind: char, ch: char) -> Option<usize> {
        let line = &self.lines[self.cy];
        match kind {
            'f' | 't' => (self.cx + 1..line.len()).find(|&i| line[i] == ch),
            'F' | 'T' => (0..self.cx).rev().find(|&i| line[i] == ch),
            _ => None,
        }
    }

    /// Move the cursor to an `f`/`t`/`F`/`T` target on the current line.
    fn find_move(&mut self, kind: char, ch: char) {
        if let Some(i) = self.find_index(kind, ch) {
            self.cx = match kind {
                't' => i.saturating_sub(1),
                'T' => i + 1,
                _ => i,
            };
        }
    }

    /// `y` + find: yank from the cursor to the target. `f` includes the char, `t`
    /// stops before it; the backward forms span target → cursor.
    fn yank_find(&mut self, kind: char, ch: char) -> Option<String> {
        let i = self.find_index(kind, ch)?;
        match kind {
            'f' => self.yank_cols(self.cy, self.cx, i + 1),
            't' => self.yank_cols(self.cy, self.cx, i),
            'F' => self.yank_cols(self.cy, i, self.cx),
            'T' => self.yank_cols(self.cy, i + 1, self.cx),
            _ => None,
        }
    }

    fn toggle_visual(&mut self, linewise: bool) {
        match self.anchor {
            Some(_) if self.linewise == linewise => self.anchor = None,
            _ => {
                self.anchor = Some((self.cy, self.cx));
                self.linewise = linewise;
            }
        }
    }

    // --- text extraction ------------------------------------------------------

    fn line_text(&self, row: usize) -> String {
        self.lines.get(row).map(|l| l.iter().collect()).unwrap_or_default()
    }

    fn lines_text(&self, a: usize, b: usize) -> String {
        (a..=b).map(|r| self.line_text(r)).collect::<Vec<_>>().join("\n")
    }

    fn yank_cols(&self, row: usize, a: usize, b: usize) -> Option<String> {
        let line = self.lines.get(row)?;
        let (a, b) = (a.min(line.len()), b.min(line.len()));
        if a >= b {
            return None;
        }
        Some(line[a..b].iter().collect())
    }

    /// Text covered by the current visual selection (anchor → cursor).
    fn selection_text(&self) -> String {
        let Some((ay, ax)) = self.anchor else { return String::new() };
        let (cy, cx) = (self.cy, self.cx);
        let ((sy, sx), (ey, ex)) =
            if (ay, ax) <= (cy, cx) { ((ay, ax), (cy, cx)) } else { ((cy, cx), (ay, ax)) };
        if self.linewise {
            return self.lines_text(sy, ey);
        }
        if sy == ey {
            let line = &self.lines[sy];
            let end = (ex + 1).min(line.len());
            return line[sx.min(line.len())..end].iter().collect();
        }
        let mut out = String::new();
        let first = &self.lines[sy];
        out.extend(&first[sx.min(first.len())..]);
        for row in (sy + 1)..ey {
            out.push('\n');
            out.extend(&self.lines[row]);
        }
        out.push('\n');
        let last = &self.lines[ey];
        out.extend(&last[..(ex + 1).min(last.len())]);
        out
    }

    // --- viewport -------------------------------------------------------------

    fn ensure_visible(&mut self, rows: usize, cols: usize) {
        if self.cy < self.top {
            self.top = self.cy;
        } else if self.cy >= self.top + rows {
            self.top = self.cy + 1 - rows;
        }
        if self.cx < self.left {
            self.left = self.cx;
        } else if self.cx >= self.left + cols {
            self.left = self.cx + 1 - cols;
        }
    }

    /// Selected column span `[s0, s1)` on `row`, in absolute columns, if any. For
    /// linewise selection the whole line (plus one cell, to show the newline) is
    /// highlighted.
    pub fn selection_on_row(&self, row: usize) -> Option<(usize, usize)> {
        let (ay, ax) = self.anchor?;
        let (cy, cx) = (self.cy, self.cx);
        let ((sy, sx), (ey, ex)) =
            if (ay, ax) <= (cy, cx) { ((ay, ax), (cy, cx)) } else { ((cy, cx), (ay, ax)) };
        if row < sy || row > ey {
            return None;
        }
        let len = self.line_len(row);
        if self.linewise {
            return Some((0, len + 1));
        }
        let start = if row == sy { sx } else { 0 };
        let end = if row == ey { (ex + 1).min(len) } else { len + 1 };
        Some((start, end))
    }
}

/// Reverse a find direction for `,` (repeat-find backwards).
fn invert_find(kind: char) -> char {
    match kind {
        'f' => 'F',
        'F' => 'f',
        't' => 'T',
        'T' => 't',
        other => other,
    }
}

/// Word character: identifiers and digits (so `yiw` grabs `0x8007139f`).
fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Char class for `iw`: whitespace, word (identifier/number), or punctuation. A
/// text object is a maximal run of one class.
fn class(c: char) -> u8 {
    if c.is_whitespace() {
        0
    } else if is_word(c) {
        1
    } else {
        2
    }
}

/// Inner/around word range on `line` containing `cx`.
fn word_object(line: &[char], cx: usize, around: bool) -> Option<(usize, usize)> {
    if line.is_empty() {
        return None;
    }
    let cx = cx.min(line.len() - 1);
    let cls = class(line[cx]);
    let mut a = cx;
    while a > 0 && class(line[a - 1]) == cls {
        a -= 1;
    }
    let mut b = cx + 1;
    while b < line.len() && class(line[b]) == cls {
        b += 1;
    }
    // `aw` extends over the trailing whitespace run (vim semantics).
    if around && cls != 0 {
        while b < line.len() && line[b].is_whitespace() {
            b += 1;
        }
    }
    Some((a, b))
}

/// Range between the `open`/`close` pair surrounding `cx` (non-nested, single line).
/// `around` includes the delimiters.
fn pair_object(line: &[char], cx: usize, open: char, close: char, around: bool) -> Option<(usize, usize)> {
    // Nearest unmatched `open` at or before cx.
    let mut depth = 0i32;
    let mut o = None;
    for i in (0..=cx.min(line.len().saturating_sub(1))).rev() {
        if line[i] == close && i != cx {
            depth += 1;
        } else if line[i] == open {
            if depth == 0 {
                o = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let o = o?;
    // Matching `close` after it.
    let mut depth = 0i32;
    let mut c = None;
    for (i, &ch) in line.iter().enumerate().skip(o + 1) {
        if ch == open {
            depth += 1;
        } else if ch == close {
            if depth == 0 {
                c = Some(i);
                break;
            }
            depth -= 1;
        }
    }
    let c = c?;
    if around {
        Some((o, c + 1))
    } else {
        Some((o + 1, c))
    }
}

/// Range inside the pair of `q` quotes surrounding `cx` (single line). `around`
/// includes the quotes.
fn quote_object(line: &[char], cx: usize, q: char, around: bool) -> Option<(usize, usize)> {
    let positions: Vec<usize> = line.iter().enumerate().filter(|(_, &c)| c == q).map(|(i, _)| i).collect();
    for pair in positions.chunks_exact(2) {
        let (a, b) = (pair[0], pair[1]);
        if cx >= a && cx <= b {
            return if around { Some((a, b + 1)) } else { Some((a + 1, b)) };
        }
    }
    None
}
