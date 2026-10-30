//! Starting the box daemon as a launchd job in the user's GUI session (macOS), so that what
//! it runs reads the user's login keychain: a process started from an ssh session lives in
//! that session's security context, where the keychain cannot be read (Claude Code then looks
//! logged out), while a job bootstrapped into `gui/<uid>` runs in the GUI (Aqua) session of
//! the user logged in at the Mac's screen, where it can.
//!
//! [`start`], under the relay's start lock ([`super::StartLock`]):
//!
//! 1. Plists of this hub key and box ([`label_prefix`]) that a relay killed part way left in
//!    the daemon's data directory are removed (also when the daemon is then started from ssh).
//! 2. `launchctl print gui/<uid>`: no such domain (exit 113: no one logged in at the screen),
//!    and the relay falls back to starting the daemon from ssh, with a note
//!    ([`no_gui_session_note`]); when that can't be told (`launchctl` failed otherwise, did
//!    not finish in time, or isn't there), it falls back too, with a note that says so
//!    ([`unknown_session_note`]); with `TWR_BOX_LAUNCHER=launchd`, either is an error. Else
//!    each job of
//!    this hub key and box that the domain lists and that `launchctl print gui/<uid>/<label>`
//!    says is loaded but not running (a daemon that ended: a job stays loaded once its
//!    process exits) is booted out: [`stale_labels`], [`job_state`]; at most
//!    [`STALE_PER_START`] of them per start.
//! 3. The job's plist ([`plist`]: label `tools.twr.box.<socket hash>.<random>`, a new one each
//!    start, [`new_label`]) is written into the daemon's private data directory (mode 0600,
//!    created exclusively), bootstrapped (`launchctl bootstrap gui/<uid> <plist>`), and
//!    removed (launchd keeps the job's definition), whatever happened. It holds the daemon's
//!    command line and the part of the relay's environment the daemon needs ([`job_env`]: an
//!    allowlist, so no secret is ever written there).
//! 4. A failed or timed-out bootstrap may still have loaded the job: it is inspected
//!    ([`after_failed_bootstrap`]): loaded, the relay waits for the daemon; not loaded, it
//!    falls back; when that can't be told, the job is booted out and must then be gone before
//!    the relay falls back (else an error: no second daemon beside one that may run).
//!
//! Every `launchctl` call is bounded ([`LAUNCHCTL_TIMEOUT`],
//! `twr_adapters::process::run_bounded`), and so are the calls of a start
//! ([`CALLS_BUDGET`]). A GUI logout ends the job (the daemon and its
//! agents); the hub reconnects after the next login and the relay starts a new one.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use twr_adapters::process::{run_bounded, Bounded};

use super::{socket_hash, RelayOptions, LAUNCHER_ENV};

/// Every box daemon's launchd label starts so: `tools.twr.box.<socket hash>.<random>`.
pub const LABEL_PREFIX: &str = "tools.twr.box.";

/// How long one `launchctl` call may take.
pub const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(10);

/// Most bytes read of what `launchctl print` prints (a domain lists every service in it).
const PRINT_MAX: usize = 8 << 20;

/// `launchctl print gui/<uid>/<label>`'s exit code for a service the domain does not have
/// ("Could not find service"), and `launchctl print gui/<uid>`'s for a domain that isn't there
/// (no one is logged in at the Mac's screen). Only this exit says either: any other failure
/// says nothing of the job or the session.
const NO_SUCH_SERVICE: i32 = 113;

/// The job's `ExitTimeOut`: how long launchd waits after its SIGTERM before it kills the
/// daemon, the daemon's whole shutdown ([`crate::client::SHUTDOWN_BUDGET`]: its runtimes'
/// grace, then a repository change still running, and a margin): 330 s, as the systemd unit in
/// `docs/runners.md` (`TimeoutStopSec=330`).
pub const EXIT_TIMEOUT: Duration = crate::client::SHUTDOWN_BUDGET;

/// Most jobs of this box one start inspects (one `launchctl` call each, and one more to boot
/// out one that ended); any more are left to the next start (each daemon leaves one when it
/// ends, so they do not pile up).
pub const STALE_PER_START: usize = 3;

/// The longest the `launchctl` calls of one [`start`] take: the domain's listing, at most
/// [`STALE_PER_START`] jobs inspected (and booted out when they ended), the bootstrap and,
/// after a failed one, its job inspected, booted out and inspected again; each at most
/// [`LAUNCHCTL_TIMEOUT`], and `twr_adapters::process::REAP_LIMIT` to reap one that did not
/// finish.
pub const CALLS_BUDGET: Duration = Duration::from_secs(
    (1 + 2 * STALE_PER_START as u64 + 1 + 3)
        * (LAUNCHCTL_TIMEOUT.as_secs() + twr_adapters::process::REAP_LIMIT.as_secs()),
);

/// The variables of the relay's environment the job gets (a launchd job does not get the ssh
/// session's environment): these, and those starting with [`ENV_PREFIXES`]. No other is ever
/// written into the plist, so an API key in the ssh session's environment does not reach a
/// daemon launchd starts (on a Mac the agents use their keychain or file logins).
const ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "SHELL",
    "USER",
    "LOGNAME",
    "LANG",
    "TMPDIR",
    "TZ",
    twr_adapters::claude_code::BIN_ENV,
    twr_adapters::codex::BIN_ENV,
    twr_adapters::grok_build::BIN_ENV,
    crate::agents::BOX_PROBE_ENV,
    "TWR_AGENT_PROBE",
];

/// Prefixes of the variables the job gets too ([`ENV_NAMES`]): `LC_*`, `XDG_*`.
const ENV_PREFIXES: &[&str] = &["LC_", "XDG_"];

/// The variables without which the job is not started by launchd (when theirs cannot be
/// written in a plist): the daemon's agents are found on `PATH`, and its files are under
/// `HOME`.
const ENV_REQUIRED: &[&str] = &["PATH", "HOME"];

/// How a relay runs `launchctl` and in which domain: [`Launchctl::system`], or a test's.
#[derive(Debug, Clone)]
pub struct Launchctl {
    /// The command (`/bin/launchctl`; a test's `sh <script>`).
    pub command: Vec<OsString>,
    /// `gui/<uid>`.
    pub domain: String,
}

