//! Markdown in the main pane, drawn the way Claude Code draws a reply: `**bold**` bold,
//! `` `code` `` in light blue without its backticks, headings bold without their `#`s, list
//! items with their marker and a hanging indent, fenced code blocks without their fences, quotes
//! after a dim bar, and rules as a dim line. Markers that open nothing (an unclosed `**`, a
//! lone backtick) stay as typed, so text still streaming in reads as written.
//!
//! [`rows`] wraps the result to a width, each row with how it continues the row above
//! ([`Join`]) and how many of its leading cells are indentation ([`MdRow::gutter`]): a copy
//! leaves those out, so a wrapped list item copies as one line.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::edit::{cluster_width, clusters};
use crate::select::Join;

/// Inline code and the text of code blocks.
pub const CODE: Color = Color::Indexed(153);

/// Dim text: rules, quote bars, link targets.
const DIM: Color = Color::DarkGray;

/// One row of rendered markdown.
#[derive(Debug, Clone, PartialEq)]
pub struct MdRow {
    /// The row, its indentation included.
    pub line: Line<'static>,
    /// How it continues the row above.
    pub join: Join,
    /// How many leading cells are indentation (left out of a copy).
    pub gutter: usize,
}

/// `text` as typed, wrapped to `width` cells after `indent`, each line keeping its own
/// indentation (copied: only `indent` is the gutter): a prompt reads this way.
pub fn plain_rows(text: &str, width: usize, indent: usize) -> Vec<MdRow> {
    let width = width.max(1);
    let text = text.replace('\t', "    ");
    let mut out = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        let body = raw.trim_start_matches(' ');
        if body.trim().is_empty() {
            out.push(MdRow {
                line: Line::default(),
                join: Join::Newline,
                gutter: 0,
            });
            continue;
        }
        let at = indent + (raw.len() - body.len());
        let mut rows = wrap(vec![(body.to_string(), Style::default())], width, at, at);
        if let Some(first) = rows.first_mut() {
            first.gutter = first.gutter.min(indent);
        }
        out.extend(rows);
    }
    out
}

