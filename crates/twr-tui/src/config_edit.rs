//! Edits one key of the user's twr config file (`config.toml`) for the TUI's settings view.
//!
//! That file is written by hand: it has comments, its own layout, and may hold secrets (box
//! tokens, environment tables). Parsing it and serializing it back would lose all of that, so
//! this never does: it edits the text. [`set_key`] changes the one line that holds the key
//! (the value only: indentation, spacing around `=` and a trailing comment stay), inserts one
//! line after the table's header, or appends a new table at the end; every other byte stays
//! as it was. Then it checks its own work: the new text must parse to exactly the old document
//! with that one key set, and still load as a twr config. Anything it cannot place that way
//! (an inline table, dotted keys, a value over several lines, a file that does not parse or
//! that twr itself would refuse) is refused with a short reason the TUI shows, so the user
//! edits it by hand instead.
//!
//! [`write_key`] writes the result into the file itself ([`rewrite`]): it stays the same
//! file, so who may read it (owner, group, mode, ACLs) and its links stay as they were, and a
//! symbolic link's target is what is edited. The file is locked from the read to the write, so
//! two TUIs' edits never lose one another, and it is left alone when an editor changed it
//! meanwhile.

use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use twr_core::config::Config;

/// A value the settings view writes: only scalars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scalar {
    Bool(bool),
    Int(i64),
    Str(String),
}

impl Scalar {
    /// The value as the parsed document holds it.
    fn value(&self) -> toml::Value {
        match self {
            Scalar::Bool(b) => toml::Value::Boolean(*b),
            Scalar::Int(i) => toml::Value::Integer(*i),
            Scalar::Str(s) => toml::Value::String(s.clone()),
        }
    }

    /// The value as TOML text, always on one line (a string is a basic string, escaped).
    fn text(&self) -> String {
        match self {
            Scalar::Bool(b) => b.to_string(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Str(s) => basic_string(s),
        }
    }
}

/// `s` as a TOML basic string on one line: quotes, backslashes and control characters escaped.
fn basic_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `key` as TOML writes it: bare when it can be, else a quoted basic string.
fn key_text(key: &str) -> String {
    if is_bare(key) {
        key.to_string()
    } else {
        basic_string(key)
    }
}

/// Whether `key` may be written without quotes.
fn is_bare(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// `table.key` with its parts joined by dots, for messages.
fn dotted(table: &[&str], key: Option<&str>) -> String {
    let mut parts: Vec<&str> = table.to_vec();
    parts.extend(key);
    parts.join(".")
}

/// What the parsed document has at a table path.
enum Found<'a> {
    /// No such table (nor anything else on its path).
    Missing,
    /// Something on the path is a value, or an array of tables, not a table.
    NotTable(String),
    /// The table.
    Table(&'a toml::Table),
}

/// The table at `path` in `doc`.
fn find_table<'a>(doc: &'a toml::Table, path: &[&str]) -> Found<'a> {
    let mut cur = doc;
    for (i, part) in path.iter().enumerate() {
        match cur.get(*part) {
            None => return Found::Missing,
            Some(toml::Value::Table(t)) => cur = t,
            Some(_) => return Found::NotTable(path[..=i].join(".")),
        }
    }
    Found::Table(cur)
}

/// A line of the text: where it starts and ends (its line ending excluded), and where the
/// next one starts.
#[derive(Debug, Clone, Copy)]
struct Line {
    start: usize,
    end: usize,
    next: usize,
}

/// The lines of `text`.
fn lines(text: &str) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0;
    for piece in text.split_inclusive('\n') {
        let next = start + piece.len();
        let body = piece
            .strip_suffix('\n')
            .map(|p| p.strip_suffix('\r').unwrap_or(p))
            .unwrap_or(piece);
        out.push(Line {
            start,
            end: start + body.len(),
            next,
        });
        start = next;
    }
    out
}

/// A table header line: the path it names, and whether it is an array of tables (`[[a.b]]`).
struct Header {
    path: Vec<String>,
    array: bool,
}

/// The header `line` is, if it is one: the line parsed alone, as a document holding only that
/// header. (A line inside a multi-line string or array can look like one; what it would
/// change is caught by [`set_key`]'s check of the result.)
fn header(line: &str) -> Option<Header> {
    let t = line.trim_start();
    if !t.starts_with('[') {
        return None;
    }
    let parsed: toml::Table = t.parse().ok()?;
    let mut path = Vec::new();
    let mut cur = &parsed;
    loop {
        if cur.is_empty() {
            return Some(Header { path, array: false });
        }
        let mut entries = cur.iter();
        let (k, v) = entries.next()?;
        if entries.next().is_some() {
            return None;
        }
        path.push(k.clone());
        match v {
            toml::Value::Table(t) => cur = t,
            toml::Value::Array(_) => return Some(Header { path, array: true }),
            _ => return None,
        }
    }
}