impl Launchctl {
    /// `/bin/launchctl`, in this user's GUI domain.
    pub fn system() -> Launchctl {
        Launchctl {
            command: vec!["/bin/launchctl".into()],
            domain: format!("gui/{}", twr_core::paths::effective_uid()),
        }
    }

    /// `launchctl args`, bounded: how it ended, with at most `max` bytes of its stdout when it
    /// exited.
    async fn run(&self, args: &[&str], max: usize) -> Ran {
        let Some((program, lead)) = self.command.split_first() else {
            return Ran::Not(Unrun::Missing);
        };
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(lead).args(args);
        match run_bounded(cmd, LAUNCHCTL_TIMEOUT, max, |_| false).await {
            Bounded::Exited(Some(code), out) => Ran::Exited(code, out),
            Bounded::Exited(None, _) => Ran::Not(Unrun::Signal),
            Bounded::NotStarted => Ran::Not(Unrun::Missing),
            Bounded::TimedOut => Ran::Not(Unrun::TimedOut),
            Bounded::TooLarge => Ran::Not(Unrun::TooLarge),
            Bounded::Enough(_) | Bounded::Failed => Ran::Not(Unrun::Failed),
        }
    }

    /// Whether someone is logged in at the Mac's screen, by `launchctl print gui/<uid>`: its
    /// listing when the domain is there ([`GuiDomain::Session`]); none only by its exit
    /// [`NO_SUCH_SERVICE`]; not known otherwise.
    async fn gui_domain(&self) -> GuiDomain {
        let print = format!("`launchctl print {}`", self.domain);
        match self.run(&["print", &self.domain], PRINT_MAX).await {
            Ran::Exited(0, out) => GuiDomain::Session(String::from_utf8_lossy(&out).into_owned()),
            Ran::Exited(NO_SUCH_SERVICE, _) => GuiDomain::NoSession,
            Ran::Exited(code, _) => GuiDomain::Unknown(format!("{print} exited {code}")),
            Ran::Not(why) => GuiDomain::Unknown(format!("{print} {}", why.said())),
        }
    }

    /// The state of job `label` ([`job_state`]).
    async fn state(&self, label: &str) -> JobState {
        match self
            .run(&["print", &format!("{}/{label}", self.domain)], 1 << 20)
            .await
        {
            Ran::Exited(code, out) => job_state(Some(code), &out),
            Ran::Not(_) => JobState::Unknown,
        }
    }

    /// `launchctl bootout gui/<uid>/<label>` (what it says is not read: the job's state is
    /// checked afterwards when it matters).
    async fn boot_out(&self, label: &str) {
        let _ = self
            .run(&["bootout", &format!("{}/{label}", self.domain)], 64 << 10)
            .await;
    }

    /// `launchctl bootstrap gui/<uid> <plist>`: an error that says how it failed (its exit
    /// code, or why it did not run to its end) otherwise.
    async fn bootstrap(&self, plist: &Path) -> anyhow::Result<()> {
        let path = plist
            .to_str()
            .ok_or_else(|| anyhow!("{} is not UTF-8", plist.display()))?;
        match self.run(&["bootstrap", &self.domain, path], 64 << 10).await {
            Ran::Exited(0, _) => Ok(()),
            Ran::Exited(code, _) => {
                bail!("`launchctl bootstrap {}` failed (exit {code})", self.domain)
            }
            Ran::Not(why) => bail!("`launchctl bootstrap {}` {}", self.domain, why.said()),
        }
    }
}

/// How a `launchctl` call ended ([`Launchctl::run`]).
#[derive(Debug)]
enum Ran {
    /// It exited with this code; its stdout.
    Exited(i32, Vec<u8>),
    /// It did not run to an exit of its own.
    Not(Unrun),
}

/// Why a `launchctl` call did not run to an exit of its own ([`Ran::Not`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unrun {
    /// It could not be started (not there, not executable).
    Missing,
    /// It did not finish within [`LAUNCHCTL_TIMEOUT`].
    TimedOut,
    /// A signal ended it.
    Signal,
    /// It printed more than is read.
    TooLarge,
    /// Reading its output, or waiting for it, failed.
    Failed,
}

impl Unrun {
    /// What happened, after the command's name: fixed words.
    fn said(self) -> String {
        match self {
            Unrun::Missing => "could not be run (is `/bin/launchctl` there?)".into(),
            Unrun::TimedOut => format!("did not finish within {} s", LAUNCHCTL_TIMEOUT.as_secs()),
            Unrun::Signal => "was ended by a signal".into(),
            Unrun::TooLarge => "printed more than twr reads".into(),
            Unrun::Failed => "failed (its output could not be read)".into(),
        }
    }
}

/// Whether someone is logged in at the Mac's screen ([`Launchctl::gui_domain`]).
#[derive(Debug, Clone, PartialEq, Eq)]
enum GuiDomain {
    /// Yes: what `launchctl print gui/<uid>` printed (the services it lists among it).
    Session(String),
    /// No (`launchctl print gui/<uid>` exited [`NO_SUCH_SERVICE`]).
    NoSession,
    /// Not known: why, in fixed words (`launchctl` failed otherwise, did not finish in time,
    /// or could not be run).
    Unknown(String),
}

/// A launchd job as `launchctl print gui/<uid>/<label>` shows it ([`job_state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Loaded, its process running.
    Running,
    /// Loaded, not running (it ended, or has not started yet).
    Loaded,
    /// Not in the domain.
    Absent,
    /// Not known (`launchctl` failed otherwise, or did not answer in time).
    Unknown,
}

/// The state of a job from `launchctl print gui/<uid>/<label>`'s exit code `code` (`None`: it
/// did not run to its end) and output `out`: exit 0 is a loaded job, running when its first
/// `state = ` line says `running`; [`NO_SUCH_SERVICE`] is none; anything else is not known.
pub fn job_state(code: Option<i32>, out: &[u8]) -> JobState {
    match code {
        Some(0) => {
            let text = String::from_utf8_lossy(out);
            let state = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("state = ").map(str::trim));
            match state {
                Some("running") => JobState::Running,
                _ => JobState::Loaded,
            }
        }
        Some(NO_SUCH_SERVICE) => JobState::Absent,
        _ => JobState::Unknown,
    }
}