/// `text` as markdown (CommonMark, with strikethrough and task lists), wrapped to `width`
/// cells, every row indented by `indent` cells. A line break inside a paragraph starts a new
/// row, as the agent wrote it. Each row's lead (its indentation, the bars of the quotes it is
/// in, the marker of a list item it starts) is built in one place ([`Render::emit`]) from the
/// containers open around it, so markers and bars line up however they nest.
pub fn rows(text: &str, width: usize, indent: usize) -> Vec<MdRow> {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    // A tab is four spaces (a terminal would not draw it).
    let text = text.replace('\t', "    ");
    let mut r = Render {
        width: width.max(1),
        out: Vec::new(),
        line: Vec::new(),
        line_bytes: 0,
        styles: vec![Style::default()],
        base: indent,
        open: Vec::new(),
        open_cells: 0,
        items: Vec::new(),
        quotes: 0,
        undrawn: Vec::new(),
        code: None,
        html: false,
        links: Vec::new(),
    };
    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for (ev, range) in Parser::new_ext(&text, opts).into_offset_iter() {
        match ev {
            Event::Start(tag) => match tag {
                Tag::Paragraph => r.block(),
                Tag::Heading { .. } => {
                    r.block();
                    r.push_style(|s| s.add_modifier(Modifier::BOLD));
                }
                Tag::BlockQuote(_) => {
                    r.block();
                    r.push_open(Open::Quote);
                    r.push_style(|s| s.add_modifier(Modifier::ITALIC));
                }
                Tag::CodeBlock(_) => {
                    r.block();
                    r.code = Some(String::new());
                }
                Tag::HtmlBlock => {
                    r.block();
                    r.html = true;
                }
                Tag::List(_) => {
                    // A list that opens an item keeps that item's marker for its first row.
                    if r.in_item() {
                        r.flush();
                    } else {
                        r.block();
                    }
                }
                Tag::Item => {
                    r.flush();
                    let marker = item_marker(&text[range.start..]);
                    let start = r.out.len();
                    r.push_open(Open::Item {
                        marker,
                        drawn: false,
                        start,
                    });
                }
                Tag::Emphasis => r.push_style(|s| s.add_modifier(Modifier::ITALIC)),
                Tag::Strong => r.push_style(|s| s.add_modifier(Modifier::BOLD)),
                Tag::Strikethrough => r.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT)),
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    r.links
                        .push((r.line.len(), r.line_bytes, dest_url.to_string()));
                    r.push_style(|s| s.add_modifier(Modifier::UNDERLINED));
                }
                _ => r.push_style(|s| s),
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => r.flush(),
                TagEnd::HtmlBlock => {
                    r.flush();
                    r.html = false;
                }
                TagEnd::Heading(_) => {
                    r.flush();
                    r.styles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    r.flush();
                    r.pop_open();
                    r.styles.pop();
                }
                TagEnd::CodeBlock => {
                    let code = r.code.take().unwrap_or_default();
                    for line in code.strip_suffix('\n').unwrap_or(&code).split('\n') {
                        r.verbatim(line, Style::default().fg(CODE));
                    }
                }
                TagEnd::List(_) => r.flush(),
                TagEnd::Item => {
                    r.flush();
                    // An empty item still shows its marker.
                    if matches!(r.open.last(), Some(Open::Item { drawn: false, .. })) {
                        r.emit(vec![r.empty_row()]);
                    }
                    r.pop_open();
                }
                TagEnd::Link | TagEnd::Image => {
                    r.styles.pop();
                    if let Some((at, at_bytes, url)) = r.links.pop() {
                        // Its label is the text since it opened: compared only when its length
                        // is the target's (nested links and images stay linear).
                        let same = at <= r.line.len()
                            && r.line_bytes.checked_sub(at_bytes) == Some(url.len())
                            && r.line[at..]
                                .iter()
                                .map(|(t, _)| t.as_str())
                                .collect::<String>()
                                == url;
                        if !same && !url.is_empty() {
                            r.put(format!(" ({url})"), Style::default().fg(DIM));
                        }
                    }
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    r.styles.pop();
                }
                _ => {
                    if r.styles.len() > 1 {
                        r.styles.pop();
                    }
                }
            },
            Event::Text(t) => match r.code.as_mut() {
                Some(code) => code.push_str(&t),
                None => r.text(&t),
            },
            Event::Code(t) => {
                let style = r.style().fg(CODE);
                r.put(t.to_string(), style);
            }
            // An HTML block (a `<thread name=…>` tag in a worker's text) is shown as written,
            // its spacing kept; inline HTML is text.
            Event::Html(t) if r.html => {
                for line in t.strip_suffix('\n').unwrap_or(&t).split('\n') {
                    if line.trim().is_empty() {
                        r.blank();
                    } else {
                        r.verbatim(line, Style::default());
                    }
                }
            }
            Event::Html(t) | Event::InlineHtml(t) => r.text(&t),
            Event::SoftBreak | Event::HardBreak => r.flush(),
            Event::Rule => {
                r.block();
                let n = r.width.saturating_sub(r.indent()).min(40);
                let rows = wrap(
                    vec![("─".repeat(n), Style::default().fg(DIM))],
                    r.width,
                    r.indent(),
                    r.indent(),
                );
                r.emit(rows);
            }
            Event::TaskListMarker(done) => {
                let style = r.style();
                r.put(if done { "[x] " } else { "[ ] " }.into(), style);
            }
            Event::FootnoteReference(t) => {
                let style = r.style();
                r.put(format!("[^{t}]"), style);
            }
            Event::InlineMath(t) | Event::DisplayMath(t) => r.text(&t),
        }
    }
    r.flush();
    let mut out = r.out;
    while out.last().is_some_and(|row| row.line.width() == 0) {
        out.pop();
    }
    out
}

/// A list item's marker as typed at the start of `src` (the item's source): `-`, `*`, `+`,
/// or its number with `.` or `)`; then a space.
fn item_marker(src: &str) -> String {
    let token = src
        .trim_start()
        .split(char::is_whitespace)
        .next()
        .unwrap_or("");
    let bullet = matches!(token, "-" | "*" | "+");
    let numbered = token.len() > 1
        && token.ends_with(['.', ')'])
        && token[..token.len() - 1].bytes().all(|b| b.is_ascii_digit());
    if bullet || numbered {
        format!("{token} ")
    } else {
        "- ".into()
    }
}