/// Where the value of `key` starts in `line`, when the line begins (after its indentation)
/// with `key` itself, bare or quoted, then `=`: a dotted key such as `key.sub =` is not it.
fn value_start(line: &str, key: &str) -> Option<usize> {
    let rest = line.trim_start_matches([' ', '\t']);
    let mut spellings = vec![basic_string(key), format!("'{key}'")];
    if is_bare(key) {
        spellings.push(key.to_string());
    }
    spellings.iter().find_map(|spelling| {
        let after = rest.strip_prefix(spelling.as_str())?;
        let v = after.trim_start_matches([' ', '\t']).strip_prefix('=')?;
        let v = v.trim_start_matches([' ', '\t']);
        Some(line.len() - v.len())
    })
}

/// Where the value that starts at `from` in `line` ends: the first `#` (or the line's end)
/// before which `k = <what precedes it>` parses to `old`. A `#` inside a string does not end
/// it, as the text before it does not parse.
fn value_end(line: &str, from: usize, old: &toml::Value) -> Option<usize> {
    let rest = &line[from..];
    let cuts = rest
        .match_indices('#')
        .map(|(i, _)| i)
        .chain(std::iter::once(rest.len()));
    for cut in cuts {
        let value = rest[..cut].trim_end_matches([' ', '\t']);
        let probe: Result<toml::Table, _> = format!("k = {value}").parse();
        if probe.ok().and_then(|t| t.get("k").cloned()).as_ref() == Some(old) {
            return Some(from + value.len());
        }
    }
    None
}

/// `text` with `table.key` set to `value` (table is a dotted header path like `["tui"]` or
/// `["tui", "notify"]`), or why it cannot be done safely.
///
/// A key already there gets its value replaced on its own line; a key missing from a table
/// whose header is in the file is added on the line after the header; a table the file does
/// not have is appended at the end. The result is checked: it must parse to the old document
/// with only `table.key` changed, and load as a twr config (as the old text must already).
pub fn set_key(text: &str, table: &[&str], key: &str, value: &Scalar) -> Result<String, String> {
    let old: toml::Table = text
        .parse()
        .map_err(|_| "config.toml does not parse: fix it by hand".to_string())?;
    if Config::from_toml_str(text).is_err() {
        return Err("config.toml is not a config twr loads: fix it by hand first".into());
    }
    let name = dotted(table, Some(key));
    let header_name = format!("[{}]", dotted(table, None));
    let by_hand =
        format!("config.toml defines {header_name} inline or by dotted keys: edit it by hand");
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let new_line = format!("{} = {}", key_text(key), value.text());
    let all = lines(text);
    // The header line of `table` and the lines of its section, up to the next header.
    let mut section: Option<(Line, Vec<Line>)> = None;
    for line in &all {
        let h = header(&text[line.start..line.end]);
        match (&mut section, h) {
            (Some(_), Some(_)) => break,
            (Some((_, body)), None) => body.push(*line),
            (None, Some(h))
                if !h.array && h.path.iter().map(String::as_str).eq(table.iter().copied()) =>
            {
                section = Some((*line, Vec::new()));
            }
            (None, _) => {}
        }
    }
    let new = match (find_table(&old, table), section) {
        (Found::NotTable(what), _) => {
            return Err(format!(
                "config.toml has {what} as a value, not a table: edit it by hand"
            ));
        }
        (Found::Table(t), None) if t.contains_key(key) => return Err(by_hand),
        (Found::Table(t), Some((_, body))) if t.contains_key(key) => {
            let old_value = &t[key];
            let mut over_lines = false;
            let mut spot = None;
            for line in body {
                let s = &text[line.start..line.end];
                let Some(from) = value_start(s, key) else {
                    continue;
                };
                // The line alone must hold the whole key and value.
                let alone: Option<toml::Table> = s.parse().ok();
                let mut expected = toml::Table::new();
                expected.insert(key.to_string(), old_value.clone());
                if alone.as_ref() != Some(&expected) {
                    over_lines |= alone.is_none();
                    continue;
                }
                if let Some(to) = value_end(s, from, old_value) {
                    spot = Some((line.start + from, line.start + to));
                    break;
                }
            }
            let Some((from, to)) = spot else {
                return Err(if over_lines {
                    format!("config.toml spreads {name} over several lines: edit it by hand")
                } else {
                    format!("config.toml sets {name} in a form this cannot change: edit it by hand")
                });
            };
            format!("{}{}{}", &text[..from], value.text(), &text[to..])
        }
        (Found::Table(_), Some((head, _))) => {
            // After the header line, keeping the file's line ending.
            let ending = if head.next > head.end { "" } else { nl };
            format!(
                "{}{ending}{new_line}{nl}{}",
                &text[..head.next],
                &text[head.next..]
            )
        }
        // A table only implied (by a header below it, as `[tui.notify]` implies `[tui]`)
        // may still get its own header; one defined inline or by dotted keys may not, which
        // the parse below finds.
        (Found::Table(_), None) | (Found::Missing, _) => {
            let mut out = text.to_string();
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push_str(nl);
                }
                let blank =
                    out.ends_with("\n\n") || out.ends_with("\n\r\n") || out.trim().is_empty();
                if !blank {
                    out.push_str(nl);
                }
            }
            let path: Vec<String> = table.iter().map(|p| key_text(p)).collect();
            out.push_str(&format!("[{}]{nl}{new_line}{nl}", path.join(".")));
            out
        }
    };
    let parsed: toml::Table = match new.parse() {
        Ok(t) => t,
        Err(_) => return Err(by_hand),
    };
    let mut expected = old;
    let mut cur = &mut expected;
    for part in table {
        let entry = cur
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        match entry {
            toml::Value::Table(t) => cur = t,
            _ => {
                return Err(format!(
                    "could not edit config.toml safely: edit {name} by hand"
                ))
            }
        }
    }
    cur.insert(key.to_string(), value.value());
    if parsed != expected {
        return Err(format!(
            "could not edit config.toml safely: edit {name} by hand"
        ));
    }
    if Config::from_toml_str(&new).is_err() {
        return Err(format!("twr does not take {name} = {}", value.text()));
    }
    Ok(new)
}