/// What a start does once `launchctl bootstrap` failed or timed out, by the state of the job
/// it submitted ([`after_failed_bootstrap`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterBootstrap {
    /// The job is loaded all the same: wait for its daemon to answer (no second one).
    Wait,
    /// It is not: start the daemon from ssh instead (unless launchd was asked for).
    Fallback,
    /// Not known: boot the job out, and fall back only once it is gone.
    BootOut,
}

/// [`AfterBootstrap`] for a submitted job in state `state`.
pub fn after_failed_bootstrap(state: JobState) -> AfterBootstrap {
    match state {
        JobState::Running | JobState::Loaded => AfterBootstrap::Wait,
        JobState::Absent => AfterBootstrap::Fallback,
        JobState::Unknown => AfterBootstrap::BootOut,
    }
}

/// The start of every label of the daemon of hub key `hub_key` and box `box_name`:
/// `tools.twr.box.<socket hash>.` ([`socket_hash`]).
pub fn label_prefix(hub_key: &[u8; 32], box_name: &str) -> String {
    format!(
        "{LABEL_PREFIX}{}.",
        socket_hash(&twr_core::sealed::hub_id(hub_key), box_name)
    )
}

/// A new label of `prefix` ([`label_prefix`]): 8 random hex digits after it, a new one each
/// start (as the systemd unit's), so a job not torn down yet never collides with the new one.
pub fn new_label(prefix: &str) -> String {
    let tag = twr_core::runner::new_nonce()
        .map(|n| n[..8].to_string())
        .unwrap_or_else(|_| std::process::id().to_string());
    format!("{prefix}{tag}")
}

/// Whether `label` is one of `prefix` ([`new_label`]'s shape: 1 to 16 letters or digits after
/// it).
pub fn is_label_of(label: &str, prefix: &str) -> bool {
    label.strip_prefix(prefix).is_some_and(|t| {
        !t.is_empty() && t.len() <= 16 && t.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// The labels of `prefix` that `listing` (what `launchctl print gui/<uid>` printed) names, but
/// `own` (the label this relay is about to submit), each once: the candidates for a stale
/// job, which are booted out only once `launchctl print gui/<uid>/<label>` says they are
/// loaded and not running.
pub fn stale_labels(listing: &str, prefix: &str, own: &str) -> Vec<String> {
    let mut out: Vec<String> = listing
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '"' || c == ';' || c == ','))
        .filter(|w| *w != own && is_label_of(w, prefix))
        .map(str::to_string)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// Whether `name`, an entry of the daemon's data directory, is a plist a relay wrote for a job
/// of `prefix` (`<label>.plist`): one left there is removed by the next start.
pub fn is_plist_of(name: &str, prefix: &str) -> bool {
    name.strip_suffix(".plist")
        .is_some_and(|l| is_label_of(l, prefix))
}

/// Whether `name` is one of the variables the job gets ([`ENV_NAMES`], [`ENV_PREFIXES`]).
fn passed_on(name: &str) -> bool {
    ENV_NAMES.contains(&name)
        || ENV_PREFIXES.iter().any(|p| {
            name.len() > p.len()
                && name.starts_with(p)
                && name
                    .bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
}

/// Whether `s` can be written in a plist as it is: every character one XML 1.0 holds (no
/// control character but tab, newline and carriage return; no U+FFFE or U+FFFF).
pub fn representable(s: &str) -> bool {
    s.chars().all(|c| {
        matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{D7FF}' | '\u{E000}'..='\u{FFFD}')
            || c >= '\u{10000}'
    })
}

/// `s` escaped for XML text or an attribute: `&`, `<`, `>`, `"`, `'`, and a carriage return
/// (which a parser would read as a newline otherwise). Only for a [`representable`] `s`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\r' => out.push_str("&#13;"),
            c => out.push(c),
        }
    }
    out
}

/// A launchd job's environment ([`job_env`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobEnv {
    /// The variables it gets, sorted by name.
    pub vars: Vec<(String, String)>,
    /// Those left out because their value cannot be written in a plist, by name.
    pub skipped: Vec<String>,
}

/// The job's environment from `vars` (the relay's): the variables [`passed_on`], sorted, and
/// the names of those skipped because their value cannot be written in a plist (not UTF-8, or
/// not [`representable`]). An error when that is so of one of [`ENV_REQUIRED`]. Never a value
/// in a message.
pub fn job_env(vars: impl IntoIterator<Item = (OsString, OsString)>) -> anyhow::Result<JobEnv> {
    let mut kept = Vec::new();
    let mut skipped = Vec::new();
    for (name, value) in vars {
        let Some(name) = name.to_str().filter(|n| passed_on(n)) else {
            continue;
        };
        match value.into_string() {
            Ok(v) if representable(&v) => kept.push((name.to_string(), v)),
            _ if ENV_REQUIRED.contains(&name) => bail!(
                "the value of {name} can't be written in a launchd job's plist (it is not UTF-8 \
                 text, or holds a character XML can't)"
            ),
            _ => skipped.push(name.to_string()),
        }
    }
    kept.sort();
    kept.dedup_by(|a, b| a.0 == b.0);
    skipped.sort();
    skipped.dedup();
    Ok(JobEnv {
        vars: kept,
        skipped,
    })
}

