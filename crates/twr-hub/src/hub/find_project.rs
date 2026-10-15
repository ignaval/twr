//! Finding a project by its repository folder name (`thread.new` `project = "api"` when `api`
//! is neither a configured project nor an absolute path): a git repository (a `.git`
//! directory or file at its top) at `<root>/<name>` or `<root>/*/<name>` for each of the
//! `project_roots` (the middle folder, an organization or group folder like
//! `~/code/<org>/<repo>`, must not be a git repository itself, so nothing inside a repository
//! is found). Hidden folders are skipped.
//!
//! Names match exactly; only when no repository matches exactly do they match ignoring case.
//! Nothing else is ever matched (no similarity). The search lists at most [`MAX_DIRS`]
//! folders and reads at most [`MAX_ENTRIES`] entries of each, so a huge root costs little;
//! it runs off the async runtime. A search cut short by these limits never picks a
//! repository (another of the same name may be in what it skipped).
//!
//! The lookup is the hub machine's: a thread on a remote box never searches it (its project
//! is configured for the box, or an absolute path there).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Most folders listed by one lookup (the roots and the folders under them).
pub(super) const MAX_DIRS: usize = 2048;
/// Most entries read from one folder.
pub(super) const MAX_ENTRIES: usize = 4096;

/// What a lookup found.
#[derive(Debug, Default)]
pub(super) struct Found {
    /// The roots that exist and were searched (canonical, each once).
    pub searched: Vec<PathBuf>,
    /// Matching git repositories (canonical, each once): exact matches if there are any,
    /// else the ones matching ignoring case.
    pub repos: Vec<PathBuf>,
    /// Folders with the name that are not git repositories (canonical), for the error.
    pub not_git: Vec<PathBuf>,
    /// A limit stopped the search early ([`MAX_DIRS`], [`MAX_ENTRIES`]).
    pub truncated: bool,
}

/// Whether `s` is a plain folder name worth looking up: 1-255 letters, digits, `-`, `_` or
/// `.`, not starting with `.` (no `.`, `..` or hidden folder) or `-`.
pub(super) fn is_plain_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s.starts_with('.')
        && !s.starts_with('-')
        && s.chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Looks for git repositories named `name` under `roots` (see the module docs).
pub(super) fn find(roots: &[PathBuf], name: &str) -> Found {
    let mut found = Found::default();
    let mut exact = Vec::new();
    let mut folded = Vec::new();
    let mut listed = 0usize;
    let lower = name.to_lowercase();
    for root in roots {
        let Ok(root) = std::fs::canonicalize(root) else {
            continue;
        };
        if !root.is_dir() || found.searched.contains(&root) {
            continue;
        }
        found.searched.push(root.clone());
        let Some(entries) = list(&root, &mut listed, &mut found.truncated) else {
            continue;
        };
        for (entry, path) in entries {
            if !path.is_dir() {
                continue;
            }
            let git = is_git(&path);
            consider(&entry, &path, git, name, &lower, &mut exact, &mut folded);
            consider_not_git(&entry, &path, git, &lower, &mut found.not_git);
            if git {
                continue;
            }
            // An organization folder: one level deeper.
            let Some(children) = list(&path, &mut listed, &mut found.truncated) else {
                continue;
            };
            for (child, cpath) in children {
                if matches(&child, name, &lower).is_none() || !cpath.is_dir() {
                    continue;
                }
                let git = is_git(&cpath);
                consider(&child, &cpath, git, name, &lower, &mut exact, &mut folded);
                consider_not_git(&child, &cpath, git, &lower, &mut found.not_git);
            }
        }
    }
    found.repos = dedup(if exact.is_empty() { folded } else { exact });
    found.not_git = dedup(found.not_git);
    found
}

/// The non-hidden entries of `dir` (name, path) by name, or `None` when it cannot be read or
/// [`MAX_DIRS`] folders were listed already.
fn list(dir: &Path, listed: &mut usize, truncated: &mut bool) -> Option<Vec<(String, PathBuf)>> {
    if *listed >= MAX_DIRS {
        *truncated = true;
        return None;
    }
    *listed += 1;
    let mut out = Vec::new();
    for (i, entry) in std::fs::read_dir(dir).ok()?.enumerate() {
        if i >= MAX_ENTRIES {
            *truncated = true;
            break;
        }
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
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

/// Whether `dir` is the top of a git repository or worktree (a `.git` directory or file).
fn is_git(dir: &Path) -> bool {
    std::fs::metadata(dir.join(OsStr::new(".git"))).is_ok_and(|m| m.is_dir() || m.is_file())
}

/// `paths` canonical, each once, in order (the ones that vanished are dropped).
fn dedup(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in paths {
        if let Ok(p) = std::fs::canonicalize(&p) {
            if !out.contains(&p) {
                out.push(p);
            }
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
}