/// A container open around the text: a quote, or a list item.
enum Open {
    /// Its rows start with a dim `│ `.
    Quote,
    /// Its first row starts with `marker`, the others with as many spaces.
    Item {
        marker: String,
        /// Its marker was drawn.
        drawn: bool,
        /// How many rows there were when it opened: more now, and another block in it is set
        /// apart by a blank row.
        start: usize,
    },
}

/// The state of [`rows`] while it reads the events of a text.
struct Render {
    width: usize,
    out: Vec<MdRow>,
    /// The line being built: its spans, in their styles.
    line: Vec<(String, Style)>,
    /// The bytes of its text.
    line_bytes: usize,
    /// The styles in effect (bold in a heading, italic in a quote, ...), the last innermost.
    styles: Vec<Style>,
    /// The caller's indentation.
    base: usize,
    /// The quotes and items open, outermost first.
    open: Vec<Open>,
    /// The cells they take (kept as they open and close: deep nesting stays linear).
    open_cells: usize,
    /// Where the open items are in `open`, innermost last.
    items: Vec<usize>,
    /// How many quotes are open.
    quotes: usize,
    /// The open items whose marker is not drawn yet.
    undrawn: Vec<usize>,
    /// A code block's text, while one is open.
    code: Option<String>,
    /// An HTML block is open.
    html: bool,
    /// The open links (an image may sit in one): where each label starts in `line` (its span
    /// and the bytes before it), and its target.
    links: Vec<(usize, usize, String)>,
}