/// The plist of job `label` that runs `program args` with `env` ([`job_env`]): `RunAtLoad`
/// (it starts once bootstrapped), not `KeepAlive` (a daemon asked to stop stays stopped),
/// `ProcessType` `Interactive` (it runs agents for the user), [`EXIT_TIMEOUT`], its streams
/// `/dev/null` (the daemon writes its own log). Every string escaped ([`escape`]); an error
/// when `program` or an argument is not UTF-8 or not [`representable`].
pub fn plist(
    label: &str,
    program: &Path,
    args: &[OsString],
    env: &[(String, String)],
) -> anyhow::Result<String> {
    let string = |s: &str| format!("<string>{}</string>", escape(s));
    let mut argv = Vec::with_capacity(args.len() + 1);
    for (i, a) in std::iter::once(program.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .enumerate()
    {
        let what = || match i {
            0 => format!("the program {}", program.display()),
            i => format!("argument {i} of the box daemon"),
        };
        let a = a
            .to_str()
            .ok_or_else(|| anyhow!("{} is not UTF-8 text", what()))?;
        if !representable(a) {
            bail!("{} holds a character a plist can't", what());
        }
        argv.push(format!("\t\t{}\n", string(a)));
    }
    if !representable(label) {
        bail!("the label {label:?} holds a character a plist can't");
    }
    let mut vars = String::new();
    for (k, v) in env {
        if !representable(k) || !representable(v) {
            bail!("{k} can't be written in a plist");
        }
        vars.push_str(&format!(
            "\t\t<key>{}</key>\n\t\t{}\n",
            escape(k),
            string(v)
        ));
    }
    let null = string("/dev/null");
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \t<key>Label</key>\n\t{label}\n\
         \t<key>ProgramArguments</key>\n\t<array>\n{argv}\t</array>\n\
         \t<key>EnvironmentVariables</key>\n\t<dict>\n{vars}\t</dict>\n\
         \t<key>RunAtLoad</key>\n\t<true/>\n\
         \t<key>KeepAlive</key>\n\t<false/>\n\
         \t<key>AbandonProcessGroup</key>\n\t<false/>\n\
         \t<key>ProcessType</key>\n\t<string>Interactive</string>\n\
         \t<key>ExitTimeOut</key>\n\t<integer>{exit}</integer>\n\
         \t<key>StandardInPath</key>\n\t{null}\n\
         \t<key>StandardOutPath</key>\n\t{null}\n\
         \t<key>StandardErrorPath</key>\n\t{null}\n\
         </dict>\n\
         </plist>\n",
        label = string(label),
        argv = argv.concat(),
        exit = EXIT_TIMEOUT.as_secs(),
    ))
}

/// The note when the Mac has no GUI session and the daemon of box `box_name` is started from
/// ssh instead.
pub fn no_gui_session_note(box_name: &str) -> String {
    format!(
        "twr-box: this Mac has no one logged in at its screen: started the box daemon from ssh, \
         where Claude Code can't read its keychain login; log in at the Mac (or turn on \
         automatic login), then `twr box restart {box_name}`"
    )
}

/// The note when whether someone is logged in at the Mac's screen can't be told (`why`:
/// how `launchctl print gui/<uid>` failed) and the daemon of box `box_name` is started from
/// ssh instead.
pub fn unknown_session_note(box_name: &str, why: &str) -> String {
    format!(
        "twr-box: warning: couldn't tell whether anyone is logged in at the Mac ({why}): \
         started the box daemon from ssh, where Claude Code can't read its keychain login; \
         `twr box restart {box_name}` tries launchd again"
    )
}

/// The note when launchd could not start the daemon of box `box_name` (`why`), which is
/// started from ssh instead.
pub fn fallback_note(box_name: &str, why: &anyhow::Error) -> String {
    format!(
        "twr-box: warning: launchd could not start the box daemon in your GUI session ({why:#}): \
         started it from ssh instead, where Claude Code can't read its keychain login; `twr box \
         restart {box_name}` tries launchd again"
    )
}

/// A plist file, removed when dropped.
struct Written(PathBuf);

impl Drop for Written {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.0) {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.0.display(), error = %e, "removing a launchd job's plist failed");
            }
        }
    }
}

