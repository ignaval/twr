//! The editor behind the input box and the orchestrator line: text in lines (`\n` between
//! them; [`LineInput::insert_one_line`] for the orchestrator line, which has one), a cursor (a
//! byte index, always on a char boundary), and its view: each line soft-wrapped into rows at
//! the box width ([`LineInput::rows`]), the box
//! as tall as the rows up to a cap ([`LineInput::height`]), scrolled vertically past the cap
//! to keep the cursor's row visible ([`LineInput::view`]). Pure: [`crate::app`] maps keys to
//! these operations and [`crate::ui`] draws the view.
//!
//! The text is read in grapheme clusters as ratatui draws it ([`clusters`]): its
//! extended grapheme clusters, each as wide as ratatui's cell width for it, so a base and its
//! combining marks, an emoji with its skin tone, variation selector or zero-width joiner
//! sequence, a flag (two regional indicators), a keycap and a tag sequence are one unit:
//! never split across rows, stepped over by Left and Right, removed whole by Backspace and
//! Delete, and placed and hit as one by the cursor and a click. Wide clusters (CJK, emoji) take
//! two cells. Words, for the word motions and Ctrl-W, are runs of non-whitespace.
//! Up and Down move between the rows of the wrapped text; Home and End go to the start and
//! end of the cursor's row, [`LineInput::home`] and [`LineInput::end`] to those of the text.
//! A line break is whitespace to the word motions; it is never drawn: the row it ends is
//! drawn without it.

use std::ops::Deref;

use ratatui::buffer::CellWidth;
use ratatui::style::Style;
use ratatui::text::Span;

/// The grapheme clusters of `s` as ratatui draws them: its extended grapheme
/// clusters (ratatui's own segmentation, `Span::styled_graphemes`), each with its byte offset.
/// A control character, which ratatui never draws, is a cluster of its own.
pub(crate) fn clusters(s: &str) -> Vec<(usize, &str)> {
    let span = Span::raw(s);
    let base = s.as_ptr() as usize;
    let drawn: Vec<(usize, usize)> = span
        .styled_graphemes(Style::default())
        .map(|g| {
            let a = g.symbol.as_ptr() as usize - base;
            (a, a + g.symbol.len())
        })
        .collect();
    let mut out = Vec::with_capacity(drawn.len());
    let mut at = 0;
    for (a, z) in drawn.into_iter().chain([(s.len(), s.len())]) {
        // Characters ratatui skipped (controls): one cluster each.
        for (i, c) in s[at..a].char_indices() {
            out.push((at + i, &s[at + i..at + i + c.len_utf8()]));
        }
        if z > a {
            out.push((a, &s[a..z]));
        }
        at = z;
    }
    out
}

/// Display width of one grapheme cluster in terminal cells, as ratatui draws it (a control
/// character counts as 0: ratatui draws nothing for it).
pub(crate) fn cluster_width(g: &str) -> usize {
    if g.contains(char::is_control) {
        0
    } else {
        usize::from(g.cell_width())
    }
}

/// Display width of `s` in terminal cells: the sum of its clusters' ([`clusters`]).
/// `s` as the input line takes it ([`LineInput::insert`]): a line break (`\r\n`, `\r` or
/// `\n`) as `\n`, a tab as a space, other control characters dropped.
pub fn clean_text(s: &str) -> String {
    let mut clean = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {}
            '\n' | '\r' => clean.push('\n'),
            '\t' => clean.push(' '),
            c if c.is_control() => {}
            c => clean.push(c),
        }
    }
    clean
}

pub fn width(s: &str) -> usize {
    clusters(s).into_iter().map(|(_, g)| cluster_width(g)).sum()
}

/// Editable text in lines, with a cursor, drawn wrapped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineInput {
    text: String,
    /// Byte index of the cursor in `text` (a char boundary, at most `text.len()`).
    cursor: usize,
    /// Byte index where the first row shown starts, as last kept by
    /// [`LineInput::keep_visible`]; [`LineInput::view`] corrects it for the current text and
    /// box.
    top: usize,
    /// The column Up and Down aim for, with the cursor they left: it holds while the cursor
    /// is still there (a run of Up/Down), and is forgotten once anything else moved it.
    goal: Option<(usize, usize)>,
}

impl Deref for LineInput {
    type Target = str;

    fn deref(&self) -> &str {
        &self.text
    }
}

impl From<&str> for LineInput {
    fn from(s: &str) -> Self {
        let mut l = LineInput::default();
        l.set(s);
        l
    }
}

impl From<String> for LineInput {
    fn from(s: String) -> Self {
        LineInput::from(s.as_str())
    }
}

impl PartialEq<&str> for LineInput {
    fn eq(&self, other: &&str) -> bool {
        self.text == *other
    }
}

impl PartialEq<str> for LineInput {
    fn eq(&self, other: &str) -> bool {
        self.text == other
    }
}

impl LineInput {
    /// The text.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The cursor's byte index.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Empties the line.
    pub fn clear(&mut self) {
        *self = LineInput::default();
    }

