//! What the installed `claude` supports, read from `claude --help` and `claude --version`.
//!
//! Claude Code's options change between versions: `--permission-prompts` first appeared in
//! 2.1.259 (and defaults to `host` there), and before 2.1.200 the `manual` permission mode was
//! called `default`. The adapter reads both outputs before the first spawn of a binary and
//! caches what it found per binary and `PATH` (its resolved path, size and modification time,
//! so a `claude update` is noticed, and the `PATH` it was probed with: a wrapper such as
//! `exec claude-real "$@"` runs another `claude` on another `PATH`), then:
//!
//! - passes `--permission-prompts host` only when `--help` lists it (`--permission-prompt-tool
//!   stdio` is always passed: it routes prompts to twr in every version);
//! - refuses a version older than [`MIN_VERSION`], or one whose `--help` lacks an option the
//!   spawn passes, with what to do ("`claude` 2.1.100 is too old for twr (needs 2.1.110 or
//!   newer): run `claude update`");
//! - passes `manual` as `default` to a version that only knows `default` (the same mode), and
//!   refuses a permission mode the version does not list (never a wider one instead);
//! - passes `--effort` only for a thread with a reasoning effort, and refuses a version whose
//!   `--help` does not list it with what to do ([`effort_refusal`], which the hub also asks
//!   before it records an effort for a local Claude Code thread, from the cache only).
//!
//! What is found describes this box's binary only: the interactive attach fits its command to
//! the cache (never probing), and the hub's attach to a thread on another box uses none of it
//! (`ClaudeCodeAdapter::remote_attach_command`); a runner probes its own box's binary. The
//! binary probed is the one the spawn runs: resolved as the spawn resolves it
//! ([`crate::resolve::resolve`]: on the runtime's `PATH`, its thread's or the one a runner
//! gave it, else the hub's), and `--help` / `--version` run with that `PATH` too (a Node
//! script finds its `node` there), in the runtime's working directory. A `PATH` with a
//! relative or empty entry (`bin:/usr/bin`) finds programs from that directory, so its results
//! are kept per directory as well; with an absolute `PATH` threads in every directory share
//! one result per binary.
//!
//! When `--help` cannot be read (it cannot be run, exits non-zero without listing options, or
//! takes longer than [`PROBE_TIMEOUT`]) nothing is guessed: the adapter passes every option,
//! and a version that rejects one fails with its own error, to which the driver adds why
//! `--help` was not read and the update hint ([`refused_options_hint`]). Only a definite answer
//! is cached (`--help` and `--version` both exited within the limit, and `--help` listed its
//! options or exited 0); anything else is probed again by the next spawn. One probe of a
//! binary with a `PATH` (and directory, as above) runs at a time: spawns arriving while it
//! runs wait for its result, and it is killed when no spawn waits for it any more.

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, SystemTime};

use tokio::sync::watch;

use crate::process;
use crate::resolve::Resolved;

/// The oldest Claude Code twr works with: the oldest version checked against everything the
/// adapter uses (its options, the permission modes, permission prompts over stdio, allow with
/// `updatedInput`, deny, interrupt, resume and the orchestrator's `dontAsk` session).
pub const MIN_VERSION: Version = Version(2, 1, 110);

/// How long `claude --help` (and `--version`) may take before the adapter stops waiting.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The option routing permission prompts to the host (Claude Code 2.1.259 and newer).
pub const PERMISSION_PROMPTS: &str = "--permission-prompts";

/// The option setting the session's reasoning effort (listed by Claude Code 2.1.288's `--help`
/// as "Effort level for the current session (low, medium, high, xhigh, max)"; passed only for
/// a thread with an effort, and refused with the update hint by a version whose `--help` does
/// not list it: [`effort_refusal`]).
pub const EFFORT: &str = "--effort";

/// Disable recorded prompts when supported, so orchestrator instructions can be updated.
pub const SYSTEM_PROMPT_SNAPSHOT: &str = "--system-prompt-snapshot";

/// Options the adapter passes that `--help` does not list (in any version).
const UNLISTED: &[&str] = &["--permission-prompt-tool", PERMISSION_PROMPTS];

/// Options the adapter passes that take no value.
const SWITCHES: &[&str] = &[
    "--verbose",
    "--include-partial-messages",
    "--strict-mcp-config",
];

/// A Claude Code version (`major.minor.patch`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    /// The version `claude --version` prints (`2.1.197 (Claude Code)`): its first word, which
    /// must be `major.minor.patch`.
    pub fn parse(s: &str) -> Option<Version> {
        let word = s.split_whitespace().next()?;
        let mut parts = word.split('.').map(|p| p.parse::<u32>().ok());
        let v = Version(parts.next()??, parts.next()??, parts.next()??);
        parts.next().is_none().then_some(v)
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// What one `claude` binary supports.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Support {
    /// From `--version`, when it could be read.
    pub version: Option<Version>,
    /// The long options `--help` lists (`--allowedTools` and its alias `--allowed-tools`
    /// both). Empty when `--help` could not be read or parsed.
    pub options: BTreeSet<String>,
    /// The `--permission-mode` choices `--help` lists, if it lists them.
    pub modes: Option<Vec<String>>,
}

impl Support {
    /// Reads `claude --help` output and, if there is one, `claude --version` output.
    ///
    /// An option line starts with `-` after at most three spaces (`  -r, --resume [value]`);
    /// the lines after it that are indented deeper are its description (Claude Code wraps it
    /// at 80 columns in some versions, not in others).
    pub fn parse(help: &str, version: Option<&str>) -> Support {
        let mut options = BTreeSet::new();
        let mut modes = None;
        for block in option_blocks(help) {
            let term = block[0].trim().split("  ").next().unwrap_or_default();
            let names: Vec<&str> = term
                .split([',', ' '])
                .filter(|w| w.starts_with("--") && w.len() > 2)
                .collect();
            if names.contains(&"--permission-mode") {
                modes = choices(&block.join(" "));
            }
            options.extend(names.into_iter().map(str::to_string));
        }
        Support {
            version: version.and_then(Version::parse),
            options,
            modes,
        }
    }

    /// Whether `--help` lists `option`.
    pub fn lists(&self, option: &str) -> bool {
        self.options.contains(option)
    }
}

/// The option blocks of a help text: each option line with its deeper-indented continuation
/// lines.
fn option_blocks(help: &str) -> Vec<Vec<&str>> {
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    let mut open = false;
    for line in help.lines() {
        let indent = line.len() - line.trim_start().len();
        let text = line.trim_start();
        if text.is_empty() {
            open = false;
        } else if indent < 4 && text.starts_with('-') {
            blocks.push(vec![line]);
            open = true;
        } else if open && indent >= 4 {
            if let Some(b) = blocks.last_mut() {
                b.push(line);
            }
        } else {
            open = false;
        }
    }
    blocks
}

