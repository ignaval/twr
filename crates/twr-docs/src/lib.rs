//! twr's own documentation, compiled into the binary, for the orchestrator's `read_docs` tool.
//!
//! The bundled pages are the repository's `README.md`, `SKILL.md` and every `docs/*.md`
//! ([`BUNDLED`]), named by their path in the repository. A caller can add pages of its own
//! ([`Library::with_pages`]): `twr mcp` adds the `--help` of every subcommand as `cli`.
//!
//! Each page is split into sections at its `#`, `##` and `###` headings (never inside a fenced
//! code block). A section's id is `<page>#<anchor>`, the anchor made the way GitHub makes
//! heading anchors (`docs/workspaces.md#whats-separate-and-whats-shared`); the text before a
//! page's first `##` is the page's own section, whose id is the page name, and reading it
//! returns the whole page. Search ranks sections with BM25 over lowercase word tokens, heading
//! words counting [`HEADING_WEIGHT`] times; `SKILL.md`, the socket API reference, weighs a
//! little less than the user docs ([`SKILL_WEIGHT`]). Nothing here does I/O.

use std::collections::{HashMap, HashSet};

/// The pages compiled in: `(name, text)`, the name being the file's path in the repository.
pub const BUNDLED: &[(&str, &str)] = &[
    ("README.md", include_str!("../../../README.md")),
    ("SKILL.md", include_str!("../../../SKILL.md")),
    ("docs/README.md", include_str!("../../../docs/README.md")),
    ("docs/install.md", include_str!("../../../docs/install.md")),
    ("docs/usage.md", include_str!("../../../docs/usage.md")),
    ("docs/tui.md", include_str!("../../../docs/tui.md")),
    (
        "docs/configuration.md",
        include_str!("../../../docs/configuration.md"),
    ),
    (
        "docs/notifications.md",
        include_str!("../../../docs/notifications.md"),
    ),
    (
        "docs/runtimes.md",
        include_str!("../../../docs/runtimes.md"),
    ),
    (
        "docs/accounts.md",
        include_str!("../../../docs/accounts.md"),
    ),
    (
        "docs/orchestrator.md",
        include_str!("../../../docs/orchestrator.md"),
    ),
    (
        "docs/worktrees.md",
        include_str!("../../../docs/worktrees.md"),
    ),
    (
        "docs/workspaces.md",
        include_str!("../../../docs/workspaces.md"),
    ),
    ("docs/runners.md", include_str!("../../../docs/runners.md")),
    (
        "docs/architecture.md",
        include_str!("../../../docs/architecture.md"),
    ),
    ("docs/recipes.md", include_str!("../../../docs/recipes.md")),
    (
        "docs/security-model.md",
        include_str!("../../../docs/security-model.md"),
    ),
    (
        "docs/development.md",
        include_str!("../../../docs/development.md"),
    ),
];

/// Most bytes one [`Library::read`] returns.
pub const PAGE_BYTES: usize = 16 * 1024;

/// Sections [`Library::search`] returns by default.
pub const SEARCH_LIMIT: usize = 5;

/// Longest excerpt of a search hit, in bytes.
pub const EXCERPT_MAX: usize = 300;

/// How many times a heading's words count.
pub const HEADING_WEIGHT: f64 = 3.0;

/// The score factor of `SKILL.md`'s sections.
pub const SKILL_WEIGHT: f64 = 0.9;

/// Longest query read, in bytes (the rest is ignored).
const QUERY_MAX: usize = 1000;

/// Longest section id looked up, in bytes (every real one is far shorter).
const ID_MAX: usize = 256;

const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Words too common to rank by.
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "can", "do", "does", "for", "from", "how",
    "i", "if", "in", "into", "is", "it", "its", "me", "my", "no", "not", "of", "on", "or", "our",
    "so", "that", "the", "then", "there", "this", "to", "we", "what", "when", "where", "which",
    "who", "why", "with", "you", "your", "use", "using",
];

/// One page.
#[derive(Debug)]
struct Page {
    name: String,
    title: String,
    text: String,
}

/// One section of a page: the bytes `start..end` of its text.
#[derive(Debug)]
struct Section {
    id: String,
    page: usize,
    /// The page title and the headings down to this one, joined with ` > `.
    heading: String,
    start: usize,
    end: usize,
    /// Term frequencies, heading words weighted.
    terms: HashMap<String, f64>,
    len: f64,
    weight: f64,
}