    /// Replaces the text (cleaned as by [`LineInput::insert`]); the cursor goes to the end.
    pub fn set(&mut self, text: &str) {
        self.clear();
        self.insert(text);
    }

    /// Inserts `s` at the cursor and moves the cursor after it. A line break (`\r\n`, `\r` or
    /// `\n`) goes in as `\n`, a tab as a space; other control characters are dropped.
    pub fn insert(&mut self, s: &str) {
        let clean = clean_text(s);
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
        self.snap();
    }

    /// Moves a cursor left inside a grapheme cluster ([`clusters`]) to its end. An edit can
    /// join clusters: a line break removed between a letter and the combining mark starting
    /// the next line makes them one character, and text typed before such a mark too.
    fn snap(&mut self) {
        let c = self.cursor;
        if let Some((i, g)) = clusters(&self.text)
            .into_iter()
            .find(|(i, g)| *i < c && c < i + g.len())
        {
            self.cursor = i + g.len();
        }
    }

    /// Inserts `s` as [`LineInput::insert`] does, but on the line the cursor is on: a line
    /// break becomes a space (`\r\n` one space). For the orchestrator line.
    pub fn insert_one_line(&mut self, s: &str) {
        self.insert(&s.replace("\r\n", " ").replace(['\r', '\n'], " "));
    }

    /// Breaks the line at the cursor (Shift-Enter): a `\n` goes in before it.
    pub fn newline(&mut self) {
        self.insert("\n");
    }

    /// Replaces bytes `start..end` of the text (char boundaries) with `s`, cleaned as by
    /// [`LineInput::insert`], and puts the cursor after it.
    pub fn replace(&mut self, start: usize, end: usize, s: &str) {
        self.text.replace_range(start..end, "");
        self.cursor = start;
        self.insert(s);
    }

    /// Byte index of the start of the cluster before `i` ([`clusters`]; 0 at the start).
    fn prev(&self, i: usize) -> usize {
        clusters(&self.text)
            .into_iter()
            .rev()
            .find(|(j, _)| *j < i)
            .map_or(0, |(j, _)| j)
    }

    /// Byte index of the end of the cluster after `i` ([`clusters`]; the end at the end).
    fn next(&self, i: usize) -> usize {
        clusters(&self.text)
            .into_iter()
            .map(|(j, g)| j + g.len())
            .find(|z| *z > i)
            .unwrap_or(self.text.len())
    }

    /// Deletes the character (grapheme cluster) before the cursor.
    pub fn backspace(&mut self) {
        let p = self.prev(self.cursor);
        self.text.replace_range(p..self.cursor, "");
        self.cursor = p;
        self.snap();
    }

    /// Deletes the character (grapheme cluster) at the cursor.
    pub fn delete(&mut self) {
        let n = self.next(self.cursor);
        self.text.replace_range(self.cursor..n, "");
        self.snap();
    }

    /// One character (grapheme cluster) left.
    pub fn left(&mut self) {
        self.cursor = self.prev(self.cursor);
    }

    /// One character (grapheme cluster) right.
    pub fn right(&mut self) {
        self.cursor = self.next(self.cursor);
    }

    /// To the start of the text.
    pub fn home(&mut self) {
        self.cursor = 0;
    }

    /// To the end of the text.
    pub fn end(&mut self) {
        self.cursor = self.text.len();
    }

    /// The start of the word before the cursor (whitespace before the cursor skipped first).
    fn word_start(&self) -> usize {
        let before = &self.text[..self.cursor];
        let trimmed = before.trim_end();
        trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map_or(0, |(i, c)| i + c.len_utf8())
    }

    /// The end of the word after the cursor (whitespace after the cursor skipped first).
    fn word_end(&self) -> usize {
        let after = &self.text[self.cursor..];
        let skipped = after.len() - after.trim_start().len();
        let rest = &after[skipped..];
        let word = rest.find(char::is_whitespace).unwrap_or(rest.len());
        self.cursor + skipped + word
    }

    /// To the start of the word before the cursor.
    pub fn word_left(&mut self) {
        self.cursor = self.word_start();
    }

    /// To the end of the word after the cursor.
    pub fn word_right(&mut self) {
        self.cursor = self.word_end();
    }

    /// Deletes the word before the cursor (Ctrl-W).
    pub fn delete_word_before(&mut self) {
        let start = self.word_start();
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.snap();
    }

    /// Deletes from the cursor to the end of its line (Ctrl-K); at the end of a line, the line
    /// break, joining the next line to it.
    pub fn kill_to_end(&mut self) {
        let end = match self.text[self.cursor..].find('\n') {
            Some(0) => self.cursor + 1,
            Some(n) => self.cursor + n,
            None => self.text.len(),
        };
        self.text.replace_range(self.cursor..end, "");
        self.snap();
    }