/// The quoted values of a `(choices: "a", "b", default: "a")` list in `text`.
fn choices(text: &str) -> Option<Vec<String>> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let start = text.find("(choices:")? + "(choices:".len();
    let rest = &text[start..];
    let end = [rest.find(')'), rest.find("default:")]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(rest.len());
    let list: Vec<String> = rest[..end]
        .split(',')
        .map(|c| c.trim().trim_matches('"').to_string())
        .filter(|c| !c.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

/// How to start `claude` for one spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// Whether the CLI supports disabling the system prompt snapshot.
    pub system_prompt_snapshot: bool,
    /// Whether to pass `--permission-prompts host`.
    pub permission_prompts: bool,
    /// The `--permission-mode` to pass.
    pub mode: &'static str,
}

/// How to start `program` (shown in errors as given) for a spawn in permission mode `mode`
/// with arguments `args` (as [`super::build_args`] makes them), given what it supports
/// (`None`: unknown). An error says why it cannot be used and what to do, program first.
pub fn plan(
    program: &Path,
    support: Option<&Support>,
    mode: &'static str,
    args: &[String],
) -> Result<Plan, String> {
    let Some(s) = support else {
        return Ok(Plan {
            system_prompt_snapshot: false,
            permission_prompts: true,
            mode,
        });
    };
    let shown = program.display();
    let at = s.version.map(|v| format!(" {v}")).unwrap_or_default();
    if let Some(v) = s.version.filter(|v| *v < MIN_VERSION) {
        return Err(format!(
            "`{shown}` {v} is too old for twr (needs {MIN_VERSION} or newer): run `claude update`"
        ));
    }
    if s.options.is_empty() {
        // `--help` said nothing usable: pass everything (see the module docs).
        return Ok(Plan {
            system_prompt_snapshot: false,
            permission_prompts: true,
            mode,
        });
    }
    if let Some(missing) = options_in(args)
        .into_iter()
        .find(|o| *o != SYSTEM_PROMPT_SNAPSHOT && !UNLISTED.contains(o) && !s.lists(o))
    {
        // The effort is a thread setting newer than the oldest version twr takes: say whose
        // it is and how to clear it, before the update hint.
        if missing == EFFORT {
            if let Some(why) = effort_refusal(program, s) {
                return Err(why);
            }
        }
        return Err(format!(
            "`{shown}`{at} does not support {missing} (twr needs Claude Code {MIN_VERSION} or \
             newer): run `claude update`"
        ));
    }
    // The mode may be one the user never chose (twr's default, `auto`): say whose it is and
    // how to pick another, before the update hint.
    let mode = mode_for(s, mode).map_err(|m| {
        format!(
            "`{shown}`{at} does not support permission mode {m:?} (the thread's mode; `twr mode \
             <thread> <mode>` changes it, `[threads] default_mode` sets it for new threads): run \
             `claude update`"
        )
    })?;
    Ok(Plan {
        system_prompt_snapshot: s.lists(SYSTEM_PROMPT_SNAPSHOT),
        permission_prompts: s.lists(PERMISSION_PROMPTS),
        mode,
    })
}

/// Why `program` (shown as given), supporting `s`, cannot take a reasoning effort: its `--help`
/// lists options but not [`EFFORT`]. `None` when it lists it, or when its `--help` said
/// nothing usable (every option is passed then, see the module docs).
pub fn effort_refusal(program: &Path, s: &Support) -> Option<String> {
    if s.options.is_empty() || s.lists(EFFORT) {
        return None;
    }
    let at = s.version.map(|v| format!(" {v}")).unwrap_or_default();
    Some(format!(
        "`{}`{at} does not support {EFFORT} (the thread's reasoning effort; `twr model <thread> \
         --effort default` clears it): run `claude update`",
        program.display()
    ))
}

/// The `--permission-mode` to pass for `mode` to a binary supporting `s`: `mode` itself when
/// listed (or when the list is unknown); `default` as written (newer versions list only its
/// new name, `manual`, and still take `default`); `manual` as `default` where only that is
/// listed (Claude Code before 2.1.200: the same mode under its old name). Anything else is not
/// supported (the error is the mode): no other mode stands in for it.
pub fn mode_for(s: &Support, mode: &'static str) -> Result<&'static str, &'static str> {
    let Some(modes) = &s.modes else {
        return Ok(mode);
    };
    let listed = |m: &str| modes.iter().any(|x| x == m);
    match mode {
        m if listed(m) => Ok(m),
        "default" => Ok("default"),
        "manual" if listed("default") => Ok("default"),
        m => Err(m),
    }
}

/// The long options in `args` (every option but [`SWITCHES`] takes one value, skipped).
fn options_in(args: &[String]) -> Vec<&str> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if !a.starts_with("--") {
            continue;
        }
        out.push(a.as_str());
        if !SWITCHES.contains(&a.as_str()) {
            it.next();
        }
    }
    out
}

/// The lines the driver adds to the stderr tail of a `claude` that refused its command line
/// (an unknown option, or a value it does not accept), so the crash says what to do; `None`
/// when `stderr` shows no such refusal. `unread` is why its `--help` was not read (the
/// adapter then passed every option): said first, the update hint stays the last line.
pub fn refused_options_hint(stderr: &str, program: &Path, unread: Option<&str>) -> Option<String> {
    let line = stderr.lines().rev().find(|l| {
        l.contains("unknown option") || (l.contains("argument '") && l.contains("is invalid"))
    })?;
    let shown = program.display();
    let why = unread
        .map(|why| {
            format!(
                "twr could not read `{shown} --help` ({why}), so it passed every option; the \
                 next start reads it again\n"
            )
        })
        .unwrap_or_default();
    Some(format!(
        "{why}`{shown}` refused twr's command line ({}): run `claude update` (twr needs Claude \
         Code {MIN_VERSION} or newer)",
        line.trim()
    ))
}

// ---------------------------------------------------------------------------------------------
// Probe and cache
// ---------------------------------------------------------------------------------------------

/// Which binary a probe result belongs to, run with which `PATH`: a different path, size or
/// modification time is a different binary (`claude update` replaces it, or moves the `claude`
/// symlink), and a different `PATH` may run a different program through the same one (a
/// `TWR_CLAUDE_BIN` wrapper running `exec claude-real "$@"` finds `claude-real` there). A
/// `PATH` with a relative or empty entry finds it from the working directory too, so there
/// the directory is part of it as well.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BinaryId {
    path: PathBuf,
    /// The `PATH` the probe runs with, the one the spawn gets: the runtime's
    /// ([`Resolved::path`]), else this process's; `None` when there is none.
    search: Option<std::ffi::OsString>,
    /// The runtime's working directory when `search` has a relative or empty entry
    /// ([`relative_entry`]); `None` otherwise (threads in other directories share the result).
    dir: Option<PathBuf>,
    len: u64,
    modified: Option<SystemTime>,
}

/// What results and probes are kept by: a binary's resolved path, the `PATH` it runs with and,
/// when that `PATH` depends on it, the working directory ([`BinaryId`]; its size and
/// modification time tell a result that is out of date).
type Key = (PathBuf, Option<std::ffi::OsString>, Option<PathBuf>);

