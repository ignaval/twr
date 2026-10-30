//! The Claude Code orchestrator's file guard: a `PreToolUse` hook (`twr claude-memory-guard`,
//! passed with `--settings`, [`settings`]) that keeps its file tools (Read, Write and Edit:
//! [`TOOLS`]) inside its auto-memory directory, so it can keep memories and touch nothing else.
//!
//! Claude Code's permission rules cannot say this on their own: in `dontAsk` mode it already
//! writes only to its memory directory (and refuses other writes), but it reads anything under
//! its working directory (the orchestrator's is the home directory by default) without a rule,
//! and a deny rule wins over the memory directory too. Edit and a Write over an existing file
//! need the file read first, so Read cannot simply be left out. The hook narrows Read, Write
//! and Edit to the memory directory; Claude Code's own `dontAsk` check still applies to what
//! the hook lets through.
//!
//! The memory directory is `<claude config dir>/projects/<project key>/memory`, the project key
//! being the session's (its transcript's directory, from the hook's `transcript_path`), or,
//! when the working directory is in a git repository, that of the repository's main checkout
//! (Claude Code keeps one memory per repository, worktrees included: [`sanitize`] of its
//! path, read from git's files by [`git_root`], no `git` process). A path is checked as the
//! file system resolves it ([`resolve`]): symbolic links are followed one part at a time,
//! dangling ones included, and a part that does not exist yet is taken as written, never a
//! `..` in it. Anything the guard cannot read or decide is refused ([`check`]), and so is a
//! call it has not decided within [`DEADLINE`] ([`run`]): Claude Code lets a call through when
//! its hook times out, so the guard denies well before the hook's [`HOOK_TIMEOUT_SECS`].
//!
//! An orchestrator on a named account keeps the native account's memory: the hub moves its
//! memory directory there (`autoMemoryDirectory` in [`settings`], [`project_memory_dir`]) and
//! names it to the guard (`--memory-dir <dir>`, [`check_with`]), which then allows exactly that
//! directory instead of deriving one. A directory moved by anything else is still refused.
//!
//! The settings ([`settings`]) keep hooks on over the user's and the project's settings
//! (`disableAllHooks: false`; `--settings` ranks above them). Managed settings rank above
//! `--settings`: when a managed settings file turns hooks off, allows only managed hooks, keeps
//! hooks to managed settings and plugins, or hands the policy to a helper program, or when
//! Claude Code keeps server-managed settings ([`policy_blocks_hooks`]), the hub gives the
//! orchestrator no file tools at all. The environment variables that would drop or wrap the
//! hook ([`HOOK_ENV`]) are removed from the runtime and its attach command.
//!
//! Not seen here, and accepted: a settings `env` block that sets one of those variables again
//! once Claude Code has started, server-managed settings Claude Code has not stored yet (it may
//! fetch them during the session), and managed settings from an MDM profile or the Windows
//! registry. Any of them can turn the hook off or wrap it, and then the file tools are not
//! confined (docs/orchestrator.md says so).

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

/// The tools the guard checks: the orchestrator's only built-in tools.
pub const TOOLS: [&str; 3] = ["Read", "Write", "Edit"];

/// The `twr` subcommand that runs the guard.
pub const COMMAND: &str = "claude-memory-guard";

/// The hook's timeout in [`settings`], in seconds. Claude Code lets the call through when the
/// hook runs longer, so the guard denies at [`DEADLINE`], well before it.
pub const HOOK_TIMEOUT_SECS: u64 = 30;

/// How long the guard takes at most ([`run`]): a call it has not decided by then is denied.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// Environment variable that shortens [`DEADLINE`] (milliseconds; never lengthens it), for
/// tests.
pub const DEADLINE_ENV: &str = "TWR_MEMORY_GUARD_DEADLINE_MS";

/// Longest project key Claude Code uses as is (a longer one gets a hash, which the guard does
/// not compute: that repository's memory directory is then refused).
const MAX_KEY: usize = 200;

/// Most symbolic links [`resolve`] follows in one path (more is a loop, refused).
const MAX_LINKS: usize = 40;

/// The `twr claude-memory-guard` option that names the one memory directory the guard allows
/// ([`check_with`]).
pub const MEMORY_DIR_FLAG: &str = "--memory-dir";

/// `--settings` for a Claude Code orchestrator: the guard (`<exe> claude-memory-guard`) as a
/// `PreToolUse` hook on [`TOOLS`], with hooks kept on (a user's or project's
/// `disableAllHooks: true` would drop it). Claude Code lets a call through when its hook exits
/// with anything but 2 (a missing binary is 127) or times out, so the command exits 2 whenever
/// the guard does not exit 0, and the guard decides within [`DEADLINE`], well inside the
/// hook's [`HOOK_TIMEOUT_SECS`].
///
/// With `memory_dir` (an orchestrator on a named account whose memory is the native account's,
/// [`project_memory_dir`]), the settings move Claude Code's memory there
/// (`autoMemoryDirectory`) and the hook command passes it to the guard
/// (`--memory-dir '<dir>'`), which then allows exactly that directory.
pub fn settings(exe: &Path, memory_dir: Option<&Path>) -> Value {
    let mut command = format!("{} {COMMAND}", shell_quote(&exe.to_string_lossy()));
    if let Some(dir) = memory_dir {
        command.push_str(&format!(
            " {MEMORY_DIR_FLAG} {}",
            shell_quote(&dir.to_string_lossy())
        ));
    }
    let mut out = json!({
        "disableAllHooks": false,
        "hooks": {
            "PreToolUse": [{
                "matcher": TOOLS.join("|"),
                "hooks": [{
                    "type": "command",
                    "command": format!("{command} || exit 2"),
                    "timeout": HOOK_TIMEOUT_SECS,
                }],
            }],
        },
    });
    if let Some(dir) = memory_dir {
        out["autoMemoryDirectory"] = json!(dir.to_string_lossy());
    }
    out
}