/// A search result.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The section id, for [`Library::read`].
    pub id: String,
    /// Its heading path.
    pub heading: String,
    /// The section's size in bytes.
    pub bytes: usize,
    /// The line that matched best, cut to [`EXCERPT_MAX`].
    pub excerpt: String,
    /// Where that line starts in the section (a [`Library::read`] offset).
    pub offset: usize,
    /// The score.
    pub score: f64,
}

/// What [`Library::read`] returns: `text` is bytes `offset..end` of the section's `total`.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    /// The section id.
    pub id: String,
    /// Its heading path.
    pub heading: String,
    /// Where `text` starts.
    pub offset: usize,
    /// Where `text` ends (the offset to read next when below `total`).
    pub end: usize,
    /// The section's size.
    pub total: usize,
    /// The text.
    pub text: String,
}

/// The pages and their sections, ready to search.
#[derive(Debug)]
pub struct Library {
    pages: Vec<Page>,
    sections: Vec<Section>,
    df: HashMap<String, usize>,
    avg_len: f64,
}

impl Library {
    /// The bundled pages ([`BUNDLED`]).
    pub fn bundled() -> Library {
        Library::with_pages(Vec::new())
    }

    /// The bundled pages plus `extra` (`(name, markdown)`), after them. A page whose name is
    /// taken already is skipped.
    pub fn with_pages(extra: Vec<(String, String)>) -> Library {
        let mut pages: Vec<(String, String)> = BUNDLED
            .iter()
            .map(|(n, t)| (n.to_string(), t.to_string()))
            .collect();
        for (name, text) in extra {
            if !pages.iter().any(|(n, _)| *n == name) {
                pages.push((name, text));
            }
        }
        Library::new(pages)
    }

    fn new(pages: Vec<(String, String)>) -> Library {
        let mut lib = Library {
            pages: Vec::new(),
            sections: Vec::new(),
            df: HashMap::new(),
            avg_len: 1.0,
        };
        for (name, text) in pages {
            lib.add_page(name, text);
        }
        for s in &lib.sections {
            for t in s.terms.keys() {
                *lib.df.entry(t.clone()).or_default() += 1;
            }
        }
        let total: f64 = lib.sections.iter().map(|s| s.len).sum();
        lib.avg_len = (total / lib.sections.len().max(1) as f64).max(1.0);
        lib
    }

    fn add_page(&mut self, name: String, text: String) {
        let page = self.pages.len();
        let weight = if name == "SKILL.md" {
            SKILL_WEIGHT
        } else {
            1.0
        };
        let heads = headings(&text);
        // The first `#` heading is the title; any other heading starts a section (a later `#`
        // counts as `##`), the page itself being the section at 0.
        let title_at = heads.iter().find(|h| h.level == 1).map(|h| h.start);
        let title = heads
            .iter()
            .find(|h| Some(h.start) == title_at)
            .map_or_else(|| name.clone(), |h| h.text.clone());
        let mut starts: Vec<(usize, String, String)> = vec![(0, name.clone(), title.clone())];
        let mut used: HashMap<String, usize> = HashMap::new();
        let mut h2: Option<String> = None;
        for h in heads.iter().filter(|h| Some(h.start) != title_at) {
            let path = match h.level {
                3 => {
                    let mut p = vec![title.clone()];
                    p.extend(h2.clone());
                    p.push(h.text.clone());
                    p.join(" > ")
                }
                _ => {
                    h2 = Some(h.text.clone());
                    format!("{title} > {}", h.text)
                }
            };
            let base = anchor(&h.text);
            let n = used.entry(base.clone()).or_default();
            let slug = if *n == 0 {
                base.clone()
            } else {
                format!("{base}-{n}")
            };
            *n += 1;
            starts.push((h.start, format!("{name}#{slug}"), path));
        }
        for (i, (start, id, heading)) in starts.iter().enumerate() {
            let end = starts.get(i + 1).map_or(text.len(), |s| s.0);
            let body = &text[*start..end];
            let mut terms: HashMap<String, f64> = HashMap::new();
            let mut len = 0.0;
            for t in tokens(body) {
                *terms.entry(t).or_default() += 1.0;
                len += 1.0;
            }
            let own = heading.rsplit(" > ").next().unwrap_or(heading);
            for t in tokens(own) {
                *terms.entry(t).or_default() += HEADING_WEIGHT;
            }
            self.sections.push(Section {
                id: id.clone(),
                page,
                heading: heading.clone(),
                start: *start,
                end,
                terms,
                len,
                weight,
            });
        }
        self.pages.push(Page { name, title, text });
    }

