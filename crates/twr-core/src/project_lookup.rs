//! Finding a git repository by its folder name under a list of roots: the hub's
//! `project_roots` lookup (`thread.new` `project = "api"`, `hub::find_project`) and a box's
//! search for a project's repository (`twr runner --find-repo`), one set of rules for both.
//!
//! A git repository (a `.git` directory or file at its top) at `<root>/<name>` or
//! `<root>/*/<name>` for each root (the middle folder, an organization or group folder like
//! `~/code/<org>/<repo>`, must not be a git repository itself, so nothing inside a repository
//! is found). Hidden folders are skipped.
//!
//! Names match exactly; only when no repository matches exactly do they match ignoring case.
//! Nothing else is ever matched (no similarity). The search lists at most [`MAX_DIRS`]
//! folders and reads at most [`MAX_ENTRIES`] entries of each, so a huge root costs little;
//! it blocks (callers run it off the async runtime). A search cut short by these limits
//! says so ([`Found::truncated`]): another repository of the same name may be in what it
//! skipped. So may a folder that could not be read ([`Found::unreadable`]), which is skipped
//! too: one that could not be listed, an entry whose kind could not be read, a folder whose
//! `.git` could not be looked at (one that can be listed but not searched, say), a match that
//! could not be resolved, a folder whose name is not UTF-8 (no name to match; a hidden one
//! aside). Something that is not there is not that. A search given a deadline
//! ([`find_until`]) stops listing folders once it has passed, and is [`Found::truncated`] then.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Most folders listed by one lookup (the roots and the folders under them).
pub const MAX_DIRS: usize = 2048;
/// Most entries read from one folder.
pub const MAX_ENTRIES: usize = 4096;

/// What a lookup found.
#[derive(Debug, Default)]
pub struct Found {
    /// The roots that exist and were searched (canonical, each once).
    pub searched: Vec<PathBuf>,
    /// Matching git repositories (canonical, each once): exact matches if there are any,
    /// else the ones matching ignoring case.
    pub repos: Vec<PathBuf>,
    /// Folders with the name that are not git repositories (canonical), for the error.
    pub not_git: Vec<PathBuf>,
    /// A limit stopped the search early ([`MAX_DIRS`], [`MAX_ENTRIES`], the deadline of
    /// [`find_until`]).
    pub truncated: bool,
    /// A root, a folder in one or a match that exists could not be resolved, listed or looked
    /// into (permission denied, say): skipped, and a repository of the name may be in it.
    pub unreadable: bool,
}

/// Whether `s` is a plain folder name worth looking up: 1-255 letters, digits, `-`, `_` or
/// `.`, not starting with `.` (no `.`, `..` or hidden folder) or `-`.
pub fn is_plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('.')
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Looks for git repositories named `name` under `roots` (see the module docs).
pub fn find(roots: &[PathBuf], name: &str) -> Found {
    find_until(roots, name, None)
}

/// [`find`], listing no folder once `deadline` has passed ([`Found::truncated`] then).
pub fn find_until(roots: &[PathBuf], name: &str, deadline: Option<std::time::Instant>) -> Found {
    let mut found = Found::default();
    let mut exact = Vec::new();
    let mut folded = Vec::new();
    let mut listed = Listed { count: 0, deadline };
    let lower = name.to_lowercase();
    for root in roots {
        let root = match std::fs::canonicalize(root) {
            Ok(r) => r,
            Err(e) => {
                found.unreadable |= !absent(&e);
                continue;
            }
        };
        if !is_dir(&root, &mut found) || found.searched.contains(&root) {
            continue;
        }
        found.searched.push(root.clone());
        let Some(entries) = list(&root, &mut listed, &mut found) else {
            continue;
        };
        for (entry, path) in entries {
            if !is_dir(&path, &mut found) {
                continue;
            }
            let Some(git) = is_git(&path, &mut found) else {
                continue;
            };
            consider(&entry, &path, git, name, &lower, &mut exact, &mut folded);
            consider_not_git(&entry, &path, git, &lower, &mut found.not_git);
            if git {
                continue;
            }
            // An organization folder: one level deeper.
            let Some(children) = list(&path, &mut listed, &mut found) else {
                continue;
            };
            for (child, cpath) in children {
                if matches(&child, name, &lower).is_none() || !is_dir(&cpath, &mut found) {
                    continue;
                }
                let Some(git) = is_git(&cpath, &mut found) else {
                    continue;
                };
                consider(&child, &cpath, git, name, &lower, &mut exact, &mut folded);
                consider_not_git(&child, &cpath, git, &lower, &mut found.not_git);
            }
        }
    }
    let repos = if exact.is_empty() { folded } else { exact };
    found.repos = dedup(repos, &mut found.unreadable);
    found.not_git = dedup(std::mem::take(&mut found.not_git), &mut found.unreadable);
    found
}