/// The memory directory Claude Code keeps, in its configuration directory `config_dir`, for a
/// session in `cwd`: `<config_dir>/projects/<key>/memory`, the key being [`sanitize`] of the
/// main checkout of the git repository `cwd` is in ([`git_root`]), else of `cwd` (resolved
/// when it can be). The directory is given as the file system resolves it ([`resolve`]): the
/// symbolic links on the way (to `config_dir`, or in it to `projects` or the project's
/// directory) are followed and a part that does not exist yet is kept as written, so
/// `autoMemoryDirectory` and the guard's `--memory-dir` name the real directory (Claude Code's
/// `dontAsk` refuses a memory write through a link). `Err` says why there is none twr can name:
/// a key longer than Claude Code uses as is (it hashes those), or a path that cannot be
/// resolved.
pub fn project_memory_dir(config_dir: &Path, cwd: &Path) -> Result<PathBuf, String> {
    let root = git_root(cwd)
        .or_else(|| cwd.canonicalize().ok())
        .unwrap_or_else(|| cwd.to_path_buf());
    let key = sanitize(&root.to_string_lossy());
    if key.len() > MAX_KEY {
        return Err(format!(
            "the project key of {} is longer than {MAX_KEY} characters (Claude Code hashes it)",
            root.display()
        ));
    }
    let dir = config_dir.join("projects").join(key).join("memory");
    resolve(&dir).map_err(|e| format!("{} cannot be resolved: {e}", dir.display()))
}

/// `s` as one word for `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Where [`policy_blocks_hooks`] reads Claude Code's managed policy: the real locations
/// ([`PolicySources::system`]), or others (a test's own files).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicySources {
    /// The directory of the managed settings files (`managed-settings.json`,
    /// `managed-settings.d`); `None`: none on this system.
    pub managed_dir: Option<PathBuf>,
    /// Claude Code's configuration directory, where it keeps server-managed settings
    /// (`remote-settings.json`); `None`: unknown.
    pub config_dir: Option<PathBuf>,
}

impl PolicySources {
    /// This system's: `/Library/Application Support/ClaudeCode` on macOS, `/etc/claude-code` on
    /// other Unix systems, and `CLAUDE_CONFIG_DIR`, else `~/.claude`.
    pub fn system() -> PolicySources {
        let managed_dir = if cfg!(target_os = "macos") {
            Some(PathBuf::from("/Library/Application Support/ClaudeCode"))
        } else if cfg!(unix) {
            Some(PathBuf::from("/etc/claude-code"))
        } else {
            None
        };
        let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR")
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude")));
        PolicySources {
            managed_dir,
            config_dir,
        }
    }
}

/// Why Claude Code's managed settings in `sources` would or might keep the guard's hook from
/// running, `None` when they would not. Then the orchestrator gets no file tools. A managed
/// settings file sets `disableAllHooks`, `allowManagedHooksOnly` or
/// `strictPluginOnlyCustomization` to anything that may lock hooks ([`blocks_hooks`]), or
/// configures a policy helper (`policyHelper`, `policyHelpers`: a program computes the policy
/// that applies), or cannot be read; or Claude Code keeps server-managed settings, which rank
/// above the files and which it fetches again when it starts ([`remote_policy_in`]).
/// Server-managed settings it has not fetched yet are not seen (see the module docs).
pub fn policy_blocks_hooks(sources: &PolicySources) -> Option<String> {
    sources
        .managed_dir
        .as_deref()
        .and_then(hooks_blocked_in)
        .or_else(|| sources.config_dir.as_deref().and_then(remote_policy_in))
}