impl Render {
    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, f: impl FnOnce(Style) -> Style) {
        let style = f(self.style());
        self.styles.push(style);
    }

    /// The cells container `o` takes before the text.
    fn cells(o: &Open) -> usize {
        match o {
            Open::Quote => 2,
            Open::Item { marker, .. } => crate::edit::width(marker),
        }
    }

    fn push_open(&mut self, o: Open) {
        self.open_cells += Self::cells(&o);
        let at = self.open.len();
        match o {
            Open::Quote => self.quotes += 1,
            Open::Item { .. } => {
                self.items.push(at);
                self.undrawn.push(at);
            }
        }
        self.open.push(o);
    }

    fn pop_open(&mut self) {
        let Some(o) = self.open.pop() else {
            return;
        };
        self.open_cells -= Self::cells(&o);
        let at = self.open.len();
        match o {
            Open::Quote => self.quotes -= 1,
            Open::Item { .. } => {
                self.items.pop();
                if self.undrawn.last() == Some(&at) {
                    self.undrawn.pop();
                }
            }
        }
    }

    /// Adds `text` to the line being built.
    fn put(&mut self, text: String, style: Style) {
        self.line_bytes += text.len();
        self.line.push((text, style));
    }

    /// Where text starts, in cells: the caller's indentation and the open containers'.
    fn indent(&self) -> usize {
        self.base + self.open_cells
    }

    fn in_item(&self) -> bool {
        !self.items.is_empty()
    }

    /// Text in the current style; a line break in it ends a row.
    fn text(&mut self, t: &str) {
        let mut parts = t.split('\n').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                let style = self.style();
                self.put(part.to_string(), style);
            }
            if parts.peek().is_some() {
                self.flush();
            }
        }
    }

    /// A line shown as written (code, HTML), spaces included, cut into rows.
    fn verbatim(&mut self, line: &str, style: Style) {
        self.flush();
        let rows = wrap_code(line, self.width, self.indent(), style);
        self.emit(rows);
    }

    /// A block starts: what was being built ends, and a blank row sets it apart from what came
    /// before, unless it is the first block of a list item (items follow one another).
    fn block(&mut self) {
        // A loose list's item opens a paragraph: its marker waits for the item's text.
        if !self.line.is_empty() {
            self.flush();
        }
        let after_text = self.out.last().is_some_and(|r| r.line.width() > 0);
        let first_in_item = self
            .items
            .last()
            .and_then(|&i| self.open.get(i))
            .is_some_and(|o| matches!(o, Open::Item { start, .. } if *start == self.out.len()));
        if after_text && !first_in_item {
            self.blank();
        }
    }

    /// A blank row: the bars of the open quotes only (it draws no marker).
    fn blank(&mut self) {
        if self.quotes > 0 {
            let row = self.empty_row();
            self.lay(vec![row], false);
        } else {
            self.out.push(MdRow {
                line: Line::default(),
                join: Join::Newline,
                gutter: 0,
            });
        }
    }

    /// A row with nothing after its lead.
    fn empty_row(&self) -> MdRow {
        let lead = self.indent().min(self.width / 2);
        MdRow {
            line: Line::from(Span::raw(" ".repeat(lead))),
            join: Join::Newline,
            gutter: lead,
        }
    }

    /// Ends the line being built: its rows.
    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.line);
        self.line_bytes = 0;
        let indent = self.indent();
        let rows = wrap(spans, self.width, indent, indent);
        self.emit(rows);
    }

    /// Adds `rows` (each led by its indentation, [`wrap`]), the lead of each rebuilt from the
    /// open containers: a quote's bar, an item's marker on its first row (spaces after), within
    /// the cells the indentation had (the innermost kept when they do not all fit). The gutter
    /// a copy leaves out ends before the first marker drawn on the row.
    fn emit(&mut self, rows: Vec<MdRow>) {
        self.lay(rows, true);
    }

    /// [`Render::emit`]; `marks`: the row may draw the markers still to be drawn (a blank row
    /// does not).
    fn lay(&mut self, mut rows: Vec<MdRow>, marks: bool) {
        if self.open.is_empty() {
            self.out.extend(rows);
            return;
        }
        let dim = Style::default().fg(DIM);
        for r in &mut rows {
            let cells = r.line.spans.first().map_or(0, |s| s.width());
            // The containers that fit, innermost first, then in order: only those are looked
            // at (deep nesting costs no more than the row's width).
            let mut segs: Vec<(String, Style, bool)> = Vec::new();
            let mut used = 0;
            for o in self.open.iter().rev() {
                let w = Self::cells(o);
                if used + w > cells {
                    break;
                }
                used += w;
                segs.push(match o {
                    Open::Quote => ("│ ".into(), dim, false),
                    Open::Item {
                        marker,
                        drawn: false,
                        ..
                    } if marks => (marker.clone(), Style::default(), true),
                    Open::Item { marker, .. } => (
                        " ".repeat(crate::edit::width(marker)),
                        Style::default(),
                        false,
                    ),
                });
            }
            segs.reverse();
            if marks {
                for i in self.undrawn.drain(..) {
                    if let Some(Open::Item { drawn, .. }) = self.open.get_mut(i) {
                        *drawn = true;
                    }
                }
            }
            let pad = cells - used;
            let mut gutter = pad;
            let mut marked = false;
            for s in &segs {
                marked |= s.2;
                if !marked {
                    gutter += crate::edit::width(&s.0);
                }
            }
            let mut spans = vec![Span::raw(" ".repeat(pad))];
            spans.extend(segs.into_iter().map(|(t, st, _)| Span::styled(t, st)));
            spans.extend(r.line.spans.drain(..).skip(1));
            r.line = Line::from(spans);
            r.gutter = gutter;
        }
        self.out.extend(rows);
    }
}

/// A line shown as written (code, HTML) cut into rows of `width` cells after `indent`, in
/// `style`: every character, spaces included, each row continuing the one before (a copy
/// glues them back).
fn wrap_code(line: &str, width: usize, indent: usize, style: Style) -> Vec<MdRow> {
    let indent = indent.min(width / 2);
    let room = width.saturating_sub(indent).max(1);
    let mut out = Vec::new();
    let (mut row, mut used, mut join) = (String::new(), 0, Join::Newline);
    for (_, g) in clusters(line) {
        let w = cluster_width(g);
        if used > 0 && used + w > room {
            out.push(code_row(std::mem::take(&mut row), style, indent, join));
            join = Join::Glue;
            used = 0;
        }
        row.push_str(g);
        used += w;
    }
    out.push(code_row(row, style, indent, join));
    out
}