/// Whether an error reading a path means it is not there (rather than unreadable).
fn absent(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
    )
}

/// Whether `path` is a folder (following symbolic links); one whose kind could not be read is
/// not, and is [`Found::unreadable`].
fn is_dir(path: &Path, found: &mut Found) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_dir(),
        Err(e) => {
            found.unreadable |= !absent(&e);
            false
        }
    }
}

/// How many folders a lookup listed, and until when it may list more.
struct Listed {
    count: usize,
    deadline: Option<std::time::Instant>,
}

/// The non-hidden entries of `dir` (name, path) by name, or `None` when it cannot be read
/// ([`Found::unreadable`], unless it is gone), or [`MAX_DIRS`] folders were listed already or
/// the deadline passed ([`Found::truncated`]). An entry whose name is not UTF-8 is not listed,
/// and makes the lookup [`Found::unreadable`] when it may be a folder (a hidden one aside):
/// a repository there could not be told by its name.
fn list(dir: &Path, listed: &mut Listed, found: &mut Found) -> Option<Vec<(String, PathBuf)>> {
    let late = listed
        .deadline
        .is_some_and(|d| std::time::Instant::now() >= d);
    if listed.count >= MAX_DIRS || late {
        found.truncated = true;
        return None;
    }
    listed.count += 1;
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) => {
            found.unreadable |= !absent(&e);
            return None;
        }
    };
    for (i, entry) in entries.enumerate() {
        if i >= MAX_ENTRIES {
            found.truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            found.unreadable = true;
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            use std::os::unix::ffi::OsStrExt;
            if !entry.file_name().as_bytes().starts_with(b".") {
                // (A symbolic link counts by what it points to.)
                found.unreadable |= match std::fs::metadata(entry.path()) {
                    Ok(m) => m.is_dir(),
                    Err(e) => !absent(&e),
                };
            }
            continue;
        };
        if !name.starts_with('.') {
            out.push((name, entry.path()));
        }
    }
    // The same folders listed first whatever order the file system returns.
    out.sort();
    Some(out)
}

/// `Some(true)`: `entry` is `name`; `Some(false)`: it is `name` ignoring case.
fn matches(entry: &str, name: &str, lower: &str) -> Option<bool> {
    if entry == name {
        Some(true)
    } else if entry.to_lowercase() == lower {
        Some(false)
    } else {
        None
    }
}

/// Adds a git repository `path` (entry `entry`) matching `name` to the exact or the
/// case-folded matches.
fn consider(
    entry: &str,
    path: &Path,
    git: bool,
    name: &str,
    lower: &str,
    exact: &mut Vec<PathBuf>,
    folded: &mut Vec<PathBuf>,
) {
    match matches(entry, name, lower) {
        Some(true) if git => exact.push(path.to_path_buf()),
        Some(false) if git => folded.push(path.to_path_buf()),
        _ => {}
    }
}

/// Adds a folder named `name` (in any case) that is not a git repository to `not_git`.
fn consider_not_git(entry: &str, path: &Path, git: bool, lower: &str, not_git: &mut Vec<PathBuf>) {
    if !git && entry.to_lowercase() == lower {
        not_git.push(path.to_path_buf());
    }
}