/// Why the server-managed settings Claude Code keeps in its configuration directory `dir`
/// (`remote-settings.json`, from the claude.ai console or a gateway) make the hook policy
/// unknown: they replace the managed settings files, and Claude Code fetches them again when it
/// starts. `None` when there are none.
fn remote_policy_in(dir: &Path) -> Option<String> {
    let cache = dir.join("remote-settings.json");
    match std::fs::symlink_metadata(&cache) {
        Ok(_) => Some(format!(
            "{} holds server-managed settings, which may change the hook policy",
            cache.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(format!("{}: {e}", cache.display())),
    }
}

/// The managed settings keys that can keep a hook of `--settings` from running.
const HOOK_POLICY_KEYS: [&str; 3] = [
    "disableAllHooks",
    "allowManagedHooksOnly",
    "strictPluginOnlyCustomization",
];

/// The managed settings keys that hand the policy to a helper program, whose output twr cannot
/// know: any value but `null` makes the hook policy unknown.
const POLICY_HELPER_KEYS: [&str; 2] = ["policyHelper", "policyHelpers"];

/// Environment variables that keep Claude Code from running the hooks of `--settings`, or run
/// them through something else: `CLAUDE_CODE_SIMPLE` (bare mode loads no settings hooks),
/// `CLAUDE_CODE_SAFE_MODE` (safe mode runs only managed hooks) and `CLAUDE_CODE_SHELL_PREFIX`
/// (a wrapper around every hook command, which need not run it). The Claude Code adapter removes
/// them from a runtime it gives `--settings`, and from its attach command.
pub const HOOK_ENV: [&str; 3] = [
    "CLAUDE_CODE_SIMPLE",
    "CLAUDE_CODE_SAFE_MODE",
    "CLAUDE_CODE_SHELL_PREFIX",
];

/// Whether managed settings key `key` (of [`HOOK_POLICY_KEYS`]) set to `v` may keep the hook
/// from running. Only an absent or `null` key, a plain `false`, or, for
/// `strictPluginOnlyCustomization`, a list of surfaces without `hooks` lets it run: Claude
/// Code reads a quoted `"true"` as true and most invalid values as their restrictive value
/// (an invalid `strictPluginOnlyCustomization` locks every surface), and the guard takes
/// anything else as blocking too.
fn blocks_hooks(key: &str, v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => false,
        Value::Array(surfaces) if key == "strictPluginOnlyCustomization" => {
            !surfaces.iter().all(|s| {
                s.as_str()
                    .is_some_and(|s| !s.trim().eq_ignore_ascii_case("hooks"))
            })
        }
        _ => true,
    }
}

/// [`policy_blocks_hooks`] for the managed settings in `dir`: `managed-settings.json` and the
/// `*.json` files of `managed-settings.d` (not hidden ones), as Claude Code reads them. Each is
/// read as [`twr_core::fsread::read_settings`] reads: one that is not a regular file, or is
/// larger than [`twr_core::fsread::SETTINGS_MAX`], can't be read, so blocks.
fn hooks_blocked_in(dir: &Path) -> Option<String> {
    let mut files = vec![dir.join("managed-settings.json")];
    let drop_ins = dir.join("managed-settings.d");
    match std::fs::read_dir(&drop_ins) {
        Ok(entries) => {
            for e in entries {
                let Ok(e) = e else {
                    return Some(format!("{} cannot be read", drop_ins.display()));
                };
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".json") && !name.starts_with('.') {
                    files.push(e.path());
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Some(format!("{}: {e}", drop_ins.display())),
    }
    for f in files {
        // Read without blocking and within a cap: the hub reads these under its core lock
        // (an orchestrator's spawn). One that can't be read so is a policy twr can't know.
        let text = match twr_core::fsread::read_settings(&f) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => return Some(format!("{} can't be read: {e}", f.display())),
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return Some(format!("{} is not JSON", f.display()));
        };
        for key in HOOK_POLICY_KEYS {
            if blocks_hooks(key, &v[key]) {
                return Some(format!("{} sets {key} to {}", f.display(), v[key]));
            }
        }
        // A helper computes the policy that applies instead: unknown here.
        for key in POLICY_HELPER_KEYS {
            if !v[key].is_null() {
                return Some(format!(
                    "{} sets {key}, so the policy that applies is computed by a program",
                    f.display()
                ));
            }
        }
    }
    None
}

/// [`DEADLINE`], or less when [`DEADLINE_ENV`] says so.
pub fn deadline() -> Duration {
    std::env::var(DEADLINE_ENV)
        .ok()
        .and_then(|v| v.parse().ok())
        .map_or(DEADLINE, |ms| DEADLINE.min(Duration::from_millis(ms)))
}

/// The guard as the hook runs it: reads its input with `read` and answers it ([`answer`]) on a
/// thread of its own, and denies when that has not finished within `deadline` (a stalled file
/// system, an input that never ends), when it fails to read, or when it panics. `None` lets
/// the call go on.
pub fn run(
    read: impl FnOnce() -> std::io::Result<String> + Send + 'static,
    deadline: Duration,
) -> Option<Value> {
    run_with(read, deadline, None)
}

/// [`run`] allowing only `memory_dir` when given (`--memory-dir`, [`check_with`]).
pub fn run_with(
    read: impl FnOnce() -> std::io::Result<String> + Send + 'static,
    deadline: Duration,
    memory_dir: Option<PathBuf>,
) -> Option<Value> {
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().spawn(move || {
        let out = match read() {
            Ok(input) => answer_with(&input, memory_dir.as_deref()),
            Err(e) => Some(deny(&format!("its input could not be read: {e}"))),
        };
        let _ = tx.send(out);
    });
    if spawned.is_err() {
        return Some(deny("the guard could not start"));
    }
    match rx.recv_timeout(deadline) {
        Ok(out) => out,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Some(deny(&format!(
            "the guard did not decide within {} ms",
            deadline.as_millis()
        ))),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Some(deny("the guard failed")),
    }
}

/// The hook's answer for its input `input` (the `PreToolUse` JSON): `None` lets the call go on
/// to Claude Code's own permission check, `Some(json)` (printed on stdout) denies it.
pub fn answer(input: &str) -> Option<Value> {
    answer_with(input, None)
}

/// [`answer`] allowing only `memory_dir` when given ([`check_with`]).
pub fn answer_with(input: &str, memory_dir: Option<&Path>) -> Option<Value> {
    let reason = match serde_json::from_str::<Value>(input) {
        Ok(v) => check_with(&v, memory_dir).err()?,
        Err(e) => format!("unreadable hook input: {e}"),
    };
    Some(deny(&reason))
}

/// The `deny` decision for `reason`.
fn deny(reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": format!(
                "twr: the orchestrator's file tools work only in its memory directory ({reason}); \
                 leave project files to worker threads"
            ),
        },
    })
}

/// Whether the `PreToolUse` call `input` stays in the session's memory directory: `Err` says
/// why not.
pub fn check(input: &Value) -> Result<(), String> {
    check_with(input, None)
}

/// [`check`], with `memory_dir` (`twr claude-memory-guard --memory-dir <dir>`, the
/// `autoMemoryDirectory` the hub set) the one directory allowed when given, instead of those
/// derived from the transcript and the working directory ([`memory_dirs`]); the transcript is
/// then not needed. Resolved as the path is ([`resolve`]): one that cannot be resolved (a
/// relative one included) allows nothing.
pub fn check_with(input: &Value, memory_dir: Option<&Path>) -> Result<(), String> {
    let tool = input["tool_name"].as_str().unwrap_or_default();
    if !TOOLS.contains(&tool) {
        return Err(format!("{tool:?} is not a file tool the guard knows"));
    }
    let path = input["tool_input"]["file_path"]
        .as_str()
        .filter(|p| !p.is_empty())
        .ok_or("no file_path")?;
    // Claude Code may read `~` as the home directory; the guard does not guess.
    if path.starts_with('~') {
        return Err(format!("{path} starts with ~"));
    }
    let transcript = match memory_dir {
        Some(_) => None,
        None => Some(
            input["transcript_path"]
                .as_str()
                .filter(|p| !p.is_empty())
                .ok_or("no transcript_path, so the memory directory is unknown")?,
        ),
    };
    let cwd = input["cwd"]
        .as_str()
        .filter(|c| !c.is_empty())
        .map(Path::new);
    let path = Path::new(path);
    let path = match (path.is_absolute(), cwd) {
        (true, _) => path.to_path_buf(),
        (false, Some(cwd)) if cwd.is_absolute() => cwd.join(path),
        (false, _) => return Err(format!("relative path {} and no cwd", path.display())),
    };
    let resolved =
        resolve(&path).map_err(|e| format!("{} cannot be resolved: {e}", path.display()))?;
    let dirs = match (memory_dir, transcript) {
        (Some(d), _) => vec![d.to_path_buf()],
        (None, Some(t)) => memory_dirs(Path::new(t), cwd),
        (None, None) => Vec::new(),
    };
    let inside = dirs
        .iter()
        .filter_map(|d| resolve(d).ok())
        .any(|d| resolved != d && resolved.starts_with(&d));
    if inside {
        Ok(())
    } else {
        Err(format!("{} is outside it", path.display()))
    }
}