    /// The rows the text soft-wraps into in a box `cols` cells wide (at least one): byte
    /// ranges in order, each starting where the one before ends, the last ending at the end
    /// of the text, always between grapheme clusters ([`clusters`]). A row ends after each
    /// line break (the `\n` is the last byte of its row, not drawn), and breaks after a run of
    /// spaces when the next word does not fit on it; a word wider than the box is cut at its
    /// edge; a space reaching past the edge stays at the end of its row (hanging, not drawn)
    /// with the spaces after it, and a line break right after those, too. When a line fills
    /// its row, an empty row follows it (holding only its `\n`, or nothing at the end of the
    /// text): the cursor at the line's end goes there.
    pub fn rows(&self, cols: usize) -> Vec<(usize, usize)> {
        let cols = cols.max(1);
        let t = &self.text;
        let mut rows: Vec<(usize, usize)> = Vec::new();
        let (mut start, mut w) = (0, 0);
        // Where the current row may break: after the last space on it.
        let mut brk: Option<usize> = None;
        // The row before ended at a hanging space: the spaces right after it hang too.
        let mut hanging = false;
        for (i, g) in clusters(t) {
            let cw = cluster_width(g);
            let next = i + g.len();
            if g == "\n" {
                match rows.last_mut() {
                    Some(last) if hanging && i == start => *last = (last.0, next),
                    _ => {
                        // A line that fills its row: the cursor at its end goes to the next.
                        if w >= cols && i > start {
                            rows.push((start, i));
                            start = i;
                        }
                        rows.push((start, next));
                    }
                }
                (start, w, brk, hanging) = (next, 0, None, false);
                continue;
            }
            if g.chars().all(char::is_whitespace) {
                if hanging && i == start {
                    if let Some(last) = rows.last_mut() {
                        *last = (last.0, next);
                    }
                    start = next;
                } else if w + cw > cols && i > start {
                    rows.push((start, next));
                    (start, w, brk, hanging) = (next, 0, None, true);
                } else {
                    w += cw;
                    brk = Some(next);
                }
                continue;
            }
            hanging = false;
            if w + cw > cols && i > start {
                if let Some(b) = brk.take() {
                    rows.push((start, b));
                    start = b;
                    w = width(&t[b..i]);
                }
                // No space to break at, or the word left is still too wide with `c`.
                if w + cw > cols && i > start {
                    rows.push((start, i));
                    (start, w) = (i, 0);
                }
            }
            w += cw;
        }
        rows.push((start, t.len()));
        if w >= cols {
            rows.push((t.len(), t.len()));
        }
        rows
    }

    /// The last position the cursor can take on row `r` of `rows`: its end on the last row;
    /// on any other, before the character it ends with (the space it breaks after, the line
    /// break, or the last character of a word cut at the edge), since its end is where the
    /// next row starts.
    fn row_last(&self, rows: &[(usize, usize)], r: usize) -> usize {
        let (s, e) = rows[r];
        if r + 1 == rows.len() {
            e
        } else {
            self.prev(e).max(s)
        }
    }

    /// The position on row `r` of `rows` before the cluster drawn at column `col` (at the
    /// row's last position past its text).
    fn pos_at(&self, rows: &[(usize, usize)], r: usize, col: usize) -> usize {
        let s = rows[r].0;
        let last = self.row_last(rows, r);
        let mut x = 0;
        for (i, g) in clusters(&self.text) {
            if i < s || i >= last {
                continue;
            }
            let w = cluster_width(g);
            if x + w > col {
                return i;
            }
            x += w;
        }
        last
    }

    /// The column of the cursor on its row of `rows`, and the row.
    fn locate(&self, rows: &[(usize, usize)]) -> (usize, usize) {
        let r = row_of(rows, self.cursor);
        (r, width(&self.text[rows[r].0..self.cursor]))
    }

    /// Up (`down` false) or down one row in a box `cols` cells wide, aiming for the column
    /// the cursor had when the first of a run of Up/Down started; from the first row up to
    /// the start, from the last row down to the end.
    fn vertical(&mut self, cols: usize, down: bool) {
        let rows = self.rows(cols);
        let (r, col) = self.locate(&rows);
        let col = match self.goal {
            Some((at, goal)) if at == self.cursor => goal,
            _ => col,
        };
        self.cursor = match (down, r) {
            (false, 0) => 0,
            (false, r) => self.pos_at(&rows, r - 1, col),
            (true, r) if r + 1 == rows.len() => self.text.len(),
            (true, r) => self.pos_at(&rows, r + 1, col),
        };
        self.goal = Some((self.cursor, col));
    }

    /// One row up in a box `cols` cells wide ([`LineInput::rows`]), keeping the column; from
    /// the first row, to the start.
    pub fn up(&mut self, cols: usize) {
        self.vertical(cols, false);
    }

    /// One row down in a box `cols` cells wide, keeping the column; from the last row, to
    /// the end.
    pub fn down(&mut self, cols: usize) {
        self.vertical(cols, true);
    }