fn code_row(text: String, style: Style, indent: usize, join: Join) -> MdRow {
    MdRow {
        line: Line::from(vec![
            Span::raw(" ".repeat(indent)),
            Span::styled(text, style),
        ]),
        join,
        gutter: indent,
    }
}

/// `spans` wrapped to `width` cells: the first row after `first` cells of indentation, the
/// rows it wraps onto after `hang` (neither more than half the width: deep nesting still
/// leaves room for the text). Words wrap at spaces (the space is not drawn); a word wider
/// than a row is cut between its grapheme clusters.
pub(crate) fn wrap(
    spans: Vec<(String, Style)>,
    width: usize,
    first: usize,
    hang: usize,
) -> Vec<MdRow> {
    let (first, hang) = (first.min(width / 2), hang.min(width / 2));
    // The text as clusters (found in the whole text: a character whose parts came in two
    // spans stays whole), each in the style of the span it starts in; spaces separate words.
    let whole: String = spans.iter().map(|(t, _)| t.as_str()).collect();
    let mut starts: Vec<(usize, Style)> = Vec::new();
    let mut at = 0;
    for (text, style) in &spans {
        starts.push((at, *style));
        at += text.len();
    }
    // (Clusters come in order: the span they start in only moves forward.)
    let mut span = 0;
    let cells: Vec<(String, Style)> = clusters(&whole)
        .into_iter()
        .map(|(i, g)| {
            while span + 1 < starts.len() && starts[span + 1].0 <= i {
                span += 1;
            }
            let style = starts.get(span).map_or(Style::default(), |s| s.1);
            (g.to_string(), style)
        })
        .collect();
    let mut out: Vec<MdRow> = Vec::new();
    let mut row: Vec<(String, Style)> = Vec::new();
    let mut used = 0;
    let mut indent = first;
    let mut join = Join::Newline;
    let room = |indent: usize| width.saturating_sub(indent).max(1);
    let finish = |out: &mut Vec<MdRow>, row: &mut Vec<(String, Style)>, indent, join| {
        let mut line = vec![Span::raw(" ".repeat(indent))];
        let mut text = String::new();
        let mut cur: Option<Style> = None;
        for (g, st) in row.drain(..) {
            if let Some(prev) = cur.filter(|p| *p != st) {
                line.push(Span::styled(std::mem::take(&mut text), prev));
            }
            cur = Some(st);
            text.push_str(&g);
        }
        if let Some(st) = cur {
            line.push(Span::styled(text, st));
        }
        out.push(MdRow {
            line: Line::from(line),
            join,
            gutter: indent,
        });
    };
    // A space came before the next word (drawn or not: one at a row's end is not).
    let mut spaced = false;
    let mut i = 0;
    while i < cells.len() {
        if cells[i].0 == " " {
            // Spaces between words on a row are drawn, and those the line starts with (code's
            // own); a row a wrap starts drops them.
            if (!row.is_empty() || out.is_empty()) && used < room(indent) {
                row.push(cells[i].clone());
                used += 1;
            }
            spaced = true;
            i += 1;
            continue;
        }
        let end = (i..cells.len())
            .find(|&k| cells[k].0 == " ")
            .unwrap_or(cells.len());
        let w: usize = cells[i..end].iter().map(|(g, _)| cluster_width(g)).sum();
        if used > 0 && used + w > room(indent) {
            while row.last().is_some_and(|(g, _)| g == " ") {
                row.pop();
            }
            finish(&mut out, &mut row, indent, join);
            join = if spaced { Join::Space } else { Join::Glue };
            indent = hang;
            used = 0;
        }
        for cell in &cells[i..end] {
            let cw = cluster_width(&cell.0);
            if used > 0 && used + cw > room(indent) {
                finish(&mut out, &mut row, indent, join);
                join = Join::Glue;
                indent = hang;
                used = 0;
            }
            row.push(cell.clone());
            used += cw;
        }
        spaced = false;
        i = end;
    }
    if !row.is_empty() || out.is_empty() {
        while row.last().is_some_and(|(g, _)| g == " ") {
            row.pop();
        }
        finish(&mut out, &mut row, indent, join);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(r: &MdRow) -> String {
        r.line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(rows: &[MdRow]) -> Vec<String> {
        rows.iter().map(text).collect()
    }

    fn styled<'a>(rows: &'a [MdRow], piece: &str) -> Option<&'a Span<'static>> {
        rows.iter()
            .flat_map(|r| r.line.spans.iter())
            .find(|s| s.content == piece)
    }

    #[test]
    fn inline_markers_become_styles() {
        let r = rows("Use **bold**, `code` and *it* here", 80, 2);
        assert_eq!(texts(&r), ["  Use bold, code and it here"]);
        let bold = styled(&r, "bold").unwrap();
        assert!(bold.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(styled(&r, "code").unwrap().style.fg, Some(CODE));
        assert!(styled(&r, "it")
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::ITALIC));
        // Unclosed markers, snake_case and lone stars stay as typed.
        let r = rows("a **half and `tick and snake_case_name and 2 * 3", 80, 0);
        assert_eq!(
            texts(&r),
            ["a **half and `tick and snake_case_name and 2 * 3"]
        );
        // Bold with code inside; an escaped star.
        let r = rows("**run `ls`** \\*not\\*", 80, 0);
        assert_eq!(texts(&r), ["run ls *not*"]);
        assert!(styled(&r, "ls")
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        // An escaped marker inside emphasis is text, not its end.
        let r = rows("*literal \\* star* and **bold**", 80, 0);
        assert_eq!(texts(&r), ["literal * star and bold"]);
        // A link: its text, its target dim.
        let r = rows("see [docs](https://x.y/z)", 80, 0);
        assert_eq!(texts(&r), ["see docs (https://x.y/z)"]);
    }

    #[test]
    fn blocks_are_drawn_like_claude_code() {
        let md = "## Title\n\nPara one.\n\n\n- item one\n- a long item that has to wrap onto the next row\n1. first\n\n```rust\nfn main() {\n    x();\n}\n```\n> quoted\n---";
        let r = rows(md, 24, 2);
        assert_eq!(
            texts(&r),
            [
                "  Title",
                "",
                "  Para one.",
                "",
                "  - item one",
                "  - a long item that has",
                "    to wrap onto the",
                "    next row",
                "",
                "  1. first",
                "",
                "  fn main() {",
                "      x();",
                "  }",
                "",
                "  │ quoted",
                "",
                "  ──────────────────────",
            ]
        );
        // The hanging rows of an item continue it, their indent left out of a copy.
        assert_eq!((r[6].join, r[6].gutter), (Join::Space, 4));
        assert_eq!(r[5].gutter, 2);
        assert!(styled(&r, "Title")
            .unwrap()
            .style
            .add_modifier
            .contains(Modifier::BOLD));
        assert_eq!(styled(&r, "fn main() {").unwrap().style.fg, Some(CODE));
    }

    #[test]
    fn long_words_split_and_streaming_text_reads_as_typed() {
        let r = rows("abcdefghij", 6, 2);
        assert_eq!(texts(&r), ["  abcd", "  efgh", "  ij"]);
        assert_eq!(r[1].join, Join::Glue);
        // An unfinished fence: the code so far, as code.
        let r = rows("before\n```\nlet x", 40, 0);
        assert_eq!(texts(&r), ["before", "", "let x"]);
        // A code span closes on a run of its own length only; a line of unmatched markers is
        // read in linear time.
        let r = rows("`a``b` and ``c`d``", 40, 0);
        assert_eq!(texts(&r), ["a``b and c`d"]);
        let many = format!("{}](x)", "*a [b `c ".repeat(10_000));
        let start = std::time::Instant::now();
        let r = rows(&many, 200, 0);
        assert!(!r.is_empty());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        // An item that opens with code, a rule, a nested list or a quote: its marker first.
        assert_eq!(
            texts(&rows("- ```\n  code\n  ```\n- b", 24, 0)),
            ["- code", "- b"]
        );
        assert_eq!(texts(&rows("- ***\n- b", 12, 0))[0], "- ──────────");
        assert_eq!(
            texts(&rows("- - a\n  - b\n- c", 24, 0)),
            ["- - a", "  - b", "- c"]
        );
        let r = rows("- > quoted text that wraps a bit more\n- b", 24, 0);
        assert_eq!(
            texts(&r),
            ["- │ quoted text that", "  │ wraps a bit more", "- b"]
        );
        // Copies keep the marker, not the bars or indentation.
        assert_eq!((r[0].gutter, r[1].gutter), (0, 4));
        // Markers as typed.
        assert_eq!(
            texts(&rows("* a\n* b\n\n7) x\n8) y\n10) z", 24, 0)),
            ["* a", "* b", "", "7) x", "8) y", "10) z"]
        );
        // Deep quotes stay within the width: the innermost bars are kept.
        for w in 4..30 {
            for r in rows("> > > > > > > > > > > x y z", w, 2) {
                assert!(r.line.width() <= w, "{w}: {:?}", r.line.to_string());
            }
        }
        // A blank row inside a quote keeps its bar; tabs are four spaces; an HTML block keeps
        // its spacing.
        assert_eq!(texts(&rows("> a\n>\n> b", 24, 0)), ["│ a", "│ ", "│ b"]);
        assert_eq!(
            texts(&rows("```\nf() {\n\treturn\n}\n```\nword\tword", 40, 0)),
            ["f() {", "    return", "}", "", "word    word"]
        );
        assert_eq!(
            texts(&rows("<div>\n  <p>x</p>\n\n</div>", 40, 0)),
            ["<div>", "  <p>x</p>", "", "</div>"]
        );
        // Another paragraph of the same item is set apart.
        assert_eq!(
            texts(&rows("- a\n\n  second para", 24, 0)),
            ["- a", "", "  second para"]
        );
        // Inline code keeps its leading spaces at the start of a line.
        assert_eq!(
            texts(&rows("`  return x` here", 40, 0)),
            ["  return x here"]
        );
        // A character made of parts in two spans (an HTML entity, an accent in bold) stays
        // whole.
        let r = rows("👩&#x200d;💻 x", 6, 0);
        assert_eq!(texts(&r), ["👩\u{200d}💻 x"]);
        // An image inside a link keeps both targets.
        let r = rows("[![logo](a.png)](https://x.y)", 60, 0);
        assert_eq!(texts(&r), ["logo (a.png) (https://x.y)"]);
        // Deep nesting stays linear, nested images too.
        let images = format!("{}x{}", "[![".repeat(2_000), "](u)](v)".repeat(2_000));
        let start = std::time::Instant::now();
        assert!(!rows(&images, 80, 2).is_empty());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let deep = format!("{}x", "> ".repeat(20_000));
        let start = std::time::Instant::now();
        assert!(!rows(&deep, 80, 2).is_empty());
        let deep = format!("{}x", "- ".repeat(5_000));
        assert!(!rows(&deep, 80, 2).is_empty());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        // A loose list keeps each marker with its item.
        let r = rows("- first\n\n- second\n\n1. one\n\n2. two", 40, 0);
        assert_eq!(texts(&r), ["- first", "- second", "", "1. one", "2. two"]);
        // A longer closing fence closes it too.
        for close in ["````", "~~~~"] {
            let open = &close[..3];
            let r = rows(&format!("{open}\ncode\n{close}\nprose"), 40, 0);
            assert_eq!(texts(&r), ["code", "", "prose"]);
            assert_eq!(styled(&r, "prose").map(|s| s.style.fg), Some(None));
        }
        // Inline code made of backtick runs is not a fence.
        let r = rows("```hello``` world", 40, 0);
        assert_eq!(texts(&r), ["hello world"]);
        // Code keeps its spaces when cut; deep indentation leaves room for the text.
        let r = rows("```\na     bcdef\n```", 8, 0);
        assert_eq!(texts(&r), ["a     bc", "def"]);
        assert_eq!(r[1].join, Join::Glue);
        let r = rows("        hidden", 8, 0);
        assert!(r.iter().all(|r| r.line.width() <= 8), "{:?}", texts(&r));
        assert!(texts(&r).concat().contains("hid"));
        // Wide characters take two cells.
        let r = rows("日本語 日本語", 8, 0);
        assert_eq!(texts(&r), ["日本語", "日本語"]);
    }
}