/// The value of `table.key` as the file at `path` has it now (`None` when unset, or when the
/// file is missing, not a regular file or does not parse), for the settings view to show what
/// the file says. A FIFO in its place is not waited on.
pub fn read_key(path: &Path, table: &[&str], key: &str) -> Option<toml::Value> {
    let bytes = read_plain(&std::fs::canonicalize(path).ok()?).ok()?;
    let doc: toml::Table = std::str::from_utf8(&bytes).ok()?.parse().ok()?;
    match find_table(&doc, table) {
        Found::Table(t) => t.get(key).cloned(),
        _ => None,
    }
}

/// Reads the file at `path` (missing = empty), applies [`set_key`], and writes the result
/// back safely ([`rewrite`]); a missing file is created private to the user (0600, in a
/// private directory). Returns why not when it did not.
pub fn write_key(path: &Path, table: &[&str], key: &str, value: &Scalar) -> Result<(), String> {
    write_key_with(path, table, key, value, |_| {})
}

/// [`write_key`], calling `before_write` with the file's path once the new text is ready and
/// before the file is checked for a change and written (tests change the file there).
fn write_key_with(
    path: &Path,
    table: &[&str],
    key: &str,
    value: &Scalar,
    before_write: impl FnOnce(&Path),
) -> Result<(), String> {
    let file = match std::fs::canonicalize(path) {
        // What a symbolic link names: the link stays.
        Ok(real) => real,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if std::fs::symlink_metadata(path).is_ok() {
                return Err("config.toml is a link to nothing: fix it by hand".into());
            }
            let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
            if let Some(dir) = dir {
                twr_core::paths::ensure_private_dir(dir)
                    .map_err(|e| format!("creating the config directory: {e}"))?;
            }
            let new = set_key("", table, key, value)?;
            return create(path, &new, before_write);
        }
        Err(e) => return Err(format!("reading config.toml: {e}")),
    };
    rewrite(&file, table, key, value, before_write)
}