    /// Whether the cursor is on the first row of a box `cols` cells wide.
    pub fn on_first_row(&self, cols: usize) -> bool {
        row_of(&self.rows(cols), self.cursor) == 0
    }

    /// Whether the cursor is on the last row of a box `cols` cells wide.
    pub fn on_last_row(&self, cols: usize) -> bool {
        let rows = self.rows(cols);
        row_of(&rows, self.cursor) + 1 >= rows.len()
    }

    /// To the start of the cursor's row in a box `cols` cells wide.
    pub fn row_home(&mut self, cols: usize) {
        let rows = self.rows(cols);
        self.cursor = rows[row_of(&rows, self.cursor)].0;
    }

    /// To the end of the cursor's row in a box `cols` cells wide (before the space it wraps
    /// at).
    pub fn row_end(&mut self, cols: usize) {
        let rows = self.rows(cols);
        self.cursor = self.row_last(&rows, row_of(&rows, self.cursor));
    }

    /// Rows a box `cols` cells wide shows: as many as the text wraps into, at least one, at
    /// most `cap`.
    pub fn height(&self, cols: usize, cap: usize) -> usize {
        self.rows(cols).len().clamp(1, cap.max(1))
    }

    /// The first row of `rows` a box `height` rows tall shows: the kept one, moved just
    /// enough to show the cursor's row, and back while rows below are empty (nothing is
    /// hidden above that could show).
    fn first_row(&self, rows: &[(usize, usize)], height: usize) -> usize {
        let h = height.max(1);
        let cur = row_of(rows, self.cursor);
        let mut first = row_of(rows, self.top);
        if cur < first {
            first = cur;
        }
        if cur >= first + h {
            first = cur + 1 - h;
        }
        first.min(rows.len().saturating_sub(h))
    }

    /// Keeps the first row [`LineInput::view`] shows in a box `cols` x `height`, so the view
    /// scrolls only when the cursor would leave it.
    pub fn keep_visible(&mut self, cols: usize, height: usize) {
        let rows = self.rows(cols);
        self.top = rows[self.first_row(&rows, height)].0;
    }

    /// What a box `cols` cells wide and `height` rows tall shows: its rows of text, and the
    /// cursor's row and column in them (a cursor past the right edge, before a space hanging
    /// there, shows on the last cell).
    pub fn view(&self, cols: usize, height: usize) -> View<'_> {
        let rows = self.rows(cols);
        let first = self.first_row(&rows, height);
        let (r, col) = self.locate(&rows);
        let end = (first + height.max(1)).min(rows.len());
        View {
            rows: rows[first..end]
                .iter()
                .map(|&(s, e)| &self.text[s..e])
                .collect(),
            cursor: (r - first, col.min(cols.saturating_sub(1))),
        }
    }

    /// Puts the cursor before the character drawn at column `col` of row `row` of a box
    /// `cols` x `height` (at the end of that row past its text; at the end below the text).
    pub fn click(&mut self, col: usize, row: usize, cols: usize, height: usize) {
        let rows = self.rows(cols);
        let r = self.first_row(&rows, height) + row;
        self.cursor = if r < rows.len() {
            self.pos_at(&rows, r, col)
        } else {
            self.text.len()
        };
    }
}

/// What the input box shows ([`LineInput::view`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View<'a> {
    /// Its rows of text, from the top (a row may end with the space it wraps at, or with the
    /// `\n` that ends its line).
    pub rows: Vec<&'a str>,
    /// The cursor's row in `rows` and its column.
    pub cursor: (usize, usize),
}