/// The session's memory directories (see the module docs): the one of its transcript's
/// project, and the one of the git repository its working directory is in.
fn memory_dirs(transcript: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let Some(project) = transcript.parent() else {
        return Vec::new();
    };
    let Some(projects) = project.parent().filter(|p| p.ends_with("projects")) else {
        return Vec::new();
    };
    let mut dirs = vec![project.join("memory")];
    if let Some(key) = cwd
        .and_then(git_root)
        .map(|r| sanitize(&r.to_string_lossy()))
        .filter(|k| k.len() <= MAX_KEY)
    {
        dirs.push(projects.join(key).join("memory"));
    }
    dirs
}

/// The main checkout of the git repository `dir` is in, read from git's files (no `git`
/// process, which could hang): the nearest `.git` at or above `dir`; a worktree's `.git` file
/// names its git directory (`gitdir: <path>`), whose `commondir` names the repository's.
/// `None`: not in one, or one whose git directory is no checkout's `.git` (bare, a submodule).
/// The files are read without blocking and within a cap ([`twr_core::fsread::read_git_file`]):
/// the hub asks under its core lock ([`project_memory_dir`], at an orchestrator's spawn and
/// attach), so a `.git` or `commondir` that is a FIFO, a device or too large to be one makes
/// `dir` no checkout (`None`) at once.
fn git_root(dir: &Path) -> Option<PathBuf> {
    use twr_core::fsread::read_git_file;
    let dir = dir.canonicalize().ok()?;
    let dot_git = dir
        .ancestors()
        .map(|d| d.join(".git"))
        .find(|g| g.symlink_metadata().is_ok())?;
    let common = if dot_git.is_dir() {
        dot_git
    } else {
        let text = read_git_file(&dot_git).ok()??;
        let gitdir = text.lines().next()?.strip_prefix("gitdir:")?.trim();
        let gitdir = dot_git.parent()?.join(gitdir);
        match read_git_file(&gitdir.join("commondir")).ok()? {
            Some(c) => gitdir.join(c.trim_end_matches(['\n', '\r'])),
            None => gitdir,
        }
    };
    let common = common.canonicalize().ok()?;
    common
        .ends_with(".git")
        .then(|| common.parent().map(Path::to_path_buf))
        .flatten()
}