/// Whether folder `dir` is the top of a git repository or worktree (a `.git` directory or
/// file); `None` when its `.git` could not be looked at (a folder that can be listed but not
/// searched, say: [`Found::unreadable`]).
fn is_git(dir: &Path, found: &mut Found) -> Option<bool> {
    match std::fs::metadata(dir.join(OsStr::new(".git"))) {
        Ok(m) => Some(m.is_dir() || m.is_file()),
        Err(e) if absent(&e) => Some(false),
        Err(_) => {
            found.unreadable = true;
            None
        }
    }
}

/// `paths` canonical, each once, in order: the ones that vanished are dropped, and so are the
/// ones that could not be resolved, which makes the search `unreadable`.
fn dedup(paths: Vec<PathBuf>, unreadable: &mut bool) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        match std::fs::canonicalize(&p) {
            Ok(p) => {
                if !out.contains(&p) {
                    out.push(p);
                }
            }
            Err(e) => *unreadable |= !absent(&e),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(p: &Path) {
        std::fs::create_dir_all(p.join(".git")).unwrap();
    }

    #[test]
    fn plain_names() {
        for ok in ["api", "orbit-webapp", "a.b_c", "Ünïcode", "x"] {
            assert!(is_plain_name(ok), "{ok}");
        }
        let long = "a".repeat(256);
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "-x",
            "a/b",
            "/abs",
            "~",
            "~/code",
            "a b",
            "a\\b",
            long.as_str(),
        ] {
            assert!(!is_plain_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn finds_at_both_depths_exact_before_case_folded() {
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        repo(&a.join("api"));
        repo(&b.join("org").join("web"));
        // A worktree or submodule: `.git` is a file.
        std::fs::create_dir_all(b.join("wt")).unwrap();
        std::fs::write(b.join("wt").join(".git"), "gitdir: /x\n").unwrap();
        // Inside a repository: never found.
        repo(&a.join("api").join("inner"));
        // Not a repository: ignored, but reported.
        std::fs::create_dir_all(a.join("docs")).unwrap();
        // Hidden: skipped.
        repo(&a.join(".hidden").join("secret"));
        let roots = [a.clone(), b.clone(), d.path().join("missing"), a.clone()];

        let f = find(&roots, "api");
        assert_eq!(f.repos, [a.join("api").canonicalize().unwrap()]);
        assert_eq!(f.searched.len(), 2, "{:?}", f.searched);
        assert!(!f.truncated);
        let f = find(&roots, "web");
        assert_eq!(f.repos, [b.join("org/web").canonicalize().unwrap()]);
        assert_eq!(find(&roots, "wt").repos.len(), 1);
        assert!(find(&roots, "inner").repos.is_empty());
        assert!(find(&roots, "secret").repos.is_empty());
        let f = find(&roots, "docs");
        assert!(f.repos.is_empty());
        assert_eq!(f.not_git, [a.join("docs").canonicalize().unwrap()]);

        // Case: an exact match wins; without one, a case-folded match counts.
        assert_eq!(find(&roots, "API").repos.len(), 1);
        repo(&b.join("API"));
        let f = find(&roots, "API");
        assert_eq!(f.repos, [b.join("API").canonicalize().unwrap()]);
        // Several case-folded matches are all candidates.
        let f = find(&roots, "Api");
        assert_eq!(f.repos.len(), 2, "{:?}", f.repos);
        // Never by similarity.
        assert!(find(&roots, "ap").repos.is_empty());
        assert!(find(&roots, "apis").repos.is_empty());
    }

    #[test]
    fn a_search_cut_short_says_so() {
        let d = tempfile::tempdir().unwrap();
        for i in 0..MAX_DIRS + 4 {
            std::fs::create_dir(d.path().join(format!("g{i:04}"))).unwrap();
        }
        repo(&d.path().join("api"));
        // Listed after the limit (by name): never searched.
        repo(&d.path().join("zz").join("api"));
        let f = find(&[d.path().to_path_buf()], "api");
        assert!(f.truncated);
        assert_eq!(f.repos, [d.path().join("api").canonicalize().unwrap()]);
    }

    /// A folder whose name is not UTF-8 cannot be matched by name: the search says it could
    /// not read it (a repository of the name may be in it, as an organization folder); a
    /// hidden one, or a file, is not that.
    #[test]
    fn a_folder_whose_name_is_not_utf8_says_so() {
        use std::os::unix::ffi::OsStrExt;
        let d = tempfile::tempdir().unwrap();
        repo(&d.path().join("api"));
        let roots = [d.path().to_path_buf()];
        std::fs::write(d.path().join(OsStr::from_bytes(b"file\xff")), "").unwrap();
        std::fs::create_dir(d.path().join(OsStr::from_bytes(b".hidden\xff"))).unwrap();
        let f = find(&roots, "api");
        assert!(!f.unreadable && !f.truncated, "{f:?}");
        let odd = d.path().join(OsStr::from_bytes(b"org\xff"));
        repo(&odd.join("api"));
        let f = find(&roots, "api");
        assert!(f.unreadable && !f.truncated, "{f:?}");
        assert_eq!(f.repos, [d.path().join("api").canonicalize().unwrap()]);
        // A symbolic link to a folder counts as one.
        std::fs::remove_dir_all(&odd).unwrap();
        std::fs::create_dir(d.path().join("real")).unwrap();
        std::os::unix::fs::symlink(d.path().join("real"), &odd).unwrap();
        assert!(find(&roots, "api").unreadable);
    }

    /// A deadline that has passed lists nothing more: the search says it was cut short.
    #[test]
    fn a_search_past_its_deadline_says_so() {
        let d = tempfile::tempdir().unwrap();
        repo(&d.path().join("api"));
        let roots = [d.path().to_path_buf()];
        let later = std::time::Instant::now() + std::time::Duration::from_secs(600);
        let f = find_until(&roots, "api", Some(later));
        assert!(!f.truncated && f.repos.len() == 1, "{f:?}");
        let f = find_until(&roots, "api", Some(std::time::Instant::now()));
        assert!(f.truncated && f.repos.is_empty(), "{f:?}");
    }

    /// A folder that cannot be listed is skipped, and the search says so: a repository of the
    /// name may be in it. A root that does not exist is not that.
    #[test]
    fn an_unreadable_folder_says_so() {
        use std::os::unix::fs::PermissionsExt;
        if crate::paths::effective_uid() == 0 {
            eprintln!("root reads any folder: skipped");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        repo(&d.path().join("api"));
        let org = d.path().join("org");
        repo(&org.join("api"));
        let roots = [d.path().to_path_buf(), d.path().join("missing")];
        let f = find(&roots, "api");
        assert!(!f.unreadable && !f.truncated);
        assert_eq!(f.repos.len(), 2);
        std::fs::set_permissions(&org, std::fs::Permissions::from_mode(0o000)).unwrap();
        let f = find(&roots, "api");
        std::fs::set_permissions(&org, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(f.unreadable && !f.truncated);
        assert_eq!(f.repos, [d.path().join("api").canonicalize().unwrap()]);
    }

    /// A folder that can be listed but not searched (no `x`): a repository of the name there,
    /// or in an organization folder like it, cannot be told from a plain folder, so the search
    /// says it could not read it.
    #[test]
    fn a_folder_listed_but_not_searchable_says_so() {
        use std::os::unix::fs::PermissionsExt;
        if crate::paths::effective_uid() == 0 {
            eprintln!("root searches any folder: skipped");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let a = d.path().join("a");
        let b = d.path().join("b");
        repo(&a.join("api"));
        repo(&b.join("api"));
        let org = d.path().join("c").join("org");
        repo(&org.join("api"));
        let roots = [a.clone(), b.clone(), d.path().join("c")];
        let f = find(&roots, "api");
        assert!(!f.unreadable && !f.truncated);
        assert_eq!(f.repos.len(), 3);
        let mode = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        // The repository's own folder.
        mode(&b.join("api"), 0o644);
        let f = find(&roots, "api");
        mode(&b.join("api"), 0o755);
        assert!(f.unreadable && !f.truncated);
        assert_eq!(f.repos.len(), 2, "{:?}", f.repos);
        // An organization folder: its entries are listed, their kind cannot be read.
        mode(&org, 0o644);
        let f = find(&roots, "api");
        mode(&org, 0o755);
        assert!(f.unreadable && !f.truncated);
        assert_eq!(f.repos.len(), 2, "{:?}", f.repos);
        assert!(!find(&roots, "api").unreadable);
    }
}