impl BinaryId {
    fn key(&self) -> Key {
        (self.path.clone(), self.search.clone(), self.dir.clone())
    }
}

/// Whether `PATH` `search` has an entry found from the working directory: a relative one
/// (`bin`) or an empty one (the directory itself).
fn relative_entry(search: Option<&std::ffi::OsStr>) -> bool {
    search.is_some_and(|s| {
        s.is_empty()
            || std::env::split_paths(s).any(|d| d.as_os_str().is_empty() || d.is_relative())
    })
}

/// What the adapter knows of a binary's `--help`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Help {
    /// Read: what the binary supports (no options when `--help` exited 0 listing none).
    Read(Support),
    /// Not read, and why (`it took longer than 10s`): every option is passed, and a refused
    /// command line says so ([`refused_options_hint`]).
    Unread(String),
}

impl Help {
    /// What the binary supports, if its `--help` was read.
    pub fn support(&self) -> Option<&Support> {
        match self {
            Help::Read(s) => Some(s),
            Help::Unread(_) => None,
        }
    }

    /// Why its `--help` was not read, if it was not.
    pub fn unread(&self) -> Option<&str> {
        match self {
            Help::Read(_) => None,
            Help::Unread(why) => Some(why),
        }
    }
}

/// A probe in flight, shared by the spawns waiting for it.
#[derive(Debug)]
struct Flight {
    id: BinaryId,
    /// The working directory the probe runs in: the first waiting spawn's (an empty one: this
    /// process's).
    cwd: PathBuf,
    /// Its result, once there is one. Each waiting spawn holds a receiver; the probe is killed
    /// when there are none.
    done: watch::Sender<Option<Help>>,
}

/// Definite probe results (see the module docs) and the probes in flight, by resolved path,
/// `PATH` and, for a `PATH` with a relative entry, working directory ([`Key`]).
#[derive(Debug, Default)]
struct Cache {
    known: HashMap<Key, (BinaryId, Support)>,
    flying: HashMap<Key, Arc<Flight>>,
}

static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

fn cache() -> MutexGuard<'static, Cache> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// What is known about a binary before spawning it.
#[derive(Debug)]
pub enum Lookup {
    /// Cached, or not worth probing (it could not be located: spawning it reports that).
    Known(Help),
    /// Being probed: [`Probe::wait`] for the result.
    Probe(Probe),
}

/// A spawn waiting for the probe of its binary. Dropping it stops waiting; the probe is killed
/// once no spawn waits for it.
#[derive(Debug)]
pub struct Probe(watch::Receiver<Option<Help>>);

impl Probe {
    /// The probe's result.
    pub async fn wait(mut self) -> Help {
        match self.0.wait_for(Option::is_some).await {
            Ok(help) => match &*help {
                Some(h) => h.clone(),
                None => Help::Unread("its probe ended without a result".into()),
            },
            Err(_) => Help::Unread("its probe was dropped".into()),
        }
    }
}

/// What is known about the binary `bin` resolves to ([`crate::resolve::resolve`], as the
/// spawn in working directory `cwd` resolves it), run with the `PATH` the spawn gets. Unless
/// its answer for that binary and `PATH` (and `cwd`, when that `PATH` has a relative entry) is
/// cached, waits for the probe in flight or starts one (each run bounded by `limit`, with that
/// `PATH`, in `cwd`); call it in a tokio runtime.
pub fn lookup(bin: &Resolved, cwd: &Path, limit: Duration) -> Lookup {
    let Some(id) = identify(bin, cwd) else {
        return Lookup::Known(Help::Unread("it could not be located".into()));
    };
    let key = id.key();
    let mut cache = cache();
    match cache.known.get(&key) {
        Some((known, support)) if *known == id => {
            return Lookup::Known(Help::Read(support.clone()))
        }
        _ => {}
    }
    if let Some(f) = cache.flying.get(&key).filter(|f| f.id == id) {
        return Lookup::Probe(Probe(f.done.subscribe()));
    }
    let (done, waiting) = watch::channel(None);
    let flight = Arc::new(Flight {
        id,
        cwd: cwd.to_path_buf(),
        done,
    });
    cache.flying.insert(key, Arc::clone(&flight));
    tokio::spawn(fly(flight, limit));
    Lookup::Probe(Probe(waiting))
}

/// The cached support of the binary `bin` resolves to, run with the `PATH` `bin` gives in
/// working directory `cwd`, if it was probed as it is now with that `PATH` (and in `cwd`, when
/// that `PATH` has a relative entry; never probes).
pub fn cached(bin: &Resolved, cwd: &Path) -> Option<Support> {
    let id = identify(bin, cwd)?;
    match cache().known.get(&id.key()) {
        Some((known, support)) if *known == id => Some(support.clone()),
        _ => None,
    }
}

/// The binary `bin` resolved to, as it is now, with the `PATH` it runs with (`bin`'s, else
/// this process's, as the spawn inherits it) and, when that `PATH` has a relative entry, the
/// working directory `cwd` (an empty one: this process's, as the spawn inherits it): `None`
/// when none was found.
fn identify(bin: &Resolved, cwd: &Path) -> Option<BinaryId> {
    let path = std::fs::canonicalize(bin.found.as_ref()?).ok()?;
    let meta = std::fs::metadata(&path).ok()?;
    let search = bin.path.clone().or_else(|| std::env::var_os("PATH"));
    let dir = relative_entry(search.as_deref()).then(|| {
        if cwd.as_os_str().is_empty() {
            std::env::current_dir().unwrap_or_default()
        } else {
            cwd.to_path_buf()
        }
    });
    Some(BinaryId {
        path,
        search,
        dir,
        len: meta.len(),
        modified: meta.modified().ok(),
    })
}

/// Runs the probe of `flight`, caches a definite result and hands the result to every spawn
/// waiting for it; kills the probe if they all stop waiting first.
async fn fly(flight: Arc<Flight>, limit: Duration) {
    let _landed = Landed(Arc::clone(&flight));
    let run = probe(&flight.id, &flight.cwd, limit);
    tokio::pin!(run);
    let (help, definite) = loop {
        tokio::select! {
            found = &mut run => break found,
            () = flight.done.closed() => {
                // Checked under the lock, so no spawn starts waiting for a probe being dropped.
                let mut cache = cache();
                if flight.done.receiver_count() == 0 {
                    land(&mut cache, &flight);
                    tracing::debug!(
                        program = %flight.id.path.display(),
                        "claude-code: no spawn waits for the CLI probe any more; killing it"
                    );
                    return;
                }
            }
        }
    };
    let mut cache = cache();
    land(&mut cache, &flight);
    if let (true, Help::Read(s)) = (definite, &help) {
        cache
            .known
            .insert(flight.id.key(), (flight.id.clone(), s.clone()));
    }
    flight.done.send_replace(Some(help));
}