/// The bytes of `file`, read without following a link or waiting on a FIFO put in its place,
/// under a shared lock (another TUI's [`rewrite`] is never seen half done; one holding it past
/// [`READ_WAIT`] does not hold up the view, which reads as it is then).
fn read_plain(file: &Path) -> Result<Vec<u8>, String> {
    let mut f = open_plain(file, false)?;
    let until = std::time::Instant::now() + READ_WAIT;
    while let Err(std::fs::TryLockError::WouldBlock) = f.try_lock_shared() {
        if std::time::Instant::now() >= until {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    read_all(&mut f)
}

/// How long [`read_key`] waits for a writer: it runs where the TUI draws.
const READ_WAIT: std::time::Duration = std::time::Duration::from_millis(200);

/// `file` opened (for writing too, with `write`), never through a link nor waiting on a FIFO
/// put in its place; only a regular file.
fn open_plain(file: &Path, write: bool) -> Result<std::fs::File, String> {
    let f = std::fs::OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(file)
        .map_err(|e| format!("reading config.toml: {e}"))?;
    let meta = f
        .metadata()
        .map_err(|e| format!("reading config.toml: {e}"))?;
    if !meta.is_file() {
        return Err("config.toml is not a regular file: edit it by hand".into());
    }
    Ok(f)
}

/// What `f` holds, from its start.
fn read_all(f: &mut std::fs::File) -> Result<Vec<u8>, String> {
    use std::io::Seek;
    let mut bytes = Vec::new();
    f.rewind()
        .and_then(|()| f.read_to_end(&mut bytes))
        .map_err(|e| format!("reading config.toml: {e}"))?;
    Ok(bytes)
}

/// Sets `table.key` in `file` ([`set_key`]), in place: the file stays the same file, so its
/// owner, group, mode, ACLs, extended attributes and hard links stay what they were, whatever
/// they are, and no copy of it ever exists that others could read. It is locked (`flock`) from
/// the read to the end of the write, so another TUI's edit waits and then reads this one's
/// result: neither is lost. An editor takes no lock: the file is read again just before it is
/// written, and left alone when it changed. While the new text is written, a copy of the old
/// one, private to the user (0600), waits beside it, removed once the file is on disk: what a
/// crash in between leaves to restore it from.
fn rewrite(
    file: &Path,
    table: &[&str],
    key: &str,
    value: &Scalar,
    before_write: impl FnOnce(&Path),
) -> Result<(), String> {
    use std::io::Seek;
    let mut f = open_plain(file, true)?;
    f.lock().map_err(|e| format!("locking config.toml: {e}"))?;
    let old = read_all(&mut f)?;
    let text = String::from_utf8(old.clone())
        .map_err(|_| "config.toml is not utf-8: edit it by hand".to_string())?;
    let new = set_key(&text, table, key, value)?;
    if new == text {
        return Ok(());
    }
    let dir = dir_of(file);
    let backup = Temp::write(dir, file.file_name().unwrap_or_default(), &text)?;
    // The copy's name on disk too, before the file is touched: a crash must not take both.
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("writing a copy of config.toml beside it: {e}"))?;
    before_write(file);
    // Still the file that was read: an editor that saves by renaming a new file over it left
    // the descriptor on the old one, which nobody reads any more.
    let same = |a: &std::fs::Metadata, b: &std::fs::Metadata| {
        use std::os::unix::fs::MetadataExt;
        (a.dev(), a.ino()) == (b.dev(), b.ino())
    };
    let replaced = match (std::fs::symlink_metadata(file), f.metadata()) {
        (Ok(now), Ok(ours)) => !same(&now, &ours),
        _ => true,
    };
    if replaced || read_all(&mut f)? != old {
        return Err("config.toml changed meanwhile: try again".into());
    }
    let len = u64::try_from(new.len()).unwrap_or(u64::MAX);
    f.rewind()
        .and_then(|()| f.write_all(new.as_bytes()))
        .and_then(|()| f.set_len(len))
        .and_then(|()| f.sync_all())
        .map_err(|e| {
            format!(
                "writing config.toml: {e} (its old text is in {})",
                backup.path.display()
            )
        })
        .inspect_err(|_| backup.keep())?;
    drop(backup);
    Ok(())
}

/// A temporary file beside the config, private to the user, removed when dropped unless
/// [`Temp::keep`] said it is still needed.
struct Temp {
    path: PathBuf,
    keep: std::cell::Cell<bool>,
}

impl Temp {
    /// Creates a new temporary file in `dir` (never through a link, never over a file that is
    /// there) with mode 0600, holding `text`, flushed to disk.
    fn write(dir: &Path, name: &std::ffi::OsStr, text: &str) -> Result<Temp, String> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(name);
        tmp_name.push(format!(".twr-edit-{}-{nonce:x}", std::process::id()));
        let path = dir.join(tmp_name);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("writing a copy of config.toml beside it: {e}"))?;
        let temp = Temp {
            path,
            keep: std::cell::Cell::new(false),
        };
        let written = (|| {
            // Set on the descriptor, whatever the umask took off at creation.
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            f.write_all(text.as_bytes())?;
            f.sync_all()
        })();
        written.map_err(|e| format!("writing a copy of config.toml beside it: {e}"))?;
        Ok(temp)
    }

    /// Marks the file as still needed (it holds what a failed write lost): dropping it no
    /// longer removes it.
    fn keep(&self) {
        self.keep.set(true);
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        if !self.keep.get() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// The directory `file` is in.
fn dir_of(file: &Path) -> &Path {
    file.parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// Flushes `dir`'s entries to disk, so a link made in it survives a crash. Best effort: the
/// file is in place already when this runs, and a directory some systems do not let one sync
/// is no reason to report the edit failed.
fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Creates `file`, missing until now, holding `new` with mode 0600: a temporary file beside it,
/// linked into place (which fails, rather than replaces, when a file appeared there meanwhile).
fn create(file: &Path, new: &str, before_write: impl FnOnce(&Path)) -> Result<(), String> {
    let dir = dir_of(file);
    let temp = Temp::write(dir, file.file_name().unwrap_or_default(), new)?;
    before_write(file);
    match std::fs::hard_link(&temp.path, file) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err("config.toml changed meanwhile: try again".into());
        }
        Err(e) => return Err(format!("writing config.toml: {e}")),
    }
    // The temporary name goes; the file stays under its own.
    drop(temp);
    sync_dir(dir);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    /// A config with comments, blank lines, a secret and several tables.
    const SAMPLE: &str = "\
# my twr config
default_runtime = \"claude-code\"

[boxes.lab]
token = \"secret-token-0123456789abcdef\"   # never share
ssh = \"me@lab.lan\"

[tui]
# how the TUI looks
mouse = true         # select with the mouse
animate = true

[tui.notify]
terminal = \"bell\"
";

    /// The lines of `a` and `b` that differ, by number.
    fn changed_lines(a: &str, b: &str) -> Vec<usize> {
        let (a, b): (Vec<_>, Vec<_>) = (a.split('\n').collect(), b.split('\n').collect());
        assert_eq!(a.len(), b.len(), "line count changed:\n{b:?}");
        (0..a.len()).filter(|&i| a[i] != b[i]).collect()
    }

    #[test]
    fn only_the_value_changes_comments_and_spacing_stay() {
        let new = set_key(SAMPLE, &["tui"], "mouse", &Scalar::Bool(false)).unwrap();
        assert_eq!(changed_lines(SAMPLE, &new), vec![9]);
        assert!(new.contains("\nmouse = false         # select with the mouse\n"));
        assert_eq!(new, SAMPLE.replace("mouse = true ", "mouse = false "));
        // The secret and everything else, byte for byte.
        assert!(new.contains("token = \"secret-token-0123456789abcdef\"   # never share\n"));
    }

    #[test]
    fn a_nested_table_and_an_int_and_a_string() {
        let new = set_key(
            SAMPLE,
            &["tui", "notify"],
            "terminal",
            &Scalar::Str("osc9".into()),
        )
        .unwrap();
        assert_eq!(
            new,
            SAMPLE.replace("terminal = \"bell\"", "terminal = \"osc9\"")
        );
        let new = set_key(SAMPLE, &["tui"], "compact_width", &Scalar::Int(90)).unwrap();
        assert_eq!(
            new,
            SAMPLE.replace("[tui]\n", "[tui]\ncompact_width = 90\n"),
            "added on the line after the header"
        );
    }

    #[test]
    fn quoted_keys_and_hashes_in_strings() {
        let text = "[tui]\n  \"suggest\"   =  true  # off sends nothing\n";
        let new = set_key(text, &["tui"], "suggest", &Scalar::Bool(false)).unwrap();
        assert_eq!(
            new,
            "[tui]\n  \"suggest\"   =  false  # off sends nothing\n"
        );
        let text = "[tui]\n'animate'=true\n";
        let new = set_key(text, &["tui"], "animate", &Scalar::Bool(false)).unwrap();
        assert_eq!(new, "[tui]\n'animate'=false\n");

        let text = "[orchestrator]\nmodel = \"a # b\" # the # model\n";
        let new = set_key(
            text,
            &["orchestrator"],
            "model",
            &Scalar::Str("x\"y#".into()),
        )
        .unwrap();
        assert_eq!(new, "[orchestrator]\nmodel = \"x\\\"y#\" # the # model\n");
        assert_eq!(
            new.parse::<toml::Table>().unwrap()["orchestrator"]["model"].as_str(),
            Some("x\"y#")
        );
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let text = "# c\r\n[tui]\r\nmouse = true # m\r\n\r\n[threads]\r\nthinking = false\r\n";
        let new = set_key(text, &["tui"], "mouse", &Scalar::Bool(false)).unwrap();
        assert_eq!(new, text.replace("mouse = true", "mouse = false"));
        let new = set_key(text, &["tui"], "animate", &Scalar::Bool(false)).unwrap();
        assert_eq!(
            new,
            text.replace("[tui]\r\n", "[tui]\r\nanimate = false\r\n")
        );
        let new = set_key(
            text,
            &["tui", "notify"],
            "when",
            &Scalar::Str("always".into()),
        )
        .unwrap();
        assert_eq!(
            new,
            format!("{text}\r\n[tui.notify]\r\nwhen = \"always\"\r\n")
        );
    }

    #[test]
    fn a_missing_table_is_appended() {
        let text = "default_runtime = \"codex\"\n";
        let new = set_key(text, &["tui"], "suggest", &Scalar::Bool(false)).unwrap();
        assert_eq!(
            new,
            "default_runtime = \"codex\"\n\n[tui]\nsuggest = false\n"
        );
        // Already ending with a blank line: no second one.
        let text = "default_runtime = \"codex\"\n\n";
        let new = set_key(text, &["tui"], "suggest", &Scalar::Bool(false)).unwrap();
        assert_eq!(
            new,
            "default_runtime = \"codex\"\n\n[tui]\nsuggest = false\n"
        );
        // No final newline: one added first.
        let text = "default_runtime = \"codex\" # last";
        let new = set_key(text, &["tui"], "suggest", &Scalar::Bool(false)).unwrap();
        assert_eq!(
            new,
            "default_runtime = \"codex\" # last\n\n[tui]\nsuggest = false\n"
        );
        // An empty file.
        let new = set_key(
            "",
            &["tui", "notify"],
            "terminal",
            &Scalar::Str("bell".into()),
        )
        .unwrap();
        assert_eq!(new, "[tui.notify]\nterminal = \"bell\"\n");
        // A header that ends the file without a newline gets its key on a line of its own.
        let new = set_key("[tui]", &["tui"], "mouse", &Scalar::Bool(false)).unwrap();
        assert_eq!(new, "[tui]\nmouse = false\n");
    }

    #[test]
    fn a_sub_table_when_only_the_table_is_there_and_back() {
        let text = "[tui]\nmouse = true\n";
        let new = set_key(
            text,
            &["tui", "notify"],
            "terminal",
            &Scalar::Str("bell".into()),
        )
        .unwrap();
        assert_eq!(
            new,
            "[tui]\nmouse = true\n\n[tui.notify]\nterminal = \"bell\"\n"
        );
        // `[tui.notify]` alone implies `[tui]`, which may still get its own header.
        let text = "[tui.notify]\nterminal = \"bell\"\n";
        let new = set_key(text, &["tui"], "mouse", &Scalar::Bool(false)).unwrap();
        assert_eq!(
            new,
            "[tui.notify]\nterminal = \"bell\"\n\n[tui]\nmouse = false\n"
        );
    }

    #[test]
    fn inline_tables_dotted_keys_and_multi_line_values_are_refused() {
        let inline = "tui = { mouse = true }\n";
        let err = set_key(inline, &["tui"], "mouse", &Scalar::Bool(false)).unwrap_err();
        assert_eq!(
            err,
            "config.toml defines [tui] inline or by dotted keys: edit it by hand"
        );
        let err = set_key(inline, &["tui"], "animate", &Scalar::Bool(false)).unwrap_err();
        assert!(err.contains("inline or by dotted keys"), "{err}");

        let dotted = "[tui]\nnotify.terminal = \"bell\"\n";
        let err = set_key(
            dotted,
            &["tui", "notify"],
            "terminal",
            &Scalar::Str("osc9".into()),
        )
        .unwrap_err();
        assert!(
            err.contains("[tui.notify] inline or by dotted keys"),
            "{err}"
        );
        let err = set_key(
            dotted,
            &["tui", "notify"],
            "when",
            &Scalar::Str("always".into()),
        )
        .unwrap_err();
        assert!(err.contains("inline or by dotted keys"), "{err}");
        let root_dotted = "tui.mouse = true\n";
        let err = set_key(root_dotted, &["tui"], "mouse", &Scalar::Bool(false)).unwrap_err();
        assert!(err.contains("inline or by dotted keys"), "{err}");

        let multi = "[orchestrator]\nmodel = \"\"\"\nopus\"\"\"\n";
        let err = set_key(multi, &["orchestrator"], "model", &Scalar::Str("x".into())).unwrap_err();
        assert_eq!(
            err,
            "config.toml spreads orchestrator.model over several lines: edit it by hand"
        );

        let value = "tui = 3\n";
        assert!(set_key(value, &["tui"], "mouse", &Scalar::Bool(true)).is_err());
    }

    #[test]
    fn a_config_twr_would_refuse_is_left_alone() {
        let err = set_key("[tui\n", &["tui"], "mouse", &Scalar::Bool(true)).unwrap_err();
        assert_eq!(err, "config.toml does not parse: fix it by hand");
        let unknown = "[tui]\nbogus = 1\n";
        let err = set_key(unknown, &["tui"], "mouse", &Scalar::Bool(true)).unwrap_err();
        assert!(err.contains("not a config twr loads"), "{err}");
        // Nor is a value twr would refuse written.
        let err = set_key(SAMPLE, &["tui"], "card_order", &Scalar::Str("nope".into())).unwrap_err();
        assert_eq!(err, "twr does not take tui.card_order = \"nope\"");
        let err = set_key(SAMPLE, &["tui"], "mouse", &Scalar::Int(1)).unwrap_err();
        assert!(err.starts_with("twr does not take"), "{err}");
    }

    #[test]
    fn a_header_look_alike_in_a_string_is_caught() {
        // `[tui]` inside a multi-line string, before the real one, is taken for the table:
        // what the edit does to the string is found by the check, and refused.
        let text = "[orchestrator]\nmodel = \"\"\"\n[tui]\nopus\"\"\"\n\n[tui]\nmouse = true\n";
        let err = set_key(text, &["tui"], "animate", &Scalar::Bool(false)).unwrap_err();
        assert_eq!(
            err,
            "could not edit config.toml safely: edit tui.animate by hand"
        );
        let err = set_key(text, &["tui"], "mouse", &Scalar::Bool(false)).unwrap_err();
        assert!(err.ends_with("edit it by hand"), "{err}");
        // Without a real `[tui]`, the look-alike is no table at all: one is appended.
        let text = "[orchestrator]\nmodel = \"\"\"\n[tui]\nopus\"\"\"\n";
        let new = set_key(text, &["tui"], "mouse", &Scalar::Bool(false)).unwrap();
        assert_eq!(new, format!("{text}\n[tui]\nmouse = false\n"));
    }

    /// A temporary directory holding `config.toml` with `text`.
    fn dir_with(text: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        std::fs::write(&file, text).unwrap();
        (dir, file)
    }

    /// The names in `dir`, sorted.
    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    /// The file's owner, group and mode stay: a copy readable by another group is never made.
    #[test]
    fn write_key_keeps_the_files_group() {
        // A group of the user's other than the one new files get, if there is one.
        let mut groups = vec![0 as libc::gid_t; 64];
        let n = unsafe { libc::getgroups(64, groups.as_mut_ptr()) };
        let primary = unsafe { libc::getegid() };
        let Some(other) = groups[..usize::try_from(n).unwrap_or(0)]
            .iter()
            .copied()
            .find(|g| *g != primary)
        else {
            return;
        };
        let (_dir, file) = dir_with(SAMPLE);
        std::os::unix::fs::chown(&file, None, Some(other)).unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_key(&file, &["tui"], "animate", &Scalar::Bool(false)).unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!((meta.gid(), meta.mode() & 0o7777), (other, 0o640));
    }

    #[test]
    fn write_key_edits_the_file_and_keeps_its_mode() {
        let (dir, file) = dir_with(SAMPLE);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_key(&file, &["tui"], "animate", &Scalar::Bool(false)).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(text, SAMPLE.replace("animate = true", "animate = false"));
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o7777, 0o640);
        assert_eq!(
            names(dir.path()),
            vec!["config.toml"],
            "no temporary file left"
        );
        assert_eq!(
            read_key(&file, &["tui"], "animate"),
            Some(toml::Value::Boolean(false))
        );
        assert_eq!(read_key(&file, &["tui"], "suggest"), None);
        assert_eq!(
            read_key(&dir.path().join("none.toml"), &["tui"], "mouse"),
            None
        );
    }

    #[test]
    fn a_refused_edit_writes_nothing() {
        let (dir, file) = dir_with("tui = { mouse = true }\n");
        let err = write_key(&file, &["tui"], "mouse", &Scalar::Bool(false)).unwrap_err();
        assert!(err.contains("inline"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "tui = { mouse = true }\n"
        );
        assert_eq!(names(dir.path()), vec!["config.toml"]);
    }

    #[test]
    fn a_symbolic_link_stays_one() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles-config.toml");
        std::fs::write(&real, SAMPLE).unwrap();
        let link = dir.path().join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        write_key(&link, &["tui"], "mouse", &Scalar::Bool(false)).unwrap();
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        let text = std::fs::read_to_string(&real).unwrap();
        assert_eq!(text, SAMPLE.replace("mouse = true ", "mouse = false "));

        let dangling = dir.path().join("dangling.toml");
        std::os::unix::fs::symlink(dir.path().join("gone"), &dangling).unwrap();
        let err = write_key(&dangling, &["tui"], "mouse", &Scalar::Bool(false)).unwrap_err();
        assert_eq!(err, "config.toml is a link to nothing: fix it by hand");
        assert!(!dir.path().join("gone").exists());
    }

    /// The file stays the same file: a mode tightened meanwhile stays, a hard link sees the
    /// edit, and so would its ACLs and extended attributes (nothing is copied over it).
    #[test]
    fn the_file_is_edited_in_place() {
        let (dir, file) = dir_with(SAMPLE);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("hard");
        std::fs::hard_link(&file, &link).unwrap();
        let inode = std::fs::metadata(&file).unwrap().ino();
        write_key_with(&file, &["tui"], "mouse", &Scalar::Bool(false), |f| {
            std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o600)).unwrap();
        })
        .unwrap();
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!((meta.ino(), meta.mode() & 0o7777), (inode, 0o600));
        let text = std::fs::read_to_string(&link).unwrap();
        assert!(text.contains("mouse = false"), "{text}");
        assert_eq!(
            names(dir.path()),
            vec!["config.toml", "hard"],
            "no copy left"
        );
    }

    /// A file an editor replaced (a new file renamed over it) meanwhile is not written: the
    /// descriptor would be the old file's, which nobody reads any more.
    #[test]
    fn a_file_renamed_over_meanwhile_is_left_alone() {
        let (dir, file) = dir_with(SAMPLE);
        let theirs = format!("{SAMPLE}# saved by an editor\n");
        let err = write_key_with(&file, &["tui"], "mouse", &Scalar::Bool(false), |f| {
            let tmp = dir.path().join("editor.tmp");
            std::fs::write(&tmp, &theirs).unwrap();
            std::fs::rename(&tmp, f).unwrap();
        })
        .unwrap_err();
        assert_eq!(err, "config.toml changed meanwhile: try again");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs);
    }

    /// A read waits for an edit in progress: it never sees the file half written.
    #[test]
    fn a_read_waits_for_the_edit_in_progress() {
        let (_dir, file) = dir_with("[tui]\ncompact_width = 120\n");
        let writer = std::fs::OpenOptions::new().write(true).open(&file).unwrap();
        writer.lock().unwrap();
        let reader = {
            let file = file.clone();
            std::thread::spawn(move || read_key(&file, &["tui"], "compact_width"))
        };
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&file, "[tui]\ncompact_width = 0\n").unwrap();
        writer.unlock().unwrap();
        assert_eq!(reader.join().unwrap(), Some(toml::Value::Integer(0)));
    }

    /// Two TUIs editing at once both land: the lock makes the second read the first's result.
    #[test]
    fn concurrent_edits_are_both_kept() {
        let (_dir, file) = dir_with(SAMPLE);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let run = |key: &'static str, values: Vec<Scalar>| {
            let (file, barrier) = (file.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                for v in &values {
                    write_key(&file, &["tui"], key, v).unwrap();
                }
            })
        };
        let a = run("mouse", (0..30).map(|i| Scalar::Bool(i % 2 == 1)).collect());
        let b = run(
            "compact_width",
            (0..30).map(|i| Scalar::Int(40 + i)).collect(),
        );
        a.join().unwrap();
        b.join().unwrap();
        let doc: toml::Table = std::fs::read_to_string(&file).unwrap().parse().unwrap();
        assert_eq!(doc["tui"]["mouse"], toml::Value::Boolean(true));
        assert_eq!(doc["tui"]["compact_width"], toml::Value::Integer(69));
    }

    #[test]
    fn a_change_meanwhile_is_not_overwritten() {
        let (dir, file) = dir_with(SAMPLE);
        let theirs = format!("{SAMPLE}# edited in another window\n");
        let err = write_key_with(&file, &["tui"], "mouse", &Scalar::Bool(false), |f| {
            std::fs::write(f, &theirs).unwrap();
        })
        .unwrap_err();
        assert_eq!(err, "config.toml changed meanwhile: try again");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs);
        assert_eq!(
            names(dir.path()),
            vec!["config.toml"],
            "the temporary file is removed"
        );

        // A file that appears while a missing one is being created is not replaced either.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config.toml");
        let err = write_key_with(&file, &["tui"], "mouse", &Scalar::Bool(false), |f| {
            std::fs::write(f, "# mine\n").unwrap();
        })
        .unwrap_err();
        assert_eq!(err, "config.toml changed meanwhile: try again");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# mine\n");
        assert_eq!(names(dir.path()), vec!["config.toml"]);
    }

    #[test]
    fn a_missing_file_is_created_private() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("twr").join("config.toml");
        write_key(&file, &["tui"], "suggest", &Scalar::Bool(false)).unwrap();
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            "[tui]\nsuggest = false\n"
        );
        assert_eq!(std::fs::metadata(&file).unwrap().mode() & 0o777, 0o600);
        let parent = std::fs::metadata(file.parent().unwrap()).unwrap();
        assert_eq!(parent.mode() & 0o777, 0o700);
        assert_eq!(names(file.parent().unwrap()), vec!["config.toml"]);
    }

    #[test]
    fn a_fifo_is_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("config.toml");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let err = write_key(&fifo, &["tui"], "mouse", &Scalar::Bool(false)).unwrap_err();
        assert_eq!(err, "config.toml is not a regular file: edit it by hand");
        assert_eq!(read_key(&fifo, &["tui"], "mouse"), None);
    }
}