    /// Every section id, in page order.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.sections.iter().map(|s| s.id.as_str())
    }

    /// Every page name, in order.
    pub fn page_names(&self) -> impl Iterator<Item = &str> {
        self.pages.iter().map(|p| p.name.as_str())
    }

    /// The best `limit` sections for `query`, best first; none when no word of it is known.
    pub fn search(&self, query: &str, limit: usize) -> Vec<Hit> {
        let query = cut(query, QUERY_MAX);
        let mut words: Vec<String> = Vec::new();
        for t in tokens(query) {
            if !words.contains(&t) {
                words.push(t);
            }
        }
        let n = self.sections.len() as f64;
        let mut scored: Vec<(f64, usize)> = Vec::new();
        for (i, s) in self.sections.iter().enumerate() {
            let mut score = 0.0;
            for w in &words {
                let Some(tf) = s.terms.get(w) else { continue };
                let df = *self.df.get(w).unwrap_or(&0) as f64;
                let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
                score += idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * s.len / self.avg_len));
            }
            if score > 0.0 {
                scored.push((score * s.weight, i));
            }
        }
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(score, i)| {
                let s = &self.sections[i];
                let text = self.section_text(s);
                let (offset, excerpt) = best_line(text, &words);
                Hit {
                    id: s.id.clone(),
                    heading: s.heading.clone(),
                    bytes: text.len(),
                    excerpt,
                    offset,
                    score,
                }
            })
            .collect()
    }

    fn section_text(&self, s: &Section) -> &str {
        &self.pages[s.page].text[s.start..s.end]
    }

    /// The section `id` (or, for a page name, the whole page), from byte `offset`: at most
    /// [`PAGE_BYTES`], ending at a line end when one is in the second half. An id without
    /// `docs/` that names a `docs/` page works too (the docs link to each other that way).
    /// `Err` names the closest ids.
    pub fn read(&self, id: &str, offset: usize) -> Result<Chunk, String> {
        if id.len() > ID_MAX {
            return Err(format!(
                "no section has an id that long ({} bytes; ids are at most {ID_MAX})",
                id.len()
            ));
        }
        let (id, text, heading) = self.lookup(id).ok_or_else(|| {
            let near = self.closest(id);
            if near.is_empty() {
                format!("no section {id:?}")
            } else {
                format!("no section {id:?}; closest: {}", near.join(", "))
            }
        })?;
        if offset > text.len() {
            return Err(format!(
                "offset {offset} is past the end of {id} ({} bytes)",
                text.len()
            ));
        }
        let mut start = offset;
        while !text.is_char_boundary(start) {
            start += 1;
        }
        let mut end = (start + PAGE_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len() {
            if let Some(nl) = text[start..end].rfind('\n') {
                if nl + 1 > PAGE_BYTES / 2 {
                    end = start + nl + 1;
                }
            }
        }
        Ok(Chunk {
            id: id.to_string(),
            heading: heading.to_string(),
            offset: start,
            end,
            total: text.len(),
            text: text[start..end].to_string(),
        })
    }

    /// `(id, text, heading)` of `id`: a section, or a whole page by its name.
    fn lookup(&self, id: &str) -> Option<(&str, &str, &str)> {
        let id = id.trim();
        let find = |id: &str| -> Option<(&str, &str, &str)> {
            if let Some(p) = self.pages.iter().find(|p| p.name == id) {
                return Some((&p.name, &p.text, &p.title));
            }
            self.sections
                .iter()
                .find(|s| s.id == id)
                .map(|s| (s.id.as_str(), self.section_text(s), s.heading.as_str()))
        };
        find(id).or_else(|| find(&format!("docs/{id}")))
    }

    /// Up to five ids that look like `id`: the most shared words, then the shortest.
    fn closest(&self, id: &str) -> Vec<&str> {
        let want: HashSet<String> = tokens(id).collect();
        let mut scored: Vec<(usize, &str)> = self
            .ids()
            .map(|cand| {
                let have: HashSet<String> = tokens(cand).collect();
                (want.intersection(&have).count(), cand)
            })
            .filter(|(n, _)| *n > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.len().cmp(&b.1.len())));
        scored.into_iter().take(5).map(|(_, id)| id).collect()
    }

    /// The index: each page with its title, size and section anchors.
    pub fn index(&self) -> String {
        let mut out = String::new();
        for (i, p) in self.pages.iter().enumerate() {
            let anchors: Vec<&str> = self
                .sections
                .iter()
                .filter(|s| s.page == i && s.id != p.name)
                .filter_map(|s| s.id.split_once('#').map(|(_, a)| a))
                .collect();
            out.push_str(&format!("{} ({}, {})", p.name, p.title, kb(p.text.len())));
            if !anchors.is_empty() {
                out.push_str(&format!(": #{}", anchors.join(" #")));
            }
            out.push('\n');
        }
        out
    }

    /// `read_docs`'s answer for `query`: the hits, each with its id, heading, size and excerpt.
    pub fn render_search(&self, query: &str, limit: usize) -> String {
        let hits = self.search(query, limit);
        if hits.is_empty() {
            return format!(
                "nothing in twr's docs matches {:?}; try other words, or read_docs without \
                 arguments for the index",
                cut(query, 200)
            );
        }
        let mut out = format!(
            "{} sections for {:?}, best first; read one with read_docs section=<id> (offset=<n> \
             starts at the matching line):\n",
            hits.len(),
            cut(query, 200)
        );
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "{}. {} ({}, {})\n   offset={}: {}\n",
                i + 1,
                h.id,
                h.heading,
                kb(h.bytes),
                h.offset,
                h.excerpt
            ));
        }
        out
    }

    /// `read_docs`'s answer for `section` from `offset`: a header line, the text and, when
    /// more follows, how to read it.
    pub fn render_read(&self, id: &str, offset: usize) -> Result<String, String> {
        let c = self.read(id, offset)?;
        let mut out = format!(
            "{} ({}), bytes {}-{} of {}:\n\n{}",
            c.id, c.heading, c.offset, c.end, c.total, c.text
        );
        if !out.ends_with('\n') {
            out.push('\n');
        }
        if c.end < c.total {
            out.push_str(&format!(
                "\n[more: read_docs section={} offset={}]\n",
                c.id, c.end
            ));
        }
        Ok(out)
    }
}