/// The row of `rows` position `pos` is on: the last one starting at or before it (a position
/// where a row ends and the next starts is on the next).
fn row_of(rows: &[(usize, usize)], pos: usize) -> usize {
    rows.partition_point(|r| r.0 <= pos).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(l: &LineInput) -> (String, String) {
        (
            l.text[..l.cursor].to_string(),
            l.text[l.cursor..].to_string(),
        )
    }

    #[test]
    fn edits_happen_at_the_cursor() {
        let mut l = LineInput::from("hello world");
        assert_eq!(l.cursor(), 11);
        l.left();
        l.left();
        l.insert("X");
        assert_eq!(at(&l), ("hello worX".into(), "ld".into()));
        l.backspace();
        l.delete();
        assert_eq!(at(&l), ("hello wor".into(), "d".into()));
        l.home();
        l.delete();
        l.left();
        l.backspace();
        assert_eq!(at(&l), ("".into(), "ello word".into()));
        l.end();
        l.right();
        l.delete();
        assert_eq!(l, "ello word");
        l.home();
        l.right();
        l.kill_to_end();
        assert_eq!(l, "e");
        l.clear();
        assert_eq!((l.as_str(), l.cursor()), ("", 0));
        l.backspace();
        l.delete();
        l.left();
        assert_eq!((l.as_str(), l.cursor()), ("", 0));
        // A range replaced, the cursor after what went in.
        let mut l = LineInput::from("ask @we now");
        l.replace(4, 7, "@web\t");
        assert_eq!(at(&l), ("ask @web ".into(), " now".into()));
    }

    #[test]
    fn words_are_runs_of_non_whitespace() {
        let mut l = LineInput::from("say  hi to/the api  ");
        l.word_left();
        assert_eq!(at(&l).1, "api  ");
        l.word_left();
        l.word_left();
        assert_eq!(at(&l).1, "hi to/the api  ");
        l.word_right();
        assert_eq!(at(&l).0, "say  hi");
        l.word_right();
        assert_eq!(at(&l).0, "say  hi to/the");
        l.end();
        l.delete_word_before();
        assert_eq!(l, "say  hi to/the ");
        l.delete_word_before();
        assert_eq!(l, "say  hi ");
        l.home();
        l.word_left();
        assert_eq!(l.cursor(), 0);
        l.delete_word_before();
        assert_eq!(l, "say  hi ");
        l.end();
        l.word_right();
        assert_eq!(l.cursor(), l.len());
    }

    #[test]
    fn wide_and_multibyte_characters_keep_the_cursor_on_char_boundaries() {
        let mut l = LineInput::from("a日本é🙂b");
        for _ in 0..10 {
            l.left();
            assert!(l.text.is_char_boundary(l.cursor()));
        }
        assert_eq!(l.cursor(), 0);
        l.right();
        l.right();
        l.insert("語");
        assert_eq!(l, "a日語本é🙂b");
        l.right();
        l.right();
        l.backspace();
        assert_eq!(l, "a日語本🙂b");
        l.delete();
        assert_eq!(l, "a日語本b");
        assert_eq!(width("a日語本b"), 8);
        l.end();
        l.delete_word_before();
        assert_eq!(l, "");
    }

    #[test]
    fn pasted_line_breaks_stay_tabs_become_spaces_and_control_characters_go() {
        let mut l = LineInput::from("ab");
        l.left();
        l.insert("1\r\n2\n3\r4\t5\u{1b}[2J\u{7}6");
        assert_eq!(l, "a1\n2\n3\n4 5[2J6b");
        assert_eq!(at(&l).1, "b");
        // On the orchestrator line, a line break is a space.
        let mut l = LineInput::from("ab");
        l.left();
        l.insert_one_line("1\r\n2\n3\r4\t5");
        assert_eq!(l, "a1 2 3 4 5b");
        // Shift-Enter breaks the line at the cursor.
        let mut l = LineInput::from("one two");
        l.word_left();
        l.newline();
        assert_eq!(at(&l), ("one \n".into(), "two".into()));
    }

    #[test]
    fn rows_end_at_line_breaks() {
        assert_eq!(shown(&LineInput::from("ab\ncd"), 8), ["ab\n", "cd"]);
        assert_eq!(shown(&LineInput::from("\n\n"), 8), ["\n", "\n", ""]);
        assert_eq!(shown(&LineInput::from("ab\n"), 8), ["ab\n", ""]);
        // Each line wraps on its own.
        assert_eq!(
            shown(&LineInput::from("hello world\nfoo bar"), 6),
            ["hello ", "world\n", "foo ", "bar"]
        );
        // A line that fills its row: an empty row for its end (and its line break).
        assert_eq!(shown(&LineInput::from("abc\nd"), 3), ["abc", "\n", "d"]);
        assert_eq!(shown(&LineInput::from("abc\n"), 3), ["abc", "\n", ""]);
        // A break right after a space hanging at the edge goes on that row.
        assert_eq!(shown(&LineInput::from("abc \nd"), 3), ["abc \n", "d"]);
        assert_eq!(
            shown(&LineInput::from("abc  \n\nd"), 3),
            ["abc  \n", "\n", "d"]
        );
        // The box grows with the lines.
        assert_eq!(LineInput::from("a\nb\nc").height(80, 6), 3);
        assert_eq!(LineInput::from("a\nb\nc\nd\ne\nf\ng\nh").height(80, 6), 6);
    }

    #[test]
    fn the_cursor_moves_across_line_breaks() {
        // Rows "one\n", "\n", "three".
        let mut l = LineInput::from("one\n\nthree");
        assert_eq!(l.view(8, 6).cursor, (2, 5));
        l.up(8);
        assert_eq!(at(&l).0, "one\n", "the empty line");
        assert_eq!(l.view(8, 6).cursor, (1, 0));
        l.up(8);
        assert_eq!(
            at(&l).0,
            "one",
            "the end of the first line, before its break"
        );
        assert_eq!(l.view(8, 6).cursor, (0, 3));
        l.down(8);
        l.down(8);
        assert_eq!(
            at(&l).0,
            "one\n\nthree",
            "column 5 held across the shorter lines"
        );
        // Home and End stay on the line; End stops before the break.
        l.up(8);
        l.up(8);
        l.row_home(8);
        assert_eq!(l.cursor(), 0);
        l.row_end(8);
        assert_eq!(at(&l).0, "one");
        // Left and Right step over the break as one character.
        l.right();
        assert_eq!(at(&l).0, "one\n");
        l.left();
        assert_eq!(at(&l).0, "one");
        // A click past a line's text lands before its break.
        l.click(7, 0, 8, 6);
        assert_eq!(at(&l).0, "one");
        l.click(4, 1, 8, 6);
        assert_eq!(at(&l).0, "one\n");
        // A line that fills its row: its end is the start of the row after it.
        let mut l = LineInput::from("abc\nd");
        l.home();
        l.row_end(3);
        l.right();
        assert_eq!(at(&l).0, "abc");
        assert_eq!(l.view(3, 6).cursor, (1, 0));
        // Backspace at a line's start joins it to the line above.
        let mut l = LineInput::from("ab\ncd");
        l.left();
        l.left();
        l.backspace();
        assert_eq!(at(&l), ("ab".into(), "cd".into()));
    }

    /// Review: removing a line break can join the characters around it into one grapheme
    /// cluster (a letter and a combining mark, an emoji and a zero-width joiner sequence);
    /// the cursor never stays inside it.
    #[test]
    fn joining_lines_keeps_the_cursor_between_characters() {
        let accent = "a\n\u{301}b";
        let zwj = "\u{1f469}\n\u{200d}\u{1f4bb}b";
        for text in [accent, zwj] {
            let joined = text.replacen('\n', "", 1);
            let whole = clusters(&joined)[0].1.len();
            // Ctrl-K at the end of the first line, Delete there, Backspace after the break.
            for how in 0..3 {
                let mut l = LineInput::from(text);
                l.home();
                for _ in 0..usize::from(how == 2) + 1 {
                    l.right();
                }
                match how {
                    0 => l.kill_to_end(),
                    1 => l.delete(),
                    _ => l.backspace(),
                }
                assert_eq!(l, joined.as_str(), "{text:?} {how}");
                assert_eq!(
                    l.cursor(),
                    whole,
                    "{text:?} {how}: after the joined character"
                );
                l.backspace();
                assert_eq!(l, "b", "{text:?} {how}: removed whole");
            }
        }
        // Text typed before a combining mark that starts a line joins it too.
        let mut l = LineInput::from("\n\u{301}b");
        l.home();
        l.delete();
        l.home();
        l.insert("e");
        assert_eq!(l.cursor(), "e\u{301}".len());
    }

    #[test]
    fn ctrl_k_deletes_to_the_end_of_the_line_then_its_break() {
        let mut l = LineInput::from("one two\nthree\nfour");
        l.home();
        l.word_right();
        l.kill_to_end();
        assert_eq!(l, "one\nthree\nfour");
        l.kill_to_end();
        assert_eq!(l, "onethree\nfour");
        l.end();
        l.kill_to_end();
        assert_eq!(l, "onethree\nfour");
        // Ctrl-W treats a line break as a space.
        l.delete_word_before();
        assert_eq!(l, "onethree\n");
        l.delete_word_before();
        assert_eq!(l, "");
    }

    /// The rows of `l` in a box `cols` wide, as text.
    fn shown(l: &LineInput, cols: usize) -> Vec<&str> {
        l.rows(cols).iter().map(|&(s, e)| &l.text[s..e]).collect()
    }

    #[test]
    fn text_wraps_after_spaces_by_display_width() {
        // A word that does not fit moves down; the space it follows ends the row above.
        assert_eq!(
            shown(&LineInput::from("hello world foo"), 8),
            ["hello ", "world ", "foo"]
        );
        // A word wider than the box is cut at its edge.
        assert_eq!(
            shown(&LineInput::from("abcdefghij"), 4),
            ["abcd", "efgh", "ij"]
        );
        // Wide characters take two cells and never split: a 5-cell row holds two of them.
        assert_eq!(
            shown(&LineInput::from("日本語です"), 5),
            ["日本", "語で", "す"]
        );
        assert_eq!(shown(&LineInput::from("ab日"), 3), ["ab", "日"]);
        assert_eq!(shown(&LineInput::from("a 日本"), 4), ["a ", "日本", ""]);
        // Spaces at the edge hang at the end of their row instead of starting the next.
        assert_eq!(shown(&LineInput::from("abc   d"), 3), ["abc   ", "d"]);
        // A full last row gets an empty one after it, for the cursor at the end.
        assert_eq!(shown(&LineInput::from("abcd"), 4), ["abcd", ""]);
        assert_eq!(shown(&LineInput::from("abc"), 4), ["abc"]);
        assert_eq!(shown(&LineInput::default(), 4), [""]);
        // A zero-width box wraps as a one-cell one.
        assert_eq!(shown(&LineInput::from("ab"), 0), ["a", "b", ""]);
    }

    #[test]
    fn the_box_grows_to_its_cap_then_scrolls_to_the_cursor() {
        let mut l = LineInput::from("abcdefghijklmnopqrstuvwxyz");
        // Six rows of five cells: as tall as that, up to the cap.
        assert_eq!(l.height(5, 10), 6);
        assert_eq!(l.height(5, 3), 3);
        assert_eq!(l.height(5, 0), 1);
        assert_eq!(LineInput::default().height(5, 6), 1);
        // Past the cap, the rows around the cursor (at the end) show.
        l.keep_visible(5, 3);
        let v = l.view(5, 3);
        assert_eq!((v.rows, v.cursor), (vec!["pqrst", "uvwxy", "z"], (2, 1)));
        // Up inside the view does not scroll it; past its top, it follows the cursor.
        for _ in 0..2 {
            l.up(5);
            l.keep_visible(5, 3);
        }
        assert_eq!(l.view(5, 3).cursor, (0, 1));
        l.up(5);
        l.keep_visible(5, 3);
        let v = l.view(5, 3);
        assert_eq!(
            (v.rows, v.cursor),
            (vec!["klmno", "pqrst", "uvwxy"], (0, 1))
        );
        l.down(5);
        l.keep_visible(5, 3);
        assert_eq!(
            l.view(5, 3).rows[0],
            "klmno",
            "down inside the view: no scroll"
        );
        assert_eq!(l.view(5, 3).cursor, (1, 1));
        l.home();
        l.keep_visible(5, 3);
        assert_eq!(l.view(5, 3).rows, ["abcde", "fghij", "klmno"]);
        // Text that fits again shows from its first row.
        l.end();
        l.keep_visible(5, 3);
        for _ in 0..15 {
            l.backspace();
        }
        l.keep_visible(5, 3);
        let v = l.view(5, 3);
        assert_eq!((v.rows, v.cursor), (vec!["abcde", "fghij", "k"], (2, 1)));
        // A narrower box (resize) scrolls on its own; a zero-sized one does not panic.
        let l = LineInput::from("0123456789");
        assert_eq!(l.view(4, 2).rows, ["4567", "89"]);
        assert_eq!(l.view(0, 0).cursor, (0, 0));
    }

    #[test]
    fn a_cursor_at_a_row_edge_shows_where_typing_goes() {
        // At the end of a full row: on the empty row below.
        let l = LineInput::from("abcd");
        assert_eq!(l.view(4, 6).cursor, (1, 0));
        // Before a space hanging past the edge: on the row's last cell.
        let mut l = LineInput::from("abc d");
        l.left();
        l.left();
        assert_eq!(l.view(3, 6).cursor, (0, 2));
        // After it: the start of the next row.
        l.right();
        assert_eq!(l.view(3, 6).cursor, (1, 0));
    }

    #[test]
    fn up_and_down_move_between_rows_keeping_the_column() {
        // Rows "hello ", "world ", "foo"; the cursor at the end (row 2, column 3).
        let mut l = LineInput::from("hello world foo");
        l.up(8);
        assert_eq!(at(&l).0, "hello wor");
        l.up(8);
        assert_eq!(at(&l).0, "hel");
        // From the first row, Up goes to the start; from the last, Down to the end.
        l.up(8);
        assert_eq!(l.cursor(), 0);
        l.down(8);
        assert_eq!(at(&l).0, "hello wor", "the column held through the start");
        l.down(8);
        l.down(8);
        assert_eq!(l.cursor(), l.len());

        // The column a run of Up/Down started at holds across a shorter row; any other
        // motion sets a new one.
        let mut l = LineInput::from("abcdefgh ab abcdefgh");
        assert_eq!(shown(&l, 9), ["abcdefgh ", "ab ", "abcdefgh"]);
        l.up(9);
        assert_eq!(at(&l).0, "abcdefgh ab", "the end of the short row");
        l.up(9);
        assert_eq!(at(&l).0, "abcdefgh", "back at column 8");
        l.left();
        l.down(9);
        l.down(9);
        assert_eq!(at(&l).0, "abcdefgh ab abcdefg", "column 7 now");

        // Wide characters: a column inside one lands before it.
        let mut l = LineInput::from("日本語です");
        l.home();
        l.right();
        l.down(5);
        assert_eq!(at(&l).0, "日本語", "column 2 on the second row");
        l.down(5);
        assert_eq!(at(&l).0, "日本語です", "the last row's end");
        l.home();
        l.right();
        l.left();
        // Left set a new column: 0.
        l.down(5);
        assert_eq!(at(&l).0, "日本");
    }

    #[test]
    fn home_and_end_go_to_the_row() {
        let mut l = LineInput::from("hello world foo");
        l.up(8);
        l.row_home(8);
        assert_eq!(at(&l).0, "hello ");
        l.row_end(8);
        assert_eq!(
            at(&l),
            ("hello world".into(), " foo".into()),
            "before the space"
        );
        l.down(8);
        l.row_end(8);
        assert_eq!(l.cursor(), l.len());
        l.row_home(8);
        assert_eq!(at(&l).1, "foo");
        // A row cut inside a word ends before its last character (its end is the next row's
        // start).
        let mut l = LineInput::from("abcdefghij");
        l.home();
        l.down(4);
        l.row_end(4);
        assert_eq!(at(&l).0, "abcdefg");
        // On one row, Home and End are the start and the end.
        let mut l = LineInput::from("short");
        l.row_home(80);
        assert_eq!(l.cursor(), 0);
        l.row_end(80);
        assert_eq!(l.cursor(), 5);
    }

    #[test]
    fn a_click_lands_on_the_row_and_cell_clicked() {
        let mut l = LineInput::from("hello world foo");
        l.click(2, 1, 8, 3);
        assert_eq!(at(&l).0, "hello wo");
        // Past a row's text: its end, before the space it wraps at.
        l.click(7, 0, 8, 3);
        assert_eq!(at(&l).0, "hello");
        l.click(7, 2, 8, 3);
        assert_eq!(l.cursor(), l.len());
        // Below the text: the end.
        l.home();
        l.click(0, 5, 8, 6);
        assert_eq!(l.cursor(), l.len());
        // A click on either cell of a wide character puts the cursor before it.
        let mut l = LineInput::from("日本語です");
        l.click(1, 1, 5, 3);
        assert_eq!(at(&l).0, "日本");
        l.click(3, 1, 5, 3);
        assert_eq!(at(&l).0, "日本語");
        // In a scrolled box, rows count from the first one shown.
        let mut l = LineInput::from("abcdefghijklmnopqrstuvwxyz");
        l.keep_visible(5, 3);
        l.click(0, 0, 5, 3);
        assert_eq!(at(&l).0, "abcdefghijklmno");
    }

    /// The text wraps, and the cursor moves, deletes and lands, by grapheme
    /// cluster with ratatui's width for each: a skin tone, a ZWJ sequence, a flag, a keycap, a
    /// combining accent and a CJK character never split across rows.
    #[test]
    fn graphemes_are_one_unit_at_row_edges() {
        let thumbs = "\u{1f44d}\u{1f3fd}";
        let coder = "\u{1f469}\u{200d}\u{1f4bb}";
        let flag = "\u{1f1e6}\u{1f1f7}";
        let keycap = "1\u{fe0f}\u{20e3}";
        let accent = "e\u{301}";
        let tag = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}";
        for (g, w) in [
            (thumbs, 2),
            (coder, 2),
            (flag, 2),
            (keycap, 2),
            (accent, 1),
            ("日", 2),
            (tag, 2),
        ] {
            assert_eq!(clusters(g).len(), 1, "{g:?} is one cluster");
            assert_eq!(width(g), w, "{g:?}");
            // At a row edge: "ab" fills two of three cells, the cluster goes down whole.
            let l = LineInput::from(format!("ab{g}").as_str());
            let rows = shown(&l, 3);
            if w == 2 {
                assert_eq!(rows, ["ab", g], "{g:?}");
            } else {
                assert_eq!(rows, [format!("ab{g}").as_str(), ""], "{g:?}");
            }
            // Left and Right step over it; Backspace and Delete remove it whole.
            let mut l = LineInput::from(format!("a{g}b").as_str());
            l.left();
            l.left();
            assert_eq!(at(&l).0, "a", "{g:?}");
            l.right();
            assert_eq!(at(&l).1, "b", "{g:?}");
            l.backspace();
            assert_eq!(l, "ab", "{g:?}");
            let mut l = LineInput::from(format!("a{g}b").as_str());
            l.home();
            l.right();
            l.delete();
            assert_eq!(l, "ab", "{g:?}");
        }
        // Several in a row wrap two cells at a time, the cursor's column counts cells, and a
        // click on either cell of one lands before it.
        let text = format!("{thumbs}{coder}{flag}{keycap}");
        let mut l = LineInput::from(text.as_str());
        assert_eq!(
            shown(&l, 5),
            [format!("{thumbs}{coder}"), format!("{flag}{keycap}")]
        );
        assert_eq!(l.view(5, 6).cursor, (1, 4));
        l.left();
        assert_eq!(l.view(5, 6).cursor, (1, 2));
        l.click(3, 0, 5, 6);
        assert_eq!(at(&l).0, thumbs);
        l.click(2, 0, 5, 6);
        assert_eq!(at(&l).0, thumbs);
        l.click(1, 1, 5, 6);
        assert_eq!(at(&l).0, format!("{thumbs}{coder}"));
        // Up and Down keep the column in cells, landing before a cluster, never inside one
        // (a row that wraps ends before its last cluster).
        l.end();
        l.up(5);
        assert_eq!(at(&l).0, thumbs);
        l.up(5);
        assert_eq!(l.cursor(), 0);
        // Row Home and End stop at cluster edges.
        let mut l = LineInput::from(format!("{accent}{accent}{accent}{accent}").as_str());
        assert_eq!(
            shown(&l, 3),
            [format!("{accent}{accent}{accent}"), accent.to_string()]
        );
        l.home();
        l.row_end(3);
        assert_eq!(at(&l).0, format!("{accent}{accent}"));
    }
}