/// Writes `xml` into the new file `<dir>/<label>.plist` (mode 0600, created exclusively: never
/// what is there, a symbolic link included), synced.
fn write_plist(dir: &Path, label: &str, xml: &str) -> anyhow::Result<Written> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(format!("{label}.plist"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    let written = Written(path);
    f.write_all(xml.as_bytes())
        .and_then(|()| f.sync_all())
        .with_context(|| format!("writing {}", written.0.display()))?;
    Ok(written)
}

/// Removes the plists of `prefix` ([`is_plist_of`]) left in the daemon's data directory `dir`
/// by a relay that did not get to remove its own.
fn remove_abandoned_plists(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        if name.to_str().is_some_and(|n| is_plist_of(n, prefix))
            && e.file_type().is_ok_and(|t| t.is_file())
        {
            tracing::info!(path = %e.path().display(), "removing a launchd job's plist a relay left");
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// Starts the daemon of `opts` (`program args`) as a launchd job (see the module docs), its
/// notes to `note`, the job's environment from `env` (the relay's). `Ok(true)`: the job was
/// submitted (the caller waits for the daemon to answer); `Ok(false)`: it was not, and the
/// caller starts the daemon from ssh (the notes said why). With `forced`
/// (`TWR_BOX_LAUNCHER=launchd`) there is no `Ok(false)`: an error instead.
pub async fn start(
    opts: &RelayOptions,
    args: &[OsString],
    forced: bool,
    ctl: &Launchctl,
    env: Vec<(OsString, OsString)>,
    note: &mut (dyn FnMut(&str) + Send),
) -> anyhow::Result<bool> {
    let refused = |why: anyhow::Error| {
        why.context(format!(
            "starting the box daemon with launchd ({LAUNCHER_ENV}=launchd)"
        ))
    };
    let prefix = label_prefix(&opts.hub_key, &opts.box_name);
    // (Also when the daemon is started from ssh instead: no plist outlives its relay.)
    remove_abandoned_plists(&opts.paths.data_dir, &prefix);
    let listing = match ctl.gui_domain().await {
        GuiDomain::Session(listing) => listing,
        GuiDomain::NoSession if forced => {
            return Err(refused(anyhow!(
                "`launchctl print {}` found no such domain: no one is logged in at this Mac's \
                 screen (there is no GUI session to start it in)",
                ctl.domain
            )))
        }
        GuiDomain::NoSession => {
            note(&no_gui_session_note(&opts.box_name));
            return Ok(false);
        }
        GuiDomain::Unknown(why) if forced => {
            return Err(refused(anyhow!(
                "couldn't tell whether anyone is logged in at this Mac ({why})"
            )))
        }
        GuiDomain::Unknown(why) => {
            note(&unknown_session_note(&opts.box_name, &why));
            return Ok(false);
        }
    };
    let label = new_label(&prefix);
    for stale in stale_labels(&listing, &prefix, &label)
        .into_iter()
        .take(STALE_PER_START)
    {
        if ctl.state(&stale).await == JobState::Loaded {
            tracing::info!(label = %stale, "booting out a box daemon job that has ended");
            ctl.boot_out(&stale).await;
        }
    }
    let failed = match submit(opts, args, ctl, &label, env, note).await {
        Ok(()) => return Ok(true),
        Err(Submit::NotSubmitted(e)) => e,
        Err(Submit::Bootstrap(e)) => match after_failed_bootstrap(ctl.state(&label).await) {
            AfterBootstrap::Wait => {
                note(&format!(
                    "twr-box: {e:#}, but the box daemon's launchd job {label} is loaded: \
                     waiting for it"
                ));
                return Ok(true);
            }
            AfterBootstrap::Fallback => e,
            AfterBootstrap::BootOut => {
                ctl.boot_out(&label).await;
                if ctl.state(&label).await != JobState::Absent {
                    bail!(
                        "{e:#}, and whether its launchd job {label} is loaded can't be told, nor \
                         could it be booted out: not starting another box daemon beside one that \
                         may run (see `launchctl print {}/{label}` on the box)",
                        ctl.domain
                    );
                }
                e
            }
        },
    };
    if forced {
        return Err(refused(failed));
    }
    note(&fallback_note(&opts.box_name, &failed));
    Ok(false)
}

/// Why [`submit`] did not submit the job.
enum Submit {
    /// Nothing reached launchd (the plist could not be made or written).
    NotSubmitted(anyhow::Error),
    /// `launchctl bootstrap` failed or timed out: the job may be loaded all the same.
    Bootstrap(anyhow::Error),
}

/// Writes job `label`'s plist into the daemon's data directory, bootstraps it, and removes it
/// (also when anything failed).
async fn submit(
    opts: &RelayOptions,
    args: &[OsString],
    ctl: &Launchctl,
    label: &str,
    env: Vec<(OsString, OsString)>,
    note: &mut (dyn FnMut(&str) + Send),
) -> Result<(), Submit> {
    let JobEnv { vars, skipped } = job_env(env).map_err(Submit::NotSubmitted)?;
    for name in skipped {
        note(&format!(
            "twr-box: not passing {name} on to the box daemon's launchd job: its value can't be \
             written in a plist"
        ));
    }
    let xml = plist(label, &opts.program, args, &vars).map_err(Submit::NotSubmitted)?;
    let written = write_plist(&opts.paths.data_dir, label, &xml).map_err(Submit::NotSubmitted)?;
    let r = ctl.bootstrap(&written.0).await.map_err(Submit::Bootstrap);
    drop(written);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay::{DaemonPaths, Launcher};

    const KEY: [u8; 32] = [3u8; 32];

    /// The label of a start names the hub key and box (their socket hash) and is new each
    /// time; only labels of that shape are taken for this box's.
    #[test]
    fn labels_name_the_box_and_are_new_each_start() {
        let prefix = label_prefix(&KEY, "gpu");
        let hash = socket_hash(&twr_core::sealed::hub_id(&KEY), "gpu");
        assert_eq!(prefix, format!("tools.twr.box.{hash}."));
        let (a, b) = (new_label(&prefix), new_label(&prefix));
        assert_ne!(a, b);
        assert!(
            is_label_of(&a, &prefix) && a.len() == prefix.len() + 8,
            "{a}"
        );
        assert_ne!(label_prefix(&KEY, "gpu2"), prefix);
        assert_ne!(label_prefix(&[4u8; 32], "gpu"), prefix);
        assert!(is_label_of(&format!("{prefix}x"), &prefix));
        for bad in [
            "",
            "abc.def",
            "ab/cd",
            "0123456789abcdef0",
            "ab cd",
            "ab-cd",
        ] {
            assert!(!is_label_of(&format!("{prefix}{bad}"), &prefix), "{bad:?}");
        }
        assert!(!is_label_of("tools.twr.box.other.0123abcd", &prefix));
        assert!(is_plist_of(&format!("{a}.plist"), &prefix));
        assert!(!is_plist_of(&a, &prefix));
        assert!(!is_plist_of(&format!("{a}.plist.tmp"), &prefix));
    }

    /// Stale candidates: this box's labels the domain lists (whatever surrounds them), not
    /// another box's, not the label about to be submitted, each once.
    #[test]
    fn stale_labels_are_this_boxs_but_its_own() {
        let prefix = label_prefix(&KEY, "gpu");
        let own = format!("{prefix}0000aaaa");
        let listing = format!(
            "gui/501 = {{\n\tservices = {{\n\t\t     0      -       {prefix}1111bbbb\n\t\t  \
             4242      -       {prefix}2222cccc\n\t\t     0      -       {own}\n\t\t     0      \
             -       com.apple.foo\n\t\t     0      -       tools.twr.box.ffffffffffffffff.\
             3333dddd\n\t}}\n\tdisabled services = {{\n\t\t\"{prefix}1111bbbb\" => \
             disabled\n\t}}\n}}\n"
        );
        assert_eq!(
            stale_labels(&listing, &prefix, &own),
            [format!("{prefix}1111bbbb"), format!("{prefix}2222cccc")]
        );
    }

    /// A job's state from `launchctl print gui/<uid>/<label>`: running or not by its first
    /// `state = ` line, none by exit 113, not known otherwise; and what a start does after a
    /// failed bootstrap by it.
    #[test]
    fn job_states_and_what_follows_a_failed_bootstrap() {
        let print = |state: &str| {
            format!(
                "gui/501/tools.twr.box.x.1 = {{\n\tactive count = 1\n\tpath = (submitted by \
                 launchctl)\n\tstate = {state}\n\n\tendpoints = {{\n\t\tstate = active\n\t}}\n}}\n"
            )
        };
        assert_eq!(
            job_state(Some(0), print("running").as_bytes()),
            JobState::Running
        );
        assert_eq!(
            job_state(Some(0), print("not running").as_bytes()),
            JobState::Loaded
        );
        assert_eq!(job_state(Some(0), b""), JobState::Loaded);
        assert_eq!(job_state(Some(113), b""), JobState::Absent);
        for code in [Some(1), Some(5), None] {
            assert_eq!(
                job_state(code, print("running").as_bytes()),
                JobState::Unknown
            );
        }
        assert_eq!(
            after_failed_bootstrap(JobState::Running),
            AfterBootstrap::Wait
        );
        assert_eq!(
            after_failed_bootstrap(JobState::Loaded),
            AfterBootstrap::Wait
        );
        assert_eq!(
            after_failed_bootstrap(JobState::Absent),
            AfterBootstrap::Fallback
        );
        assert_eq!(
            after_failed_bootstrap(JobState::Unknown),
            AfterBootstrap::BootOut
        );
    }

    /// Only the allowlisted variables reach the job; one whose value a plist can't hold is
    /// skipped by name (never its value), but PATH and HOME, which fail the launchd start.
    #[test]
    fn the_jobs_environment_is_an_allowlist() {
        use std::os::unix::ffi::OsStringExt;
        let vars = |extra: Vec<(OsString, OsString)>| {
            [
                ("PATH", "/Users/u/.local/bin:/usr/bin"),
                ("HOME", "/Users/u"),
                ("ANTHROPIC_API_KEY", "sk-secret"),
                ("OPENAI_API_KEY", "sk-other"),
                ("SSH_AUTH_SOCK", "/tmp/agent"),
                ("SSH_CONNECTION", "1.2.3.4 5 6.7.8.9 22"),
                ("TWR_RUNNER_TOKEN", "tok"),
                ("LC_ALL", "en_US.UTF-8"),
                ("LC_", "x"),
                ("lc_lower", "x"),
                ("XDG_DATA_HOME", "/Users/u/d"),
                ("TWR_CLAUDE_BIN", "/opt/claude"),
                ("TWR_BOX_PROBE", "off"),
                ("TMPDIR", "/var/folders/x/T/"),
                ("XPC_SERVICE_NAME", "0"),
                ("INVOCATION_ID", "abc"),
            ]
            .into_iter()
            .map(|(k, v)| (OsString::from(k), OsString::from(v)))
            .chain(extra)
            .collect::<Vec<_>>()
        };
        let JobEnv {
            vars: kept,
            skipped,
        } = job_env(vars(vec![
            ("LANG".into(), OsString::from_vec(b"en\xff".to_vec())),
            ("TZ".into(), "a\u{1}b".into()),
        ]))
        .unwrap();
        let names: Vec<&str> = kept.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            names,
            [
                "HOME",
                "LC_ALL",
                "PATH",
                "TMPDIR",
                "TWR_BOX_PROBE",
                "TWR_CLAUDE_BIN",
                "XDG_DATA_HOME"
            ]
        );
        assert_eq!(skipped, ["LANG", "TZ"]);
        for bad in [
            ("PATH", OsString::from_vec(b"/bin\xff".to_vec())),
            ("HOME", OsString::from("/Users/\u{0}u")),
        ] {
            let e = job_env(vars(vec![(bad.0.into(), bad.1)]))
                .unwrap_err()
                .to_string();
            assert!(
                e.contains(bad.0) && !e.contains("/bin") && !e.contains("Users"),
                "{e}"
            );
        }
    }

    /// The plist: every string escaped, the daemon's command line and environment, started at
    /// load and never kept alive, the daemon's whole shutdown as its exit timeout, its streams
    /// /dev/null; a command line a plist can't hold is refused.
    #[test]
    fn the_plist_holds_the_command_escaped() {
        assert_eq!(EXIT_TIMEOUT, Duration::from_secs(330));
        assert_eq!(
            escape("a&b<c>d\"e'f\rg\th"),
            "a&amp;b&lt;c&gt;d&quot;e&apos;f&#13;g\th"
        );
        assert!(representable("tab\tnew\nline é 漢 🦀"));
        for bad in ["\u{0}", "\u{1b}[0m", "\u{fffe}", "\u{ffff}"] {
            assert!(!representable(bad), "{bad:?}");
        }
        let xml = plist(
            "tools.twr.box.0123456789abcdef.0a1b2c3d",
            Path::new("/Users/u/twr builds/<b>&/twr"),
            &["runner".into(), "--daemon".into(), "--box=a'b\"c".into()],
            &[
                ("HOME".into(), "/Users/u".into()),
                ("PATH".into(), "/a&b:/usr/bin".into()),
            ],
        )
        .unwrap();
        for want in [
            "<key>Label</key>\n\t<string>tools.twr.box.0123456789abcdef.0a1b2c3d</string>",
            "<array>\n\t\t<string>/Users/u/twr builds/&lt;b&gt;&amp;/twr</string>\n\t\t\
             <string>runner</string>\n\t\t<string>--daemon</string>\n\t\t\
             <string>--box=a&apos;b&quot;c</string>\n\t</array>",
            "<dict>\n\t\t<key>HOME</key>\n\t\t<string>/Users/u</string>\n\t\t<key>PATH</key>\n\t\t\
             <string>/a&amp;b:/usr/bin</string>\n\t</dict>",
            "<key>RunAtLoad</key>\n\t<true/>",
            "<key>KeepAlive</key>\n\t<false/>",
            "<key>AbandonProcessGroup</key>\n\t<false/>",
            "<key>ProcessType</key>\n\t<string>Interactive</string>",
            "<key>ExitTimeOut</key>\n\t<integer>330</integer>",
            "<key>StandardInPath</key>\n\t<string>/dev/null</string>",
            "<key>StandardOutPath</key>\n\t<string>/dev/null</string>",
            "<key>StandardErrorPath</key>\n\t<string>/dev/null</string>",
        ] {
            assert!(xml.contains(want), "{want}\nin\n{xml}");
        }
        assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist"));
        assert!(xml.ends_with("</dict>\n</plist>\n"));
        let e = plist("l", Path::new("/t"), &["\u{1}".into()], &[]).unwrap_err();
        assert!(e.to_string().contains("argument 1"), "{e}");
        use std::os::unix::ffi::OsStringExt;
        let odd = OsString::from_vec(b"/t\xff".to_vec());
        assert!(plist("l", Path::new(&odd), &[], &[]).is_err());
    }

    /// A fake `launchctl` (`sh <script>`) for [`start`]: it logs each call to `calls`; `print
    /// gui/501` exits with the file `gui-exit` when there is one, else succeeds when the file
    /// `gui` exists (printing `listing`), else exits 113; `print
    /// gui/501/<label>` prints `state = <the file state.<label>>` when there is one, exits 113
    /// once the job was booted out, else with the file `print-exit` (else 113); `bootstrap`
    /// copies the plist to `submitted` and then does as the file `bootstrap` says (`ok`;
    /// `fail`; `fail-loaded`: fails, the job loaded); `bootout` removes the job unless the file
    /// `stuck` exists.
    fn fake(dir: &Path) -> Launchctl {
        let script = dir.join("launchctl.sh");
        std::fs::write(
            &script,
            r#"d=$(dirname "$0")
echo "$*" >> "$d/calls"
case "$1" in
print)
    case "$2" in
    gui/501)
        if [ -f "$d/gui-exit" ]; then exit "$(cat "$d/gui-exit")"; fi
        [ -f "$d/gui" ] || exit 113; cat "$d/listing" 2>/dev/null; exit 0 ;;
    gui/501/*)
        l=${2#gui/501/}
        if [ -f "$d/state.$l" ]; then echo "	state = $(cat "$d/state.$l")"; exit 0; fi
        [ -f "$d/gone.$l" ] && exit 113
        if [ -f "$d/print-exit" ]; then exit "$(cat "$d/print-exit")"; fi
        exit 113 ;;
    esac ;;
bootstrap)
    cp "$3" "$d/submitted"
    l=$(basename "$3" .plist)
    case "$(cat "$d/bootstrap")" in
    ok) echo running > "$d/state.$l"; exit 0 ;;
    fail) exit 5 ;;
    fail-loaded) echo running > "$d/state.$l"; exit 5 ;;
    esac ;;
bootout)
    [ -f "$d/stuck" ] && exit 1
    l=${2#gui/501/}
    rm -f "$d/state.$l"; : > "$d/gone.$l"; exit 0 ;;
esac
exit 1
"#,
        )
        .unwrap();
        Launchctl {
            command: vec!["/bin/sh".into(), script.into()],
            domain: "gui/501".into(),
        }
    }

    fn opts(dir: &Path) -> RelayOptions {
        let data = dir.join("data");
        std::fs::create_dir_all(&data).unwrap();
        RelayOptions {
            box_name: "gpu".into(),
            hub_key: KEY,
            replace: false,
            restart: false,
            paths: DaemonPaths {
                socket: dir.join("d.sock"),
                data_dir: data.clone(),
                lock: dir.join("box.lock"),
                log: data.join("daemon.log"),
            },
            program: "/Users/u/twr".into(),
            launcher: Launcher::Auto,
            start_timeout: Duration::from_secs(1),
            stop_timeout: Duration::from_secs(1),
            progress_every: crate::relay::PROGRESS_EVERY,
            build: None,
        }
    }

    fn env() -> Vec<(OsString, OsString)> {
        vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), "/Users/u".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-secret".into()),
        ]
    }

    /// Runs [`start`] with the fake `launchctl` of `dir`: its result and notes.
    async fn run(dir: &Path, forced: bool) -> (anyhow::Result<bool>, Vec<String>) {
        let o = opts(dir);
        let mut notes = Vec::new();
        let r = start(
            &o,
            &o.daemon_args(),
            forced,
            &fake(dir),
            env(),
            &mut |l: &str| notes.push(l.to_string()),
        )
        .await;
        (r, notes)
    }

    fn calls(dir: &Path) -> String {
        std::fs::read_to_string(dir.join("calls")).unwrap_or_default()
    }

    /// No GUI session: the daemon is started from ssh with the note that names the restart;
    /// with launchd forced, an error. Nothing is written or submitted, and a plist a relay
    /// left is removed all the same (another box's is kept).
    #[tokio::test]
    async fn without_a_gui_session_it_falls_back_unless_forced() {
        let d = tempfile::tempdir().unwrap();
        let data = opts(d.path()).paths.data_dir;
        let left = data.join(format!("{}9999eeee.plist", label_prefix(&KEY, "gpu")));
        std::fs::write(&left, "old").unwrap();
        let keep = data.join("tools.twr.box.ffffffffffffffff.3333dddd.plist");
        std::fs::write(&keep, "another box's").unwrap();
        let (r, notes) = run(d.path(), false).await;
        assert!(!left.exists() && keep.exists());
        assert!(!r.unwrap());
        assert_eq!(notes, [no_gui_session_note("gpu")]);
        assert!(notes[0].contains("`twr box restart gpu`"), "{notes:?}");
        let (r, notes) = run(d.path(), true).await;
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("TWR_BOX_LAUNCHER=launchd") && e.contains("no one is logged in"),
            "{e}"
        );
        assert!(notes.is_empty());
        assert!(!calls(d.path()).contains("bootstrap"));
    }

    /// Only exit 113 says no one is logged in at the Mac's screen: a `launchctl` that fails
    /// otherwise, or can't be run, says nothing of it. The daemon is then started from ssh
    /// with a note that says that couldn't be told, and why (forced: an error that does).
    #[tokio::test]
    async fn a_failed_launchctl_is_not_taken_for_no_gui_session() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("gui"), "").unwrap();
        std::fs::write(d.path().join("gui-exit"), "1").unwrap();
        let (r, notes) = run(d.path(), false).await;
        assert!(!r.unwrap());
        assert_eq!(
            notes,
            [unknown_session_note(
                "gpu",
                "`launchctl print gui/501` exited 1"
            )]
        );
        assert!(
            notes[0].contains("couldn't tell whether anyone is logged in at the Mac")
                && notes[0].contains("`twr box restart gpu`")
                && !notes[0].contains("has no one logged in"),
            "{notes:?}"
        );
        let (r, notes) = run(d.path(), true).await;
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("couldn't tell whether anyone is logged in") && e.contains("exited 1"),
            "{e}"
        );
        assert!(notes.is_empty());
        assert!(!calls(d.path()).contains("bootstrap"));
        // Not there at all.
        let o = opts(d.path());
        let missing = Launchctl {
            command: vec![d.path().join("no-launchctl").into()],
            domain: "gui/501".into(),
        };
        let mut notes = Vec::new();
        let r = start(
            &o,
            &o.daemon_args(),
            false,
            &missing,
            env(),
            &mut |l: &str| notes.push(l.to_string()),
        )
        .await;
        assert!(!r.unwrap());
        assert!(
            notes[0].contains("couldn't tell") && notes[0].contains("could not be run"),
            "{notes:?}"
        );
        assert!(matches!(missing.gui_domain().await, GuiDomain::Unknown(_)));
        let e = missing
            .bootstrap(Path::new("/x.plist"))
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "`launchctl bootstrap gui/501` could not be run (is `/bin/launchctl` there?)"
        );
    }

    /// With a GUI session: ended jobs of this box are booted out (not a running one, not
    /// another box's), a plist a relay left is removed, and the job is bootstrapped from a
    /// plist (0600, in the data directory) holding the daemon's command line and only the
    /// allowlisted environment, removed afterwards.
    #[tokio::test]
    async fn a_start_cleans_up_then_submits_the_job() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        let prefix = label_prefix(&KEY, "gpu");
        let (ended, running) = (format!("{prefix}1111bbbb"), format!("{prefix}2222cccc"));
        let other = "tools.twr.box.ffffffffffffffff.3333dddd";
        std::fs::write(dir.join("gui"), "").unwrap();
        std::fs::write(
            dir.join("listing"),
            format!(
                "\tservices = {{\n\t\t0 - {ended}\n\t\t42 - {running}\n\t\t0 - {other}\n\t}}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join(format!("state.{ended}")), "not running").unwrap();
        std::fs::write(dir.join(format!("state.{running}")), "running").unwrap();
        std::fs::write(dir.join(format!("state.{other}")), "not running").unwrap();
        std::fs::write(dir.join("bootstrap"), "ok").unwrap();
        let o = opts(dir);
        let left = o.paths.data_dir.join(format!("{prefix}9999eeee.plist"));
        std::fs::write(&left, "old").unwrap();
        let keep = o
            .paths
            .data_dir
            .join("tools.twr.box.ffffffffffffffff.3333dddd.plist");
        std::fs::write(&keep, "another box's").unwrap();
        let (r, notes) = run(dir, false).await;
        assert!(r.unwrap(), "{notes:?}");
        assert!(notes.is_empty(), "{notes:?}");
        let calls = calls(dir);
        assert!(
            calls.contains(&format!("bootout gui/501/{ended}")),
            "{calls}"
        );
        assert!(
            !calls.contains(&format!("bootout gui/501/{running}")),
            "{calls}"
        );
        assert!(
            !calls.contains(&format!("bootout gui/501/{other}")),
            "{calls}"
        );
        assert!(!left.exists() && keep.exists());
        let submitted = std::fs::read_to_string(dir.join("submitted")).unwrap();
        let line = calls.lines().find(|l| l.starts_with("bootstrap")).unwrap();
        let path = line.strip_prefix("bootstrap gui/501 ").unwrap();
        assert!(Path::new(path).starts_with(&o.paths.data_dir), "{path}");
        assert!(!Path::new(path).exists(), "the plist was left behind");
        let label = Path::new(path).file_stem().unwrap().to_str().unwrap();
        assert!(is_label_of(label, &prefix), "{label}");
        assert!(submitted.contains(&format!("<string>{label}</string>")));
        assert!(submitted.contains("<string>/Users/u/twr</string>"));
        assert!(submitted.contains("<string>--box=gpu</string>"));
        assert!(submitted.contains("<key>PATH</key>"));
        assert!(!submitted.contains("sk-secret") && !submitted.contains("ANTHROPIC"));
    }

    /// The plist is created 0600 and exclusively: never over what is there.
    #[test]
    fn the_plist_file_is_new_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        let w = write_plist(d.path(), "l", "<plist/>").unwrap();
        let mode = std::fs::metadata(&w.0).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(write_plist(d.path(), "l", "x").is_err());
        let path = w.0.clone();
        drop(w);
        assert!(!path.exists());
        std::os::unix::fs::symlink(d.path().join("target"), d.path().join("s.plist")).unwrap();
        assert!(write_plist(d.path(), "s", "x").is_err());
        assert!(!d.path().join("target").exists());
    }

    /// A failed bootstrap: a job loaded all the same is waited for; one not loaded falls back
    /// (forced: an error); one whose state can't be told is booted out, then falls back once
    /// it is gone, and is an error while it may still be there. The plist goes either way.
    #[tokio::test]
    async fn a_failed_bootstrap_is_checked_before_the_fallback() {
        // Loaded all the same.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("gui"), "").unwrap();
        std::fs::write(d.path().join("bootstrap"), "fail-loaded").unwrap();
        let (r, notes) = run(d.path(), true).await;
        assert!(r.unwrap());
        assert!(notes[0].contains("is loaded: waiting for it"), "{notes:?}");
        // Not loaded.
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("gui"), "").unwrap();
        std::fs::write(d.path().join("bootstrap"), "fail").unwrap();
        let (r, notes) = run(d.path(), false).await;
        assert!(!r.unwrap());
        assert!(
            notes[0].contains("exit 5") && notes[0].contains("`twr box restart gpu`"),
            "{notes:?}"
        );
        assert!(!calls(d.path()).contains("bootout"));
        let (r, _) = run(d.path(), true).await;
        assert!(format!("{:#}", r.unwrap_err()).contains("exit 5"));
        // Not known: booted out, then gone (fallback), or not (an error).
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("gui"), "").unwrap();
        std::fs::write(d.path().join("bootstrap"), "fail").unwrap();
        std::fs::write(d.path().join("print-exit"), "1").unwrap();
        let (r, notes) = run(d.path(), false).await;
        assert!(!r.unwrap());
        assert!(
            notes[0].contains("started it from ssh instead"),
            "{notes:?}"
        );
        assert!(calls(d.path()).contains("bootout gui/501/tools.twr.box."));
        std::fs::write(d.path().join("stuck"), "").unwrap();
        let (r, notes) = run(d.path(), false).await;
        let e = format!("{:#}", r.unwrap_err());
        assert!(
            e.contains("can't be told") && e.contains("not starting another"),
            "{e}"
        );
        assert!(notes.is_empty(), "{notes:?}");
        let left: Vec<_> = std::fs::read_dir(opts(d.path()).paths.data_dir)
            .unwrap()
            .map(|f| f.unwrap().path())
            .collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }
}