/// Claude Code's project key for the directory `path`: every character but an ASCII letter or
/// digit becomes `-` (`/home/me/.code` is `-home-me--code`).
pub fn sanitize(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// The absolute path `path` as the file system resolves it, one part at a time: a symbolic
/// link (dangling or not) is replaced by its target, read relative to its directory (at most
/// [`MAX_LINKS`] of them), and `..` goes up from where the parts before it led. From the first
/// part that does not exist on, the rest is taken as written (nothing below it exists, so no
/// link either), refusing a `..` there (what it names depends on what is made). `Err` says why
/// it cannot be resolved: any error but a missing part, too many links, or a relative path.
fn resolve(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("not an absolute path".into());
    }
    let mut real = PathBuf::new();
    // The parts still to walk, the next one last.
    let mut left: Vec<OsString> = Vec::new();
    let push = |left: &mut Vec<OsString>, p: &Path| {
        left.extend(p.components().rev().map(|c| c.as_os_str().to_os_string()));
    };
    push(&mut left, path);
    let mut links = 0;
    let mut missing = false;
    while let Some(part) = left.pop() {
        match Path::new(&part).components().next() {
            // The root, where an absolute link starts again.
            Some(Component::RootDir | Component::Prefix(_)) => {
                real.push(&part);
                continue;
            }
            Some(Component::CurDir) => continue,
            Some(Component::ParentDir) if missing => {
                return Err("a `..` under a part that does not exist".into());
            }
            Some(Component::ParentDir) => {
                real.pop();
                continue;
            }
            _ => {}
        }
        let next = real.join(&part);
        if missing {
            real = next;
            continue;
        }
        match std::fs::symlink_metadata(&next) {
            Ok(m) if m.file_type().is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(format!("more than {MAX_LINKS} symbolic links (a loop?)"));
                }
                let target =
                    std::fs::read_link(&next).map_err(|e| format!("{}: {e}", next.display()))?;
                push(&mut left, &target);
            }
            Ok(_) => real = next,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                missing = true;
                real = next;
            }
            Err(e) => return Err(format!("{}: {e}", next.display())),
        }
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home with a memory directory, a session transcript, and a file elsewhere.
    struct Home {
        dir: tempfile::TempDir,
    }

    impl Home {
        fn new() -> Home {
            let dir = tempfile::tempdir().unwrap();
            let h = Home { dir };
            std::fs::create_dir_all(h.memory()).unwrap();
            std::fs::write(h.memory().join("MEMORY.md"), "- [a](a.md)\n").unwrap();
            std::fs::write(h.root().join("secret.txt"), "no").unwrap();
            h
        }

        fn root(&self) -> PathBuf {
            self.dir.path().canonicalize().unwrap()
        }

        fn project(&self) -> PathBuf {
            self.root().join(".claude/projects/-home-me")
        }

        fn memory(&self) -> PathBuf {
            self.project().join("memory")
        }

        fn call(&self, tool: &str, path: &Path) -> Result<(), String> {
            check(&json!({
                "tool_name": tool,
                "tool_input": {"file_path": path},
                "cwd": self.root(),
                "transcript_path": self.project().join("0f.jsonl"),
            }))
        }
    }

    /// Read, Write and Edit work on files in the memory directory, new ones too, and nowhere
    /// else: not the home directory, not the project's transcripts, not another project's
    /// memory, not the memory directory itself.
    #[test]
    fn only_the_memory_directory_is_open() {
        let h = Home::new();
        for tool in TOOLS {
            for ok in [
                h.memory().join("MEMORY.md"),
                h.memory().join("new-topic.md"),
                h.memory().join("sub/deeper.md"),
            ] {
                assert_eq!(h.call(tool, &ok), Ok(()), "{tool} {}", ok.display());
            }
            for bad in [
                h.root().join("secret.txt"),
                h.project().join("0f.jsonl"),
                h.root().join(".claude/projects/-other/memory/MEMORY.md"),
                h.memory(),
                PathBuf::from("/etc/passwd"),
            ] {
                assert!(h.call(tool, &bad).is_err(), "{tool} {}", bad.display());
            }
        }
        // Other tools are not the guard's to let through.
        assert!(h.call("Bash", &h.memory().join("MEMORY.md")).is_err());
    }

    /// `..`, symbolic links and relative paths are checked where they lead.
    #[test]
    fn paths_are_checked_where_they_lead() {
        let h = Home::new();
        let up = h.memory().join("../../../../secret.txt");
        assert!(h.call("Read", &up).is_err());
        // Inside after resolution: fine.
        let around = h.memory().join("../memory/MEMORY.md");
        assert_eq!(h.call("Edit", &around), Ok(()));
        // A `..` under a part that does not exist yet.
        let unborn = h.memory().join("nope/../../secret.txt");
        assert!(h.call("Write", &unborn).is_err());
        // A link in the memory directory to a file or directory outside.
        std::os::unix::fs::symlink(h.root().join("secret.txt"), h.memory().join("link.md"))
            .unwrap();
        assert!(h.call("Read", &h.memory().join("link.md")).is_err());
        std::os::unix::fs::symlink(h.root(), h.memory().join("out")).unwrap();
        assert!(h.call("Write", &h.memory().join("out/x.md")).is_err());
        // Relative to the working directory.
        assert!(h.call("Read", Path::new("secret.txt")).is_err());
        assert_eq!(
            h.call(
                "Read",
                Path::new(".claude/projects/-home-me/memory/MEMORY.md")
            ),
            Ok(())
        );
    }

    /// What the guard cannot read or place is refused, and the refusal is a `deny` decision
    /// that says why.
    #[test]
    fn what_it_cannot_decide_is_denied() {
        let h = Home::new();
        let deny = |input: &str| {
            let v = answer(input).expect("denied");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert!(deny("not json").contains("unreadable"));
        let base = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": h.memory().join("MEMORY.md")},
            "cwd": h.root(),
            "transcript_path": h.project().join("0f.jsonl"),
        });
        assert_eq!(answer(&base.to_string()), None);
        for (key, why) in [
            ("transcript_path", "transcript_path"),
            ("tool_input", "file_path"),
        ] {
            let mut v = base.clone();
            v.as_object_mut().unwrap().remove(key);
            let reason = deny(&v.to_string());
            assert!(reason.contains(why), "{reason}");
            assert!(reason.contains("leave project files to worker threads"));
        }
        // A transcript that is not under a `projects` directory names no memory directory.
        let mut v = base.clone();
        v["transcript_path"] = json!(h.root().join("0f.jsonl"));
        assert!(answer(&v.to_string()).is_some());
    }

    /// In a git repository the memory directory is the main checkout's, worktrees included.
    #[test]
    fn a_repository_has_one_memory_directory() {
        let h = Home::new();
        let repo = h.root().join("code/app");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        let wt = h.root().join("wt");
        git(&["worktree", "add", "-q", wt.to_str().unwrap()]);
        let key = sanitize(&repo.to_string_lossy());
        let memory = h.root().join(".claude/projects").join(&key).join("memory");
        let project = h
            .root()
            .join(".claude/projects")
            .join(sanitize(&wt.to_string_lossy()));
        let call = |path: &Path| {
            check(&json!({
                "tool_name": "Write",
                "tool_input": {"file_path": path},
                "cwd": wt,
                "transcript_path": project.join("1a.jsonl"),
            }))
        };
        assert_eq!(call(&memory.join("MEMORY.md")), Ok(()));
        assert_eq!(call(&project.join("memory/MEMORY.md")), Ok(()));
        assert!(call(&repo.join("src.rs")).is_err());
        assert_eq!(sanitize("/home/me/.code"), "-home-me--code");
    }

    /// Links are followed where they lead even when nothing is there yet: a dangling link, at
    /// the end or as a directory, is checked at its target; a loop, a part that cannot be
    /// read, `~` and a `..` under a missing part are refused.
    #[test]
    fn dangling_links_loops_and_errors_are_checked() {
        use std::os::unix::fs::symlink;
        let h = Home::new();
        let m = h.memory();
        let outside = h.root().join("outside");
        // A dangling link to a file outside, absolute or relative.
        symlink(outside.join("new.md"), m.join("new.md")).unwrap();
        symlink("../../../../outside/rel.md", m.join("rel.md")).unwrap();
        // A dangling link as a directory.
        symlink(outside.join("dir"), m.join("sub")).unwrap();
        // Dangling links that lead inside are fine.
        symlink("later.md", m.join("soon.md")).unwrap();
        symlink(m.join("notyet"), m.join("alias")).unwrap();
        // A loop.
        symlink(m.join("b"), m.join("a")).unwrap();
        symlink(m.join("a"), m.join("b")).unwrap();
        for tool in TOOLS {
            for bad in [
                m.join("new.md"),
                m.join("rel.md"),
                m.join("sub/x.md"),
                m.join("sub/deeper/x.md"),
                m.join("a"),
                m.join("a/x.md"),
                m.join("nope/../../secret.txt"),
                m.join("../memory/../0f.jsonl"),
                PathBuf::from("/etc/shadow"),
                PathBuf::from("/nonexistent/x.md"),
            ] {
                assert!(h.call(tool, &bad).is_err(), "{tool} {}", bad.display());
            }
            for ok in [m.join("soon.md"), m.join("alias/x.md"), m.join("./x.md")] {
                assert_eq!(h.call(tool, &ok), Ok(()), "{tool} {}", ok.display());
            }
            for tilde in ["~/secret.txt", "~", "~me/x"] {
                let r = h.call(tool, Path::new(tilde));
                assert!(r.as_ref().is_err_and(|e| e.contains('~')), "{tilde}: {r:?}");
            }
        }
        assert!(h
            .call("Write", &m.join("a/x.md"))
            .unwrap_err()
            .contains("symbolic links"));
        // A directory the guard cannot search: refused, not taken as missing.
        let locked = m.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("x.md"), "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root can search it anyway.
        if std::fs::read_dir(&locked).is_err() {
            let r = h.call("Read", &locked.join("x.md"));
            assert!(r.is_err_and(|e| e.contains("cannot be resolved")));
            assert!(h.call("Write", &locked.join("new/y.md")).is_err());
        }
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(h.call("Read", &locked.join("x.md")), Ok(()));
    }

    /// The hook runs the guard through `sh`, its path quoted, with hooks kept on and a timeout
    /// well past the guard's own deadline.
    #[test]
    fn settings_run_the_guard_on_the_file_tools() {
        let s = settings(Path::new("/opt/it's/twr"), None);
        assert_eq!(s["disableAllHooks"], false);
        let hook = &s["hooks"]["PreToolUse"][0];
        assert_eq!(hook["matcher"], "Read|Write|Edit");
        assert_eq!(hook["hooks"][0]["type"], "command");
        assert_eq!(
            hook["hooks"][0]["command"],
            r"'/opt/it'\''s/twr' claude-memory-guard || exit 2"
        );
        assert_eq!(hook["hooks"][0]["timeout"], HOOK_TIMEOUT_SECS);
        assert!(DEADLINE * 2 <= Duration::from_secs(HOOK_TIMEOUT_SECS));
        assert!(s.get("autoMemoryDirectory").is_none(), "{s}");
    }

    /// With a memory directory, the settings move Claude Code's memory there and the hook
    /// command names it to the guard, quoted like the binary.
    #[test]
    fn settings_with_a_memory_directory_name_it_twice() {
        let s = settings(
            Path::new("/opt/twr"),
            Some(Path::new("/h/.claude/projects/-it's/memory")),
        );
        assert_eq!(s["autoMemoryDirectory"], "/h/.claude/projects/-it's/memory");
        let hook = &s["hooks"]["PreToolUse"][0]["hooks"][0];
        assert_eq!(
            hook["command"],
            r"'/opt/twr' claude-memory-guard --memory-dir '/h/.claude/projects/-it'\''s/memory' || exit 2"
        );
        assert_eq!(s["disableAllHooks"], false);
        assert_eq!(s["hooks"]["PreToolUse"][0]["matcher"], "Read|Write|Edit");
    }

    /// `--memory-dir`: exactly that directory is open (no transcript needed), and none of the
    /// derived ones; the directory itself, a link out of it and a relative directory are not.
    #[test]
    fn a_given_memory_directory_replaces_the_derived_ones() {
        let h = Home::new();
        let shared = h.root().join("native/projects/-home-me/memory");
        std::fs::create_dir_all(&shared).unwrap();
        let call = |tool: &str, path: &Path, dir: &Path, transcript: bool| {
            let mut input = json!({
                "tool_name": tool,
                "tool_input": {"file_path": path},
                "cwd": h.root(),
            });
            if transcript {
                input["transcript_path"] = json!(h.project().join("0f.jsonl"));
            }
            check_with(&input, Some(dir))
        };
        for tool in TOOLS {
            for transcript in [true, false] {
                for ok in [
                    shared.join("MEMORY.md"),
                    shared.join("new.md"),
                    shared.join("sub/deeper.md"),
                ] {
                    assert_eq!(
                        call(tool, &ok, &shared, transcript),
                        Ok(()),
                        "{tool} {}",
                        ok.display()
                    );
                }
                for bad in [
                    // The derived one (the transcript's project) is no longer open.
                    h.memory().join("MEMORY.md"),
                    shared.clone(),
                    h.root().join("secret.txt"),
                    shared.join("../../../secret.txt"),
                ] {
                    assert!(
                        call(tool, &bad, &shared, transcript).is_err(),
                        "{tool} {}",
                        bad.display()
                    );
                }
            }
        }
        // A directory that does not exist yet is checked as written.
        let unborn = h.root().join("native/projects/-new/memory");
        assert_eq!(
            call("Write", &unborn.join("MEMORY.md"), &unborn, false),
            Ok(())
        );
        // A link out of it.
        std::os::unix::fs::symlink(h.root().join("secret.txt"), shared.join("out.md")).unwrap();
        assert!(call("Read", &shared.join("out.md"), &shared, false).is_err());
        // A relative directory allows nothing.
        assert!(call(
            "Read",
            &shared.join("MEMORY.md"),
            Path::new("native/projects/-home-me/memory"),
            false
        )
        .is_err());
        // Without it, the transcript is needed and the derived directory is the one.
        let derived = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": h.memory().join("MEMORY.md")},
            "cwd": h.root(),
            "transcript_path": h.project().join("0f.jsonl"),
        });
        assert_eq!(check_with(&derived, None), Ok(()));
        let mut v = derived.clone();
        v["tool_input"]["file_path"] = json!(shared.join("MEMORY.md"));
        assert!(check_with(&v, None).is_err());
        // Through the hook's answer and run as well.
        assert_eq!(answer_with(&derived.to_string(), None), None);
        assert!(answer_with(&derived.to_string(), Some(&shared)).is_some());
        let input = v.to_string();
        assert_eq!(run_with(move || Ok(input), DEADLINE, Some(shared)), None);
    }

    /// The memory directory of a session: the main checkout's key in a repository (worktrees
    /// included), else the directory's own; none for a key Claude Code would hash.
    #[test]
    fn a_project_memory_directory_follows_the_main_checkout() {
        let h = Home::new();
        let config = Path::new("/n/.claude");
        let plain = h.root().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        assert_eq!(
            project_memory_dir(config, &plain),
            Ok(config
                .join("projects")
                .join(sanitize(&plain.to_string_lossy()))
                .join("memory"))
        );
        let repo = h.root().join("code/app");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "x",
        ]);
        let wt = h.root().join("wt");
        git(&["worktree", "add", "-q", wt.to_str().unwrap()]);
        let want = config
            .join("projects")
            .join(sanitize(&repo.to_string_lossy()))
            .join("memory");
        for cwd in [repo.clone(), repo.join("src"), wt] {
            assert_eq!(
                project_memory_dir(config, &cwd).as_ref(),
                Ok(&want),
                "{}",
                cwd.display()
            );
        }
        let long = h.root().join("x".repeat(MAX_KEY));
        assert!(project_memory_dir(config, &long).is_err_and(|e| e.contains("longer than")));
    }

    /// A configuration directory reached through symbolic links (to it, or in it to
    /// `projects` or the project's directory) gives the real memory directory, the parts that
    /// do not exist yet kept as written; the guard allows a write in it.
    #[test]
    fn a_project_memory_directory_is_the_real_one() {
        let h = Home::new();
        let cwd = h.root().join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let key = sanitize(&cwd.to_string_lossy());
        let real = h.root().join("dotfiles/claude");
        std::fs::create_dir_all(&real).unwrap();
        let link = h.root().join("link-claude");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // Nothing below the configuration directory exists yet.
        let want = real.join("projects").join(&key).join("memory");
        assert_eq!(project_memory_dir(&link, &cwd), Ok(want.clone()));
        // `projects` a link elsewhere, the project's directory a relative link in it.
        let projects = h.root().join("store/projects");
        std::fs::create_dir_all(projects.join("real-key")).unwrap();
        std::os::unix::fs::symlink(&projects, real.join("projects")).unwrap();
        std::os::unix::fs::symlink("real-key", projects.join(&key)).unwrap();
        let want = projects.join("real-key/memory");
        let got = project_memory_dir(&link, &cwd).unwrap();
        assert_eq!(got, want);
        let write = json!({
            "tool_name": "Write",
            "tool_input": {"file_path": got.join("MEMORY.md")},
            "cwd": cwd,
        });
        assert_eq!(check_with(&write, Some(&got)), Ok(()));
    }

    /// A `.git`, or a worktree's `commondir`, that is a FIFO (no writer: a blocking read
    /// would wait forever) makes its directory no checkout, at once: the memory directory is
    /// the directory's own. Run on a thread of its own, so a read that blocks fails the test
    /// instead of hanging it.
    #[test]
    fn git_files_that_are_fifos_are_no_checkout() {
        let h = Home::new();
        let mkfifo = |p: &Path| {
            let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        };
        // `.git` itself a FIFO.
        let fifo_git = h.root().join("fifo-git");
        std::fs::create_dir_all(&fifo_git).unwrap();
        mkfifo(&fifo_git.join(".git"));
        // A worktree's `.git` file names a git directory whose `commondir` is a FIFO.
        let fifo_common = h.root().join("fifo-commondir");
        let gitdir = h.root().join("repo/.git/worktrees/w");
        std::fs::create_dir_all(&fifo_common).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(
            fifo_common.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();
        mkfifo(&gitdir.join("commondir"));
        let config = PathBuf::from("/n/.claude");
        let (tx, rx) = std::sync::mpsc::channel();
        let dirs = [fifo_git.clone(), fifo_common.clone()];
        let c = config.clone();
        std::thread::spawn(move || {
            let got: Vec<_> = dirs
                .iter()
                .map(|d| (git_root(d), project_memory_dir(&c, d)))
                .collect();
            let _ = tx.send(got);
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reading a FIFO in git's files blocked");
        for ((root, memory), dir) in got.into_iter().zip([fifo_git, fifo_common]) {
            assert_eq!(root, None, "{}", dir.display());
            let own = config
                .join("projects")
                .join(sanitize(&dir.to_string_lossy()))
                .join("memory");
            assert_eq!(memory, Ok(own), "{}", dir.display());
        }
    }

    /// Managed settings files that turn hooks off, allow only managed hooks, or cannot be read
    /// keep the hook from running; others do not.
    #[test]
    fn managed_settings_that_stop_hooks_are_seen() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        assert_eq!(hooks_blocked_in(d), None);
        assert_eq!(hooks_blocked_in(&d.join("absent")), None);
        std::fs::write(
            d.join("managed-settings.json"),
            r#"{"disableAllHooks": false}"#,
        )
        .unwrap();
        assert_eq!(hooks_blocked_in(d), None);
        std::fs::create_dir(d.join("managed-settings.d")).unwrap();
        std::fs::write(
            d.join("managed-settings.d/.hidden.json"),
            r#"{"disableAllHooks": true}"#,
        )
        .unwrap();
        std::fs::write(d.join("managed-settings.d/10-a.txt"), "not json").unwrap();
        assert_eq!(hooks_blocked_in(d), None);
        std::fs::write(
            d.join("managed-settings.d/20-hooks.json"),
            r#"{"allowManagedHooksOnly": true}"#,
        )
        .unwrap();
        assert!(hooks_blocked_in(d).is_some_and(|r| r.contains("allowManagedHooksOnly")));
        std::fs::remove_file(d.join("managed-settings.d/20-hooks.json")).unwrap();
        std::fs::write(
            d.join("managed-settings.json"),
            r#"{"disableAllHooks": true}"#,
        )
        .unwrap();
        assert!(hooks_blocked_in(d).is_some_and(|r| r.contains("disableAllHooks")));
        std::fs::write(d.join("managed-settings.json"), "{").unwrap();
        assert!(hooks_blocked_in(d).is_some_and(|r| r.contains("not JSON")));
        // A FIFO (no writer) or an oversized file is not read, at once: it blocks.
        std::fs::remove_file(d.join("managed-settings.json")).unwrap();
        let fifo = d.join("managed-settings.d/30-fifo.json");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert_eq!(
            hooks_blocked_in(d),
            Some(format!(
                "{} can't be read: it is a FIFO, not a regular file",
                fifo.display()
            ))
        );
        std::fs::remove_file(&fifo).unwrap();
        let big = d.join("managed-settings.json");
        std::fs::File::create(&big)
            .unwrap()
            .set_len(twr_core::fsread::SETTINGS_MAX + 1)
            .unwrap();
        assert_eq!(
            hooks_blocked_in(d),
            Some(format!(
                "{} can't be read: it is larger than 1024 KiB",
                big.display()
            ))
        );
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    /// A managed policy key lets the hook run only when absent, `null`, a plain `false`, or
    /// (`strictPluginOnlyCustomization`) a list of surfaces without `hooks`; quoted booleans,
    /// `true`, a list naming hooks and invalid values all block it.
    #[test]
    fn managed_hook_policies_block_unless_plainly_off() {
        for key in HOOK_POLICY_KEYS {
            for open in [json!(null), json!(false)] {
                assert!(!blocks_hooks(key, &open), "{key}: {open}");
            }
            for shut in [
                json!(true),
                json!("true"),
                json!("false"),
                json!("yes"),
                json!(1),
                json!(0),
                json!({}),
                json!(["hooks"]),
            ] {
                assert!(blocks_hooks(key, &shut), "{key}: {shut}");
            }
        }
        let strict = "strictPluginOnlyCustomization";
        for open in [
            json!([]),
            json!(["skills", "agents", "mcp"]),
            json!(["skill"]),
        ] {
            assert!(!blocks_hooks(strict, &open), "{open}");
        }
        for shut in [
            json!(["skills", "hooks"]),
            json!(["HOOKS"]),
            json!([" hooks"]),
            json!(["skills", 3]),
            json!([null]),
        ] {
            assert!(blocks_hooks(strict, &shut), "{shut}");
        }
        // Only that key takes a list of surfaces.
        assert!(blocks_hooks("allowManagedHooksOnly", &json!([])));
        // As read from a file.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("managed-settings.json");
        for (text, blocked) in [
            (r#"{"strictPluginOnlyCustomization": ["hooks"]}"#, true),
            (r#"{"strictPluginOnlyCustomization": true}"#, true),
            (r#"{"strictPluginOnlyCustomization": "skills"}"#, true),
            (r#"{"allowManagedHooksOnly": "true"}"#, true),
            (r#"{"disableAllHooks": "true"}"#, true),
            (r#"{"strictPluginOnlyCustomization": ["skills"]}"#, false),
            (
                r#"{"allowManagedHooksOnly": false, "disableAllHooks": null}"#,
                false,
            ),
        ] {
            std::fs::write(&file, text).unwrap();
            assert_eq!(hooks_blocked_in(dir.path()).is_some(), blocked, "{text}");
        }
    }

    /// A managed settings file that hands the policy to a helper program (`policyHelper`,
    /// `policyHelpers`, any value but `null`) makes the hook policy unknown, and so do
    /// server-managed settings Claude Code keeps (`remote-settings.json`), which outrank the
    /// files.
    #[test]
    fn policy_helpers_and_server_managed_settings_make_the_policy_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("managed-settings.json");
        for (text, blocked) in [
            (
                r#"{"policyHelper": {"path": "/usr/local/bin/policy"}}"#,
                true,
            ),
            (
                r#"{"allowManagedHooksOnly": false, "policyHelpers": {"linux": {"path": "/p"}}}"#,
                true,
            ),
            (r#"{"policyHelpers": {}}"#, true),
            (r#"{"policyHelper": "nonsense"}"#, true),
            (r#"{"policyHelper": null, "policyHelpers": null}"#, false),
        ] {
            std::fs::write(&file, text).unwrap();
            let r = hooks_blocked_in(dir.path());
            assert_eq!(r.is_some(), blocked, "{text}: {r:?}");
            if blocked {
                assert!(r.unwrap().contains("policyHelper"), "{text}");
            }
        }
        // In a drop-in too.
        std::fs::write(&file, "{}").unwrap();
        std::fs::create_dir(dir.path().join("managed-settings.d")).unwrap();
        std::fs::write(
            dir.path().join("managed-settings.d/50-helper.json"),
            r#"{"policyHelpers": {"default": {}}}"#,
        )
        .unwrap();
        assert!(hooks_blocked_in(dir.path()).is_some());

        let config = tempfile::tempdir().unwrap();
        assert_eq!(remote_policy_in(config.path()), None);
        std::fs::write(config.path().join("remote-settings.json"), "{}").unwrap();
        let r = remote_policy_in(config.path());
        assert!(r.is_some_and(|r| r.contains("server-managed")));
    }

    /// The policy is read where the sources say: none there lets the hook run, a blocking
    /// managed file or a server-managed settings cache there keeps it from running.
    #[test]
    fn the_policy_is_read_from_its_sources() {
        let dir = tempfile::tempdir().unwrap();
        let sources = PolicySources {
            managed_dir: Some(dir.path().join("managed")),
            config_dir: Some(dir.path().join("config")),
        };
        assert_eq!(policy_blocks_hooks(&sources), None);
        assert_eq!(policy_blocks_hooks(&PolicySources::default()), None);
        std::fs::create_dir_all(dir.path().join("managed")).unwrap();
        std::fs::write(
            dir.path().join("managed/managed-settings.json"),
            r#"{"allowManagedHooksOnly": true}"#,
        )
        .unwrap();
        assert!(policy_blocks_hooks(&sources).is_some_and(|r| r.contains("allowManagedHooksOnly")));
        std::fs::remove_file(dir.path().join("managed/managed-settings.json")).unwrap();
        std::fs::create_dir_all(dir.path().join("config")).unwrap();
        std::fs::write(dir.path().join("config/remote-settings.json"), "{}").unwrap();
        assert!(policy_blocks_hooks(&sources).is_some_and(|r| r.contains("server-managed")));
        // This system's sources are where Claude Code keeps them.
        let system = PolicySources::system();
        if cfg!(target_os = "linux") {
            assert_eq!(
                system.managed_dir.as_deref(),
                Some(Path::new("/etc/claude-code"))
            );
        }
    }

    /// The guard denies what it has not decided by its deadline, an input it cannot read and
    /// a panic; it answers as [`answer`] does otherwise.
    #[test]
    fn a_slow_guard_denies() {
        let reason = |v: Option<Value>| {
            let v = v.expect("denied");
            assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
            v["hookSpecificOutput"]["permissionDecisionReason"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let start = std::time::Instant::now();
        let slow = || {
            std::thread::sleep(Duration::from_secs(30));
            Ok(String::new())
        };
        assert!(reason(run(slow, Duration::from_millis(100))).contains("within 100 ms"));
        assert!(start.elapsed() < Duration::from_secs(10));
        let broken = || Err(std::io::Error::other("boom"));
        assert!(reason(run(broken, DEADLINE)).contains("boom"));
        let panics = || -> std::io::Result<String> { panic!("guard bug") };
        assert!(reason(run(panics, DEADLINE)).contains("failed"));
        assert!(reason(run(|| Ok("not json".into()), DEADLINE)).contains("unreadable"));
        let h = Home::new();
        let input = json!({
            "tool_name": "Read",
            "tool_input": {"file_path": h.memory().join("MEMORY.md")},
            "cwd": h.root(),
            "transcript_path": h.project().join("0f.jsonl"),
        })
        .to_string();
        assert_eq!(run(move || Ok(input), DEADLINE), None);
        // The environment shortens the deadline, never lengthens it.
        assert!(deadline() <= DEADLINE);
    }
}