/// Takes `flight` out of the probes in flight (if a newer probe did not replace it).
fn land(cache: &mut Cache, flight: &Arc<Flight>) {
    let key = flight.id.key();
    if cache
        .flying
        .get(&key)
        .is_some_and(|f| Arc::ptr_eq(f, flight))
    {
        cache.flying.remove(&key);
    }
}

/// [`land`]s a flight however its task ends (a runtime shutting down drops it mid-probe).
struct Landed(Arc<Flight>);

impl Drop for Landed {
    fn drop(&mut self) {
        land(&mut cache(), &self.0);
    }
}

/// Runs `--help` and `--version` of binary `id` in the runtime's working directory `cwd`
/// (each bounded by `limit`, with its `PATH`, [`BinaryId::search`]): what it supports, and
/// whether that is definite (worth caching): `--help` was read and `--version` exited within
/// the limit, whatever its code.
async fn probe(id: &BinaryId, cwd: &Path, limit: Duration) -> (Help, bool) {
    let path = id.search.as_deref();
    let (help, version) = tokio::join!(
        run_for_output(&id.path, "--help", path, cwd, limit),
        run_for_output(&id.path, "--version", path, cwd, limit)
    );
    let (version, version_known) = match &version {
        Ran::Exited(0, out) => (Some(out.as_str()), true),
        Ran::Exited(..) => (None, true),
        Ran::Failed(_) => (None, false),
    };
    let help = match help {
        Ran::Exited(code, out) => {
            // A non-zero exit still counts when it listed the options.
            let s = Support::parse(&out, version);
            if code == 0 || !s.options.is_empty() {
                Help::Read(s)
            } else {
                Help::Unread(format!("it exited with code {code}"))
            }
        }
        Ran::Failed(why) => Help::Unread(why),
    };
    let definite = matches!(help, Help::Read(_)) && version_known;
    match &help {
        Help::Read(s) => tracing::info!(
            program = %id.path.display(),
            version = ?s.version.map(|v| v.to_string()),
            permission_prompts = s.lists(PERMISSION_PROMPTS),
            cached = definite,
            "claude-code: read what the CLI supports"
        ),
        Help::Unread(why) => tracing::warn!(
            program = %id.path.display(),
            %why,
            "claude-code: `--help` could not be read; passing every option"
        ),
    }
    (help, definite)
}

/// How one run of `program arg` went.
enum Ran {
    /// It exited by itself within the limit, with this code and stdout.
    Exited(i32, String),
    /// It could not be run or read, was killed by a signal, or took longer than the limit:
    /// why.
    Failed(String),
}

/// Most bytes of `--help` / `--version` output read (1 MiB, well above what Claude Code
/// prints): a binary that floods its stdout is stopped there and its probe fails.
const MAX_PROBE_OUTPUT: usize = 1 << 20;