/// A heading line of a page.
#[derive(Debug)]
struct Heading {
    level: usize,
    text: String,
    start: usize,
}

/// The `#`, `##` and `###` headings of `text` outside fenced code blocks.
fn headings(text: &str) -> Vec<Heading> {
    let mut out = Vec::new();
    let mut fence: Option<(char, usize)> = None;
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let start = pos;
        pos += line.len();
        let t = line.trim_end_matches(['\n', '\r']);
        let lead = t.len() - t.trim_start().len();
        let body = t.trim_start();
        if lead <= 3 {
            if let Some(c) = ['`', '~'].into_iter().find(|c| body.starts_with(*c)) {
                let n = body.chars().take_while(|x| *x == c).count();
                if n >= 3 {
                    match fence {
                        None => fence = Some((c, n)),
                        Some((fc, fnum)) if fc == c && n >= fnum && body[n..].trim().is_empty() => {
                            fence = None
                        }
                        _ => {}
                    }
                    continue;
                }
            }
        }
        if fence.is_some() || lead > 0 {
            continue;
        }
        let level = t.chars().take_while(|c| *c == '#').count();
        if !(1..=3).contains(&level) || !t[level..].starts_with(' ') {
            continue;
        }
        let text = t[level..].trim().trim_end_matches('#').trim().to_string();
        if !text.is_empty() {
            out.push(Heading { level, text, start });
        }
    }
    out
}

/// GitHub's anchor for heading `text`: lowercase, spaces to `-`, only letters, digits, `-`
/// and `_` kept.
pub fn anchor(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
            _ => None,
        })
        .collect()
}