/// Runs `program arg` (with `PATH` `path` when given, in `cwd` unless it is empty) for at
/// most `limit`, reading at most [`MAX_PROBE_OUTPUT`] of its stdout ([`process::run_bounded`]:
/// its process group is killed once it exits, before it is reaped, else when it is stopped
/// early or this is dropped first; no spawn waits for the probe any more).
async fn run_for_output(
    program: &Path,
    arg: &str,
    path: Option<&std::ffi::OsStr>,
    cwd: &Path,
    limit: Duration,
) -> Ran {
    let mut cmd = tokio::process::Command::new(program);
    if let Some(p) = path {
        cmd.env("PATH", p);
    }
    if !cwd.as_os_str().is_empty() {
        cmd.current_dir(cwd);
    }
    cmd.arg(arg);
    match process::run_bounded(cmd, limit, MAX_PROBE_OUTPUT, |_| false).await {
        process::Bounded::Exited(Some(code), out) => {
            Ran::Exited(code, String::from_utf8_lossy(&out).into_owned())
        }
        process::Bounded::Exited(None, _) => Ran::Failed("it was killed by a signal".into()),
        process::Bounded::NotStarted => Ran::Failed("it could not be started".into()),
        process::Bounded::TooLarge => Ran::Failed(format!(
            "it printed more than {} KiB",
            MAX_PROBE_OUTPUT >> 10
        )),
        process::Bounded::TimedOut => Ran::Failed(format!("it took longer than {limit:?}")),
        // `Enough` is never asked for.
        process::Bounded::Failed | process::Bounded::Enough(_) => {
            Ran::Failed("reading its output failed".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::install;

    /// `bin` resolved as a spawn in `/` resolves it.
    fn at(bin: &Path) -> Resolved {
        crate::resolve::resolve(bin, &[], None, Path::new("/"))
    }

    /// Claude Code 2.1.287 (wrapped at 80 columns), cut down.
    const HELP_287: &str = r#"Usage: claude [options] [command] [prompt]

Options:
  --allowedTools, --allowed-tools <tools...>
      Comma or space-separated list of tool names to allow (e.g. "Bash(git *)
      Edit")
  --append-system-prompt <prompt>       Append a system prompt to the default
                                        system prompt
  --bare                                Minimal mode: skip hooks, LSP, plugin
                                        --append-system-prompt[-file], --add-dir
                                        (CLAUDE.md dirs), --mcp-config,
  --include-partial-messages            Include partial message chunks as they
                                        arrive (only works with --print and
                                        --output-format=stream-json)
  --input-format <format>               Input format (only works with --print):
                                        "text" (default), or "stream-json"
                                        (realtime streaming input) (choices:
                                        "text", "stream-json")
  --mcp-config <configs...>             Load MCP servers from JSON files or
                                        strings (space-separated)
  --model <model>                       Model for the current session.
  --output-format <format>              Output format (only works with --print):
                                        (choices: "text", "json", "stream-json")
  --permission-mode <mode>              Permission mode to use for the session
                                        (choices: "acceptEdits", "auto",
                                        "bypassPermissions", "manual",
                                        "dontAsk", "plan")
  --permission-prompts <target>         Who answers permission prompts with
                                        --print: "host" (the SDK host or
                                        --permission-prompt-tool) or "none"
                                        (choices: "host", "none", default:
                                        "host")
  -p, --print                           Print response and exit (useful for
                                        pipes).
  -r, --resume [value]                  Resume a conversation by session ID, or
                                        open interactive picker
  --session-id <uuid>                   Use a specific session ID for the
                                        conversation (must be a valid UUID)
  --strict-mcp-config                   Only use MCP servers from --mcp-config,
                                        ignoring all other MCP configurations
  --tools <tools...>                    Specify the list of available tools from
                                        the built-in set.
  --verbose                             Override verbose mode setting from
                                        config
  -h, --help                            Display help for command

Commands:
  agents [options]                      Manage background agents
  auth                                  Manage authentication
"#;

    /// HELP_287 as Claude Code 2.1.258 printed it: no `--permission-prompts`.
    fn help_258() -> String {
        let start = HELP_287.find("  --permission-prompts").unwrap();
        let end = HELP_287.find("  -p, --print").unwrap();
        format!("{}{}", &HELP_287[..start], &HELP_287[end..])
    }

    /// HELP_258 with the mode list of Claude Code 2.1.197 (`default`, not `manual`).
    fn help_197() -> String {
        help_258().replace("\"manual\"", "\"default\"")
    }

    /// Claude Code 2.1.148 does not wrap: one line per option.
    const HELP_148: &str = r#"Options:
  --allowedTools, --allowed-tools <tools...>        Comma or space-separated list of tool names to allow (e.g. "Bash(git *) Edit")
  --append-system-prompt <prompt>                   Append a system prompt to the default system prompt
  --include-partial-messages                        Include partial message chunks as they arrive (only works with --print and --output-format=stream-json)
  --input-format <format>                           Input format (only works with --print): "text" (default), or "stream-json" (realtime streaming input) (choices: "text", "stream-json")
  --mcp-config <configs...>                         Load MCP servers from JSON files or strings (space-separated)
  --model <model>                                   Model for the current session.
  --output-format <format>                          Output format (choices: "text", "json", "stream-json")
  --permission-mode <mode>                          Permission mode to use for the session (choices: "acceptEdits", "auto", "bypassPermissions", "default", "dontAsk", "plan")
  -p, --print                                       Print response and exit (useful for pipes).
  -r, --resume [value]                              Resume a conversation by session ID
  --session-id <uuid>                               Use a specific session ID for the conversation (must be a valid UUID)
  --strict-mcp-config                               Only use MCP servers from --mcp-config, ignoring all other MCP configurations
  --tools <tools...>                                Specify the list of available tools from the built-in set.
  --verbose                                         Override verbose mode setting from config
"#;

    fn modes(list: &[&str]) -> Option<Vec<String>> {
        Some(list.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn help_options_and_modes() {
        let s = Support::parse(HELP_287, Some("2.1.287 (Claude Code)\n"));
        assert_eq!(s.version, Some(Version(2, 1, 287)));
        for o in [
            "--allowedTools",
            "--allowed-tools",
            "--append-system-prompt",
            "--include-partial-messages",
            "--permission-mode",
            PERMISSION_PROMPTS,
            "--print",
            "--resume",
            "--session-id",
            "--strict-mcp-config",
            "--tools",
            "--verbose",
            "--help",
        ] {
            assert!(s.lists(o), "{o}");
        }
        // Options only named in a description are not options, nor are commands.
        assert!(!s.lists("--permission-prompt-tool"));
        assert!(!s.lists("--add-dir"));
        assert!(!s.lists("--output-format=stream-json"));
        assert!(!s.lists("agents"));
        assert_eq!(
            s.modes,
            modes(&[
                "acceptEdits",
                "auto",
                "bypassPermissions",
                "manual",
                "dontAsk",
                "plan"
            ])
        );

        let s = Support::parse(&help_258(), None);
        assert_eq!(s.version, None);
        assert!(!s.lists(PERMISSION_PROMPTS));
        assert!(s.lists("--permission-mode") && s.lists("--verbose"));

        // Unwrapped help reads the same.
        let s = Support::parse(HELP_148, Some("2.1.148 (Claude Code)"));
        assert!(s.lists("--tools") && s.lists("--allowed-tools") && s.lists("--resume"));
        assert!(!s.lists(PERMISSION_PROMPTS));
        assert_eq!(
            s.modes,
            modes(&[
                "acceptEdits",
                "auto",
                "bypassPermissions",
                "default",
                "dontAsk",
                "plan"
            ])
        );

        // Nothing usable.
        let s = Support::parse("{\"type\":\"system\"}\n", Some("{\"type\":1}"));
        assert_eq!(s, Support::default());
    }

    #[test]
    fn versions() {
        assert_eq!(
            Version::parse("2.1.197 (Claude Code)"),
            Some(Version(2, 1, 197))
        );
        assert_eq!(Version::parse("  10.0.3\n"), Some(Version(10, 0, 3)));
        for bad in [
            "",
            "Claude Code 2.1.197",
            "2.1",
            "2.1.197.4",
            "2.1.x",
            "v2.1.197",
        ] {
            assert_eq!(Version::parse(bad), None, "{bad}");
        }
        assert!(Version(2, 1, 109) < MIN_VERSION);
        assert!(Version(2, 1, 110) >= MIN_VERSION);
        assert!(Version(2, 0, 999) < Version(2, 1, 0));
        assert!(Version(2, 1, 99) < Version(2, 1, 100));
        assert!(Version(3, 0, 0) > Version(2, 99, 99));
        assert_eq!(MIN_VERSION.to_string(), "2.1.110");
    }

    fn args(cfg: &twr_core::adapter::SpawnConfig) -> Vec<String> {
        super::super::build_args(cfg, "11111111-2222-4333-8444-555555555555")
    }

    #[test]
    fn plans() {
        use twr_core::adapter::SpawnConfig;
        let claude = Path::new("claude");
        let full = SpawnConfig {
            model: Some("haiku".into()),
            system_append: Some("--not-an-option".into()),
            mcp_config: Some(serde_json::json!({"mcpServers": {}})),
            allowed_tools: vec!["mcp__twr".into()],
            builtin_tools: Some(vec![]),
            ..Default::default()
        };
        let a = args(&full);
        let p = |s: &Support, mode| plan(claude, Some(s), mode, &a);

        let new = Support::parse(HELP_287, Some("2.1.287 (Claude Code)"));
        assert_eq!(
            p(&new, "manual"),
            Ok(Plan {
                system_prompt_snapshot: false,
                permission_prompts: true,
                mode: "manual"
            })
        );
        // `default` is still taken by versions listing only `manual`.
        assert_eq!(p(&new, "default").unwrap().mode, "default");

        let v258 = Support::parse(&help_258(), Some("2.1.258 (Claude Code)"));
        assert_eq!(
            p(&v258, "dontAsk"),
            Ok(Plan {
                system_prompt_snapshot: false,
                permission_prompts: false,
                mode: "dontAsk"
            })
        );

        // Before 2.1.200 `manual` was called `default`.
        let v197 = Support::parse(&help_197(), Some("2.1.197 (Claude Code)"));
        assert_eq!(
            p(&v197, "manual"),
            Ok(Plan {
                system_prompt_snapshot: false,
                permission_prompts: false,
                mode: "default"
            })
        );
        assert_eq!(p(&v197, "plan").unwrap().mode, "plan");

        // A mode the version does not list is refused, never replaced by another.
        let mut no_auto = v197.clone();
        no_auto.modes = modes(&["acceptEdits", "default", "plan"]);
        assert_eq!(
            p(&no_auto, "auto"),
            Err(
                "`claude` 2.1.197 does not support permission mode \"auto\" (the thread's mode; \
                 `twr mode <thread> <mode>` changes it, `[threads] default_mode` sets it for new \
                 threads): run `claude update`"
                    .into()
            )
        );
        assert_eq!(p(&no_auto, "acceptEdits").unwrap().mode, "acceptEdits");

        // Too old, whatever its help says.
        let old = Support {
            version: Some(Version(2, 1, 100)),
            ..v197.clone()
        };
        assert_eq!(
            p(&old, "plan"),
            Err(
                "`claude` 2.1.100 is too old for twr (needs 2.1.110 or newer): run `claude update`"
                    .into()
            )
        );
        let old_unreadable = Support {
            version: Some(Version(2, 0, 0)),
            ..Default::default()
        };
        assert!(p(&old_unreadable, "plan").unwrap_err().contains("too old"));

        // An option the spawn passes that the help lacks; values are not options.
        let mut no_tools = v197.clone();
        no_tools.options.remove("--tools");
        assert_eq!(
            plan(Path::new("/opt/claude"), Some(&no_tools), "plan", &a),
            Err(
                "`/opt/claude` 2.1.197 does not support --tools (twr needs Claude Code 2.1.110 \
                 or newer): run `claude update`"
                    .into()
            )
        );
        // ... but only when this spawn passes it.
        assert!(plan(claude, Some(&no_tools), "plan", &args(&Default::default())).is_ok());
        no_tools.version = None;
        assert!(p(&no_tools, "plan")
            .unwrap_err()
            .starts_with("`claude` does not support --tools"));

        // Unknown support (no probe, a failed one, or help that says nothing): pass everything.
        let pass_all = Ok(Plan {
            system_prompt_snapshot: false,
            permission_prompts: true,
            mode: "manual",
        });
        assert_eq!(plan(claude, None, "manual", &a), pass_all);
        assert_eq!(p(&Support::default(), "manual"), pass_all);
        let no_list = Support {
            modes: None,
            ..v258.clone()
        };
        assert_eq!(p(&no_list, "auto").unwrap().mode, "auto");
    }

    /// HELP_287 plus the `--effort` option as Claude Code 2.1.288 prints it.
    fn help_288() -> String {
        let at = HELP_287.find("  --include-partial-messages").unwrap();
        format!(
            "{}  --effort <level>                      Effort level for the current session\n                                        (low, medium, high, xhigh, max)\n{}",
            &HELP_287[..at],
            &HELP_287[at..]
        )
    }

    /// `--effort` is passed only for a thread with an effort; a version whose `--help` lacks it
    /// refuses such a spawn with the thread setting and the update hint, and any other spawn
    /// runs as before.
    #[test]
    fn efforts_need_a_version_that_lists_them() {
        use twr_core::adapter::SpawnConfig;
        let claude = Path::new("claude");
        let high = SpawnConfig {
            effort: Some("high".into()),
            ..Default::default()
        };
        let with = args(&high);
        let at = with.iter().position(|a| a == EFFORT).unwrap();
        assert_eq!(with[at + 1], "high");
        assert!(!args(&Default::default()).iter().any(|a| a == EFFORT));
        assert!(!args(&SpawnConfig {
            effort: Some(String::new()),
            ..Default::default()
        })
        .iter()
        .any(|a| a == EFFORT));

        let new = Support::parse(&help_288(), Some("2.1.288 (Claude Code)"));
        assert!(new.lists(EFFORT));
        assert_eq!(effort_refusal(claude, &new), None);
        assert!(plan(claude, Some(&new), "manual", &with).is_ok());

        let old = Support::parse(HELP_287, Some("2.1.287 (Claude Code)"));
        let why = "`claude` 2.1.287 does not support --effort (the thread's reasoning effort; \
                   `twr model <thread> --effort default` clears it): run `claude update`";
        assert_eq!(effort_refusal(claude, &old).as_deref(), Some(why));
        assert_eq!(plan(claude, Some(&old), "manual", &with), Err(why.into()));
        assert!(plan(claude, Some(&old), "manual", &args(&Default::default())).is_ok());
        // Nothing usable known: pass it (the binary refuses it itself, with the hint).
        assert_eq!(effort_refusal(claude, &Support::default()), None);
        assert!(plan(claude, None, "manual", &with).is_ok());
    }

    #[test]
    fn refused_options() {
        let tail = "noise\nerror: unknown option '--permission-prompts'\n";
        let hint = "`claude` refused twr's command line (error: unknown option \
                    '--permission-prompts'): run `claude update` (twr needs Claude Code 2.1.110 \
                    or newer)";
        assert_eq!(
            refused_options_hint(tail, Path::new("claude"), None).as_deref(),
            Some(hint)
        );
        // `--help` was not read: the hint says why first; the update hint stays last.
        assert_eq!(
            refused_options_hint(tail, Path::new("claude"), Some("it took longer than 10s")),
            Some(format!(
                "twr could not read `claude --help` (it took longer than 10s), so it passed \
                 every option; the next start reads it again\n{hint}"
            ))
        );
        let tail = "error: option '--permission-mode <mode>' argument 'manual' is invalid. \
                    Allowed choices are acceptEdits, auto, bypassPermissions, default, dontAsk, \
                    plan.\n";
        assert!(refused_options_hint(tail, Path::new("claude"), None)
            .unwrap()
            .contains("argument 'manual' is invalid"));
        assert_eq!(
            refused_options_hint("fatal: bad flag\n", Path::new("claude"), Some("x")),
            None
        );
        assert_eq!(refused_options_hint("", Path::new("claude"), None), None);
    }

    /// A `claude` stub in `dir` answering `--help` and `--version` with the arms of a
    /// `case "$1" in`; every `--help` run is counted in `<dir>/probes`. Written by `cp`
    /// ([`install`]: no fork of this process may hold it open for writing when it is run).
    fn stub(dir: &Path, help: &str, version: &str) -> PathBuf {
        let path = dir.join("claude");
        stub_at(&path, dir, help, version);
        path
    }

    /// [`stub`] at `path`, counting its `--help` runs in `<dir>/probes`.
    fn stub_at(path: &Path, dir: &Path, help: &str, version: &str) {
        let d = dir.display();
        install(
            path,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n--help) echo probed >> {d}/probes; {help};;\n\
                 --version) {version};;\nesac\nexit 9\n"
            ),
        );
    }

    fn probes(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("probes"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    const LIMIT: Duration = Duration::from_secs(10);

    /// The result of a spawn's lookup, waiting for the probe if there is one; whether it was
    /// cached.
    async fn look(bin: &Path, limit: Duration) -> (Help, bool) {
        match lookup(&at(bin), Path::new("/"), limit) {
            Lookup::Known(h) => (h, true),
            Lookup::Probe(p) => (p.wait().await, false),
        }
    }

    fn help_of(dir: &Path) -> String {
        let file = dir.join("help.txt");
        std::fs::write(&file, HELP_287).unwrap();
        format!("cat {}", file.display())
    }

    #[tokio::test]
    async fn a_read_help_is_cached() {
        let dir = tempfile::tempdir().unwrap();
        let help = format!("{}; exit 0", help_of(dir.path()));
        let bin = stub(dir.path(), &help, "echo '2.1.287 (Claude Code)'; exit 0");
        let (h, cached) = look(&bin, LIMIT).await;
        assert!(!cached);
        let s = h.support().unwrap();
        assert_eq!(s.version, Some(Version(2, 1, 287)));
        assert!(s.lists(PERMISSION_PROMPTS));
        let (again, cached) = look(&bin, LIMIT).await;
        assert!(cached);
        assert_eq!(again, h);
        assert_eq!(cached_support(&bin), Some(s.clone()));
        assert_eq!(probes(dir.path()), 1);
    }

    fn cached_support(bin: &Path) -> Option<Support> {
        cached(&at(bin), Path::new("/"))
    }

    /// A `--help` listing its options counts even when it exits non-zero; one that says
    /// nothing and exits non-zero is not read, and not cached.
    #[tokio::test]
    async fn a_failed_help_is_read_only_if_it_lists_options() {
        let dir = tempfile::tempdir().unwrap();
        let help = format!("{}; exit 1", help_of(dir.path()));
        let bin = stub(dir.path(), &help, "exit 1");
        let (h, _) = look(&bin, LIMIT).await;
        assert!(h.support().unwrap().lists("--tools"));
        assert_eq!(h.support().unwrap().version, None);
        assert!(look(&bin, LIMIT).await.1, "cached");

        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "echo 'unknown command' >&2; exit 2", "exit 0");
        let (h, cached) = look(&bin, LIMIT).await;
        assert!(!cached);
        assert_eq!(h, Help::Unread("it exited with code 2".into()));
        assert!(!look(&bin, LIMIT).await.1, "not cached");
        assert_eq!(cached_support(&bin), None);
        assert_eq!(probes(dir.path()), 2);
    }

    /// A `--help` that takes too long is killed and not cached: the next spawn probes again.
    #[tokio::test]
    async fn a_timed_out_probe_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "sleep 30", "exit 0");
        let limit = Duration::from_millis(200);
        let (h, cached) = look(&bin, limit).await;
        assert!(!cached);
        assert_eq!(h, Help::Unread("it took longer than 200ms".into()));
        assert_eq!(h.support(), None);
        assert_eq!(cached_support(&bin), None);
        let (h, cached) = look(&bin, limit).await;
        assert!(!cached);
        assert_eq!(h.unread(), Some("it took longer than 200ms"));
        assert_eq!(probes(dir.path()), 2);

        // Answering in time later is cached.
        let help = format!("{}; exit 0", help_of(dir.path()));
        let bin = stub(dir.path(), &format!("true; {help}"), "exit 0");
        assert!(look(&bin, limit).await.0.support().is_some());
        assert!(look(&bin, limit).await.1);
    }

    /// A `--version` that takes too long: its `--help` is used, but not cached (the version
    /// check would be skipped for good).
    #[tokio::test]
    async fn a_timed_out_version_is_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let help = format!("{}; exit 0", help_of(dir.path()));
        let bin = stub(dir.path(), &help, "sleep 30");
        let limit = Duration::from_millis(300);
        let (h, cached) = look(&bin, limit).await;
        assert!(!cached);
        assert!(h.support().unwrap().lists(PERMISSION_PROMPTS));
        assert!(!look(&bin, limit).await.1);
        assert_eq!(probes(dir.path()), 2);
    }

    /// A `--help` that floods its stdout is stopped at the cap: not read, not cached.
    #[tokio::test]
    async fn a_flooding_help_is_capped() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "exec yes flood", "exit 0");
        let (h, cached) = look(&bin, LIMIT).await;
        assert!(!cached);
        assert_eq!(h, Help::Unread("it printed more than 1024 KiB".into()));
        assert_eq!(cached_support(&bin), None);
    }

    /// One wrapper (a `TWR_CLAUDE_BIN` running `exec claude-real "$@"`) on two `PATH`s runs
    /// two different `claude`s: each `PATH` is probed on its own, and what is cached for one
    /// never answers for the other, neither for a spawn's lookup nor for the cache reads of an
    /// attach command and of the effort check.
    #[tokio::test]
    async fn one_wrapper_on_two_paths_is_two_binaries() {
        use twr_core::adapter::{Adapter, SpawnConfig};
        let dir = tempfile::tempdir().unwrap();
        let (new, old) = (dir.path().join("new"), dir.path().join("old"));
        for (d, help, version) in [
            (&new, help_288(), "2.1.288"),
            (&old, HELP_287.into(), "2.1.287"),
        ] {
            std::fs::create_dir(d).unwrap();
            let file = d.join("help.txt");
            std::fs::write(&file, help).unwrap();
            stub_at(
                &d.join("claude-real"),
                d,
                &format!("cat {}; exit 0", file.display()),
                &format!("echo '{version} (Claude Code)'; exit 0"),
            );
        }
        let wrapper = dir.path().join("wrapper");
        install(&wrapper, "#!/bin/sh\nexec claude-real \"$@\"\n");
        let env = |d: &Path| vec![("PATH".to_string(), format!("{}:/usr/bin:/bin", d.display()))];
        let on = |d: &Path| crate::resolve::resolve(&wrapper, &env(d), None, Path::new("/"));
        let look_on = |d: &Path| {
            let r = on(d);
            async move {
                match lookup(&r, Path::new("/"), LIMIT) {
                    Lookup::Known(h) => (h, true),
                    Lookup::Probe(p) => (p.wait().await, false),
                }
            }
        };
        // Probed apart, though the wrapper is one file.
        let (h_new, cached_new) = look_on(&new).await;
        let (h_old, cached_old) = look_on(&old).await;
        assert!(!cached_new && !cached_old);
        assert!(h_new.support().unwrap().lists(EFFORT), "{h_new:?}");
        assert!(!h_old.support().unwrap().lists(EFFORT), "{h_old:?}");
        assert_eq!((probes(&new), probes(&old)), (1, 1));
        // Each answered from the cache since, with its own result.
        let (again, was_cached) = look_on(&old).await;
        assert!(was_cached);
        assert_eq!(again, h_old);
        assert_eq!(cached(&on(&new), Path::new("/")).as_ref(), h_new.support());
        assert_eq!(cached(&on(&old), Path::new("/")).as_ref(), h_old.support());
        assert_eq!((probes(&new), probes(&old)), (1, 1));
        // The effort check and the attach command read the result of their own `PATH`.
        assert_eq!(
            effort_refusal(&wrapper, &cached(&on(&new), Path::new("/")).unwrap()),
            None
        );
        assert!(effort_refusal(&wrapper, &cached(&on(&old), Path::new("/")).unwrap()).is_some());
        let adapter = crate::claude_code::ClaudeCodeAdapter::with_bin(&wrapper);
        let attach = |d: &Path| {
            let cfg = SpawnConfig {
                effort: Some("high".into()),
                env: env(d),
                cwd: dir.path().to_path_buf(),
                ..Default::default()
            };
            let c = adapter.attach_command(&cfg, "s-1").unwrap();
            c.get_args().any(|a| a == EFFORT)
        };
        assert!(attach(&new), "the newer claude takes --effort");
        assert!(!attach(&old), "the older one would refuse it");
    }

    /// One absolute wrapper (`exec claude-real "$@"`) with the relative `PATH` `bin:/usr/bin`
    /// runs the `bin/claude-real` of each thread's directory: the probe runs there, and each
    /// directory is probed (at the same time, too) and cached on its own, for a spawn's lookup
    /// and for the cache reads of an attach command and of the effort check. With an absolute
    /// `PATH`, threads in other directories share the one probe.
    #[tokio::test]
    async fn a_relative_path_is_probed_per_directory() {
        use twr_core::adapter::{Adapter, SpawnConfig};
        let dir = tempfile::tempdir().unwrap();
        let (new, old) = (dir.path().join("new"), dir.path().join("old"));
        for (d, help, version) in [
            (&new, help_288(), "2.1.288"),
            (&old, HELP_287.into(), "2.1.287"),
        ] {
            std::fs::create_dir_all(d.join("bin")).unwrap();
            let file = d.join("help.txt");
            std::fs::write(&file, help).unwrap();
            stub_at(
                &d.join("bin/claude-real"),
                d,
                &format!("cat {}; exit 0", file.display()),
                &format!("echo '{version} (Claude Code)'; exit 0"),
            );
        }
        let wrapper = dir.path().join("wrapper");
        install(&wrapper, "#!/bin/sh\nexec claude-real \"$@\"\n");
        let env = vec![("PATH".to_string(), "bin:/usr/bin:/bin".to_string())];
        let on = |d: &Path| crate::resolve::resolve(&wrapper, &env, None, d);
        // Both probed at once: neither waits for the other's probe.
        let (p_new, p_old) = match (
            lookup(&on(&new), &new, LIMIT),
            lookup(&on(&old), &old, LIMIT),
        ) {
            (Lookup::Probe(a), Lookup::Probe(b)) => (a, b),
            other => panic!("not two probes: {other:?}"),
        };
        let (h_new, h_old) = (p_new.wait().await, p_old.wait().await);
        assert!(h_new.support().unwrap().lists(EFFORT), "{h_new:?}");
        assert!(!h_old.support().unwrap().lists(EFFORT), "{h_old:?}");
        assert_eq!((probes(&new), probes(&old)), (1, 1));
        // Each answered from the cache since, with its own result.
        for (d, h) in [(&new, &h_new), (&old, &h_old)] {
            match lookup(&on(d), d, LIMIT) {
                Lookup::Known(known) => assert_eq!(&known, h),
                Lookup::Probe(_) => panic!("probed again in {}", d.display()),
            }
            assert_eq!(cached(&on(d), d).as_ref(), h.support());
        }
        assert_eq!((probes(&new), probes(&old)), (1, 1));
        // Nothing probed in a third directory: nothing known there.
        assert_eq!(cached(&on(dir.path()), dir.path()), None);
        // The effort check and the attach command read the result of their own directory.
        assert_eq!(
            effort_refusal(&wrapper, &cached(&on(&new), &new).unwrap()),
            None
        );
        assert!(effort_refusal(&wrapper, &cached(&on(&old), &old).unwrap()).is_some());
        let adapter = crate::claude_code::ClaudeCodeAdapter::with_bin(&wrapper);
        let attach = |d: &Path| {
            let cfg = SpawnConfig {
                effort: Some("high".into()),
                env: env.clone(),
                cwd: d.to_path_buf(),
                ..Default::default()
            };
            let c = adapter.attach_command(&cfg, "s-1").unwrap();
            c.get_args().any(|a| a == EFFORT)
        };
        assert!(attach(&new), "the newer claude takes --effort");
        assert!(!attach(&old), "the older one would refuse it");

        // An absolute `PATH` (to `new/bin`): one probe, shared by every directory.
        let env = vec![(
            "PATH".to_string(),
            format!("{}:/usr/bin:/bin", new.join("bin").display()),
        )];
        let on = |d: &Path| crate::resolve::resolve(&wrapper, &env, None, d);
        let (h, was_cached) = match lookup(&on(&old), &old, LIMIT) {
            Lookup::Known(h) => (h, true),
            Lookup::Probe(p) => (p.wait().await, false),
        };
        assert!(!was_cached);
        assert!(h.support().unwrap().lists(EFFORT), "{h:?}");
        assert_eq!((probes(&new), probes(&old)), (2, 1));
        for d in [&new, &old, &dir.path().to_path_buf()] {
            match lookup(&on(d), d, LIMIT) {
                Lookup::Known(known) => assert_eq!(known, h),
                Lookup::Probe(_) => panic!("probed again in {}", d.display()),
            }
            assert_eq!(cached(&on(d), d).as_ref(), h.support());
        }
        assert_eq!((probes(&new), probes(&old)), (2, 1));
    }

    #[test]
    fn relative_path_entries() {
        let rel = |s: &str| relative_entry(Some(std::ffi::OsStr::new(s)));
        assert!(rel("bin:/usr/bin"));
        assert!(rel("/usr/bin:./bin"));
        assert!(rel("/usr/bin:"));
        assert!(rel(":/usr/bin"));
        assert!(rel("/usr/bin::/bin"));
        assert!(rel(""));
        assert!(!rel("/usr/bin:/bin"));
        assert!(!relative_entry(None));
    }

    /// Spawns arriving while a probe runs wait for it: one probe, one result for all.
    #[tokio::test]
    async fn spawns_share_one_probe() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "sleep 0.5; exit 3", "exit 0");
        let probes_now: Vec<Probe> = (0..3)
            .map(|_| match lookup(&at(&bin), Path::new("/"), LIMIT) {
                Lookup::Probe(p) => p,
                Lookup::Known(h) => panic!("known: {h:?}"),
            })
            .collect();
        let mut results = Vec::new();
        for p in probes_now {
            results.push(p.wait().await);
        }
        assert!(results
            .iter()
            .all(|h| *h == Help::Unread("it exited with code 3".into())));
        assert_eq!(probes(dir.path()), 1);
    }

    /// A probe nobody waits for any more is killed (its process group with it); a spawn
    /// arriving later starts a new one.
    #[tokio::test]
    async fn a_probe_nobody_waits_for_is_killed() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().display();
        let bin = stub(
            dir.path(),
            &format!("sleep 1; echo done >> {d}/done; exit 0"),
            "exit 0",
        );
        let Lookup::Probe(p) = lookup(&at(&bin), Path::new("/"), LIMIT) else {
            panic!("not probed");
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(p);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(!dir.path().join("done").exists(), "the probe ran on");
        assert_eq!(probes(dir.path()), 1);
        let (h, _) = look(&bin, LIMIT).await;
        assert!(h.support().is_some());
        assert_eq!(probes(dir.path()), 2);
    }

    #[test]
    fn options_of_args() {
        let a: Vec<String> = [
            "-p",
            "--verbose",
            "--model",
            "--tools",
            "--tools",
            "",
            "--append-system-prompt",
            "--x",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(
            options_in(&a),
            ["--verbose", "--model", "--tools", "--append-system-prompt"]
        );
    }
}