/// The search words of `text`: lowercase alphanumeric runs of two or more characters, not
/// stopwords, with a plural `s` / `ies` taken off.
fn tokens(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .map(|w| stem(&w.to_lowercase()))
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
}

fn stem(w: &str) -> String {
    let n = w.chars().count();
    if n > 4 && w.ends_with("ies") {
        format!("{}y", &w[..w.len() - 3])
    } else if n > 3 && w.ends_with('s') && !w.ends_with("ss") && !w.ends_with("us") {
        w[..w.len() - 1].to_string()
    } else {
        w.to_string()
    }
}

/// The line of `text` with the most distinct `words` (the first on a tie) as `(its offset,
/// its text cut to EXCERPT_MAX with whitespace collapsed)`.
fn best_line(text: &str, words: &[String]) -> (usize, String) {
    let mut best = (0usize, 0usize, "");
    let mut pos = 0;
    for line in text.split_inclusive('\n') {
        let have: HashSet<String> = tokens(line).collect();
        let n = words.iter().filter(|w| have.contains(*w)).count();
        if n > best.0 {
            best = (n, pos, line);
        }
        pos += line.len();
    }
    if best.0 == 0 {
        best.2 = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        best.1 = 0;
    }
    let flat = best.2.split_whitespace().collect::<Vec<_>>().join(" ");
    let excerpt = if flat.len() > EXCERPT_MAX {
        format!("{}...", cut(&flat, EXCERPT_MAX))
    } else {
        flat
    };
    (best.1, excerpt)
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

fn kb(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else {
        format!("{:.1} KB", n as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn top_ids(lib: &Library, q: &str, n: usize) -> Vec<String> {
        lib.search(q, n).into_iter().map(|h| h.id).collect()
    }

    #[test]
    fn every_doc_page_is_bundled() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut on_disk: Vec<String> = std::fs::read_dir(root.join("docs"))
            .unwrap()
            .filter_map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.ends_with(".md").then(|| format!("docs/{name}"))
            })
            .collect();
        on_disk.extend(["README.md".to_string(), "SKILL.md".to_string()]);
        on_disk.sort();
        let mut bundled: Vec<String> = BUNDLED.iter().map(|(n, _)| n.to_string()).collect();
        bundled.sort();
        assert_eq!(bundled, on_disk, "add new pages to BUNDLED");
    }

    #[test]
    fn the_motivating_question_finds_workspaces() {
        let lib = Library::bundled();
        let q = "how can I run another twr instance with a fresh conversation and fresh list \
                 of threads?";
        let top = top_ids(&lib, q, 3);
        assert!(
            top.iter().any(|id| id.starts_with("docs/workspaces.md")
                || id == "docs/configuration.md#a-second-instance"),
            "{top:?}"
        );
    }

    #[test]
    fn golden_queries_find_their_pages() {
        let lib = Library::bundled();
        for (q, want) in [
            ("restore a deleted thread from the trash", "trash"),
            (
                "switch the orchestrator to codex",
                "docs/orchestrator.md#switching",
            ),
            ("what does parking a thread remove", "park"),
            ("separate hubs on one machine", "docs/workspaces.md"),
            (
                "connect a runner on another machine with a token",
                "docs/runners.md",
            ),
            ("ssh box daemon", "docs/runners.md"),
            ("socket api methods", "SKILL.md"),
        ] {
            let top = top_ids(&lib, q, 3);
            assert!(top.iter().any(|id| id.contains(want)), "{q}: {top:?}");
        }
    }

    #[test]
    fn section_ids_are_unique_and_anchors_follow_github() {
        let lib = Library::bundled();
        let mut seen = HashSet::new();
        for id in lib.ids() {
            assert!(seen.insert(id.to_string()), "duplicate id {id}");
        }
        assert!(seen.contains("docs/workspaces.md#whats-separate-and-whats-shared"));
        assert!(seen.contains("docs/configuration.md#a-second-instance"));
        assert_eq!(anchor("`twr ws` and -w"), "twr-ws-and--w");
    }

    #[test]
    fn headings_in_code_fences_are_not_sections() {
        let lib = Library::new(vec![(
            "p.md".into(),
            "# Title\n\nintro\n\n```toml\n# not a heading\n## nor this\n```\n\n## Real\n\nbody \
             ```inline``` text\n~~~\n### hidden\n~~~\n### Sub\n\nmore\n"
                .into(),
        )]);
        let ids: Vec<&str> = lib.ids().collect();
        assert_eq!(ids, vec!["p.md", "p.md#real", "p.md#sub"]);
        let sub = lib.read("p.md#sub", 0).unwrap();
        assert_eq!(sub.heading, "Title > Real > Sub");
        assert!(lib.read("p.md#real", 0).unwrap().text.contains("hidden"));
        // The page id reads the whole page.
        assert!(lib.read("p.md", 0).unwrap().text.contains("not a heading"));
    }

    #[test]
    fn duplicate_headings_get_numbered_anchors() {
        let lib = Library::new(vec![(
            "p.md".into(),
            "# T\n## Keys\na\n## Keys\nb\n### Keys\nc\n".into(),
        )]);
        let ids: Vec<&str> = lib.ids().collect();
        assert_eq!(ids, vec!["p.md", "p.md#keys", "p.md#keys-1", "p.md#keys-2"]);
    }

    #[test]
    fn reading_pages_through_a_long_section() {
        let lib = Library::bundled();
        let total = lib.read("SKILL.md", 0).unwrap().total;
        assert!(total > PAGE_BYTES * 2);
        let mut offset = 0;
        let mut whole = String::new();
        loop {
            let c = lib.read("SKILL.md", offset).unwrap();
            assert!(c.text.len() <= PAGE_BYTES);
            whole.push_str(&c.text);
            if c.end == c.total {
                break;
            }
            assert!(c.text.ends_with('\n'), "pages end at line ends");
            offset = c.end;
        }
        assert_eq!(whole, BUNDLED[1].1);
        assert!(lib.read("SKILL.md", total + 1).is_err());
        // An offset inside a character moves to the next boundary.
        let lib = Library::new(vec![("p.md".into(), "# é\n".into())]);
        assert_eq!(lib.read("p.md", 3).unwrap().offset, 4);
    }

    #[test]
    fn unknown_ids_name_close_ones_and_docs_prefix_is_optional() {
        let lib = Library::bundled();
        let e = lib.read("docs/workspaces.md#in-the-tuii", 0).unwrap_err();
        assert!(e.contains("docs/workspaces.md"), "{e}");
        let c = lib.read("workspaces.md#in-the-tui", 0).unwrap();
        assert_eq!(c.id, "docs/workspaces.md#in-the-tui");
        // A huge unknown id is refused in a few words, not echoed.
        let e = lib.read(&"x".repeat(100_000), 0).unwrap_err();
        assert!(e.len() < 200, "{}", e.len());
    }

    #[test]
    fn hits_point_at_the_matching_line() {
        let lib = Library::bundled();
        let hit = &lib.search("TWR_HOME second instance", 1)[0];
        let c = lib.read(&hit.id, hit.offset).unwrap();
        assert!(
            c.text.lines().next().unwrap().contains("instance") || hit.excerpt.contains("TWR_HOME"),
            "{hit:?}"
        );
        assert!(hit.excerpt.len() <= EXCERPT_MAX + 3);
    }

    #[test]
    fn rendered_answers_stay_small() {
        let lib = Library::bundled();
        assert!(lib.index().len() < 12 * 1024, "{}", lib.index().len());
        let s = lib.render_search("orchestrator memory hook guard", SEARCH_LIMIT);
        assert!(s.len() < 4 * 1024, "{s}");
        assert!(lib
            .render_search("zzzqqq", SEARCH_LIMIT)
            .contains("nothing"));
        let r = lib.render_read("SKILL.md", 0).unwrap();
        assert!(
            r.len() < PAGE_BYTES + 512 && r.contains("[more: read_docs section=SKILL.md offset=")
        );
    }

    #[test]
    fn extra_pages_are_searchable() {
        let lib = Library::with_pages(vec![(
            "cli".into(),
            "# twr command line\n\n## twr thread new\n\n```\n--runtime <RUNTIME>\n```\n".into(),
        )]);
        let top = top_ids(&lib, "twr thread new --runtime", 3);
        assert!(top.contains(&"cli#twr-thread-new".to_string()), "{top:?}");
    }
}
