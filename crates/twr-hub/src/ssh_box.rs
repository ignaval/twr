//! The hub's ssh supervisor of an SSH box (`[boxes.<name>] transport = "ssh"`): one task per
//! box, in the default workspace's hub only (a named workspace's hub never dials boxes, as it
//! never listens), started with the hub or by a live add (`box.add`) and ended by the hub's
//! shutdown, which kills its ssh children. It keeps the box connected:
//!
//! 1. **Probe** ([`PROBE_TIMEOUT`]): `ssh <box> -- sh -c <probe>` ([`probe_script`]) prints the
//!    box's `uname -s` and `uname -m`, its data directory (`$XDG_DATA_HOME` when set to an
//!    absolute path, else `$HOME/.local/share`: the rule the relay on the box follows) and the
//!    twr builds installed there (`<data>/twr/runner/builds/<build id>/twr`, executable).
//!    What a login script prints around it is ignored ([`parse_probe`]). The builds tree must
//!    be safe to use ([`TREE_FNS`]: the user's alone, as are the directories above it): one
//!    that is not makes the box `needs_install` with the path and why, and a build whose
//!    directory or executable is not is left out (a note says which).
//! 2. **Which build** ([`choose_build`]): on a box of the hub's own OS and CPU
//!    ([`os_name`], [`arch_name`]), the hub's own ([`twr_core::build_id`]); otherwise
//!    `[boxes.<name>] build` (what `twr box install` installed), else the box needs an install
//!    (state `needs_install`, tried again every [`INSTALL_RETRY`] or when `box.install` wakes
//!    the supervisor).
//! 3. **Upload** (the hub's own build missing on the box): the hub's running executable is
//!    streamed through ssh's stdin into a temporary file next to its place ([`upload_script`]),
//!    whose size, SHA-256 (when the box has `sha256sum` or `shasum`; else size and version
//!    decide; one that is there but fails refuses the upload) and `--version` are checked
//!    before it is moved into place. A mismatch removes the file and leaves the box
//!    `needs_install` with the reason. Once a session is welcomed, the hub records the builds
//!    it uses there (launched, running, configured) as its builds reference
//!    ([`twr_core::BUILD_REFS_DIR`], one per hub instance and box: the builds directory is
//!    shared by every hub that reaches the box as that user; the relay adds its own build as
//!    it starts, `twr box install` the one it installs) and removes the builds no reference
//!    names, but one more and any an upload may be at ([`prune_script`]).
//! 4. **Connect**: `ssh <box> -- exec <build> runner --stdio --box <name> --hub-key <hex>
//!    [--replace]` ([`connect_command`]): the relay on the box, which starts the box daemon if
//!    none runs. Its stderr lines are the box's `notes` (the latest [`NOTES_KEPT`]; the newest
//!    that reads like an error is its `last_error`). The hub opens the sealed channel over the
//!    child's stdout and stdin ([`twr_core::sealed::hub_side`], [`OPEN_TIMEOUT`], which covers
//!    starting the daemon), accepts a WebSocket inside it with the small pre-authentication
//!    limits, and serves it as the box's runner connection (`Hub::serve_sealed_runner`).
//! 5. When the session ends: the ssh child is killed (its whole process group), and the next
//!    attempt follows a backoff ([`backoff_delay`]: 1 s doubling, at most 5 s apart during the
//!    first minute of an outage, then at most 30 s; reset by a welcomed session, as a token
//!    runner's). A daemon that speaks another runner protocol was refused at its hello: the
//!    next attempt passes `--replace` at once, so the relay replaces it with the build the hub
//!    launches (its runtimes end; the reconciliation closes their threads with crash cards).
//!    A daemon of another build that speaks the same protocol is served as it is and marked
//!    `switch_pending`, until the hub switches it (below). An attempt that is to replace the
//!    daemon and fails before a session keeps that for the next one.
//!
//! **Switching builds** (`switch_pending`: the daemon authenticated, so it speaks the hub's
//! protocol, but runs another build than the one launched; or, on a box of another OS or CPU
//! that runs its configured build, `twr box install` recorded another one meanwhile, which
//! `box.install` hands the session: [`SshBox::set_build`]): the box keeps working on the build
//! it runs, and is switched to the launched (or newly configured) one once it is quiet,
//! without crash cards and without losing a prompt (the hub's part is `hub::box_switch`):
//!
//! 1. While the session lasts, every [`SwitchTiming::check`] (30 s) the supervisor asks the
//!    hub whether the box is quiet (`Hub::box_switch_quiet`): online, no repository change of
//!    it in flight or unsettled, no new thread being made on it and no deletion, trash restore
//!    or attach of its threads being checked (they took the box's operation with their gate
//!    check, `Core::reserve_box`), and of each of its threads no running turn, no queued prompt
//!    (a queue a failure holds waits for the user anyway) or due retry, no open item its
//!    runtime raised, no background task of its runtime, no attach granted or asked for, no
//!    park, resume, deletion or trim under way, no runtime being stopped, restarted or
//!    exiting, and no answer on its way to the box. A box that is not quiet, or on whose
//!    threads anything was recorded since the previous check, starts counting again.
//! 2. Quiet for [`SwitchTiming::quiet`] (2 minutes): the hub sets the box's `switching` gate
//!    under the core lock in the same step as it checks that again (`Hub::begin_box_switch`;
//!    anything not quiet any more, or recorded since: no gate, and the count starts again),
//!    and stops every live runtime of the box through the stop machinery (`hub::restart`):
//!    durable stop requests that keep the queues and the box's `stop` commands, in one batch;
//!    each exit closes as a `thread.stopped` with `ext.twr.reason` = `box_upgrade`, which does
//!    not stop the thread. The gate is the hub's, in memory only: while it is set the box's
//!    threads take no turn and start no runtime (prompts are accepted and queued, as for an
//!    offline box), and a `thread.stop` meanwhile wins (the thread stays stopped).
//! 3. Once every stopped runtime's exit is recorded (`Hub::box_switch_drained`), or after
//!    [`SwitchTiming::drain`] (2 minutes), the supervisor ends the session
//!    (`Hub::end_box_session`) and connects again at once with `--replace`: the relay stops
//!    the old daemon, whose spool holds only frames the hub acked or the exits of runtimes
//!    that outlived the drain, and starts the launched build on the same data directory. A
//!    session that ends by itself during the drain (the box went offline) keeps the gate, and
//!    the next attempts replace the daemon too.
//! 4. The reconciliation of the box's next connection clears the gate and dispatches its
//!    threads' queues (`hub::runners`): each thread's next prompt resumes its session on the
//!    new daemon.
//!
//! The notes get `switching to build <id>` when the gate is set and `now on build <id>` when it
//! is cleared; `hub.info` shows the box `switching` meanwhile. A hub restart during the switch
//! forgets the gate and the stops' reason (the durable stop requests have no room for it): the
//! runtimes' exits close their threads as plainly stopped (no crash card; their queues are
//! kept and dispatched), and the new hub's supervisor finds the box `switch_pending` again.
//!
//! **ssh** ([`ssh_args`]): `TWR_SSH`, else `ssh` on `PATH`, with the hub's environment (its
//! `SSH_AUTH_SOCK` among it), never prompting (`BatchMode=yes`), with forwarding of every kind
//! and local commands turned off so that the user's `~/.ssh/config` cannot add any
//! ([`SSH_OPTIONS`] before the box's `ssh` words, [`SSH_NO_FORWARDING`] after them), the
//! box's words in between (refused when they forward anything or change how ssh runs:
//! [`twr_core::config::check_ssh_words`], at admission and again here), and one remote
//! command whose every variable part is shell-quoted. What it prints is read with limits
//! ([`SSH_OUTPUT_MAX`] per stream of a script's ssh, lines of [`NOTE_MAX_BYTES`] at most
//! from the relay, [`SESSION_NOTES_MAX`] of them per session). Each
//! child runs in a process group of its own (a terminal's signals to the hub do not reach it)
//! and is killed with that group when it is dropped. ssh's own failures (exit 255: a refused
//! key, an unknown host key, a name that does not resolve) become a plain `last_error`
//! ([`ssh_failure`]).
//!
//! **State** ([`Status`], `hub.info` `boxes[]`): `connecting` (probing, installing, opening,
//! or connected and not ready yet), `online` (the box is ready) and `switching` (its gate is
//! set): the hub's, not this module's, to say; `offline` (between attempts), `needs_install`,
//! `error` (the hub's box key cannot be loaded: every SSH box shows it); with the last error,
//! the notes, the box's OS and CPU, the build launched and `switch_pending`.
//!
//! **Tests** dial a box through [`HubOptions::box_dialer`](crate::HubOptions::box_dialer)
//! instead of ssh: no probe, upload or relay; the dialer gives the stream to the daemon and the
//! build the hub is to take for the one it launched (so a test makes a daemon "of another
//! build"), and is told whether the attempt replaces the daemon.

use std::collections::{BTreeSet, VecDeque};
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt,
};
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, watch, Notify};
use twr_core::config::BoxConfig;
use twr_core::sealed::{DaemonHello, HubKey};

use crate::hub::{HubState, SealedEnd, WeakHub};
use crate::remote::{sh_quote, ssh_words, RemoteBox, SSH_NO_FORWARDING};
use crate::Hub;

/// How long the probe may take.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an upload of the hub's executable may take, its checks included.
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// How long the box may take to open the sealed channel once ssh runs: connecting, starting
/// the daemon when none runs (the relay waits up to 15 s for it) and the key exchange.
pub const OPEN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the WebSocket handshake inside the channel may take.
pub const WS_TIMEOUT: Duration = Duration::from_secs(15);

/// How often a box that needs an install is looked at again (unless `box.install` wakes it).
pub const INSTALL_RETRY: Duration = Duration::from_secs(300);

/// The relay's stderr lines kept as the box's notes.
pub const NOTES_KEPT: usize = 20;

/// The longest line read from the relay's stderr, and so the longest note: a longer line is
/// cut while it is read ([`read_line_capped`]), the rest of it skipped.
pub const NOTE_MAX_BYTES: usize = 1024;

/// The most lines of the relay's stderr one session takes as notes (and logs): the relay says
/// a few when it starts; once a session's are used up, its stderr is still read (so ssh never
/// blocks on it) but dropped, with one note saying so.
const SESSION_NOTES_MAX: usize = 1000;

/// The most bytes read from each of stdout and stderr of an ssh that runs a script on the box
/// (probe, upload, prune; `twr box add` and `twr box install` too): a box that prints more (a
/// login script without end, say) fails the attempt, and its ssh is killed.
pub const SSH_OUTPUT_MAX: usize = 64 * 1024;

/// While an outage is younger than this, attempts are at most [`SHORT_OUTAGE_CAP`] apart.
const SHORT_OUTAGE: Duration = Duration::from_secs(60);

/// Longest wait between attempts during a short outage.
const SHORT_OUTAGE_CAP: Duration = Duration::from_secs(5);

/// Longest wait between attempts.
const LONG_OUTAGE_CAP: Duration = Duration::from_secs(30);

/// The first wait between attempts.
const BACKOFF_MIN: Duration = Duration::from_secs(1);

/// The hub's box key, in its data directory.
pub const BOX_KEY_FILE: &str = "box-key";

/// How long a box whose daemon runs another build must be quiet before it is switched.
pub const SWITCH_QUIET: Duration = Duration::from_secs(120);

/// How often a box whose daemon runs another build is checked for quiet.
pub const SWITCH_CHECK: Duration = Duration::from_secs(30);

/// Longest wait for the exits of the runtimes a switch stopped.
pub const SWITCH_DRAIN: Duration = Duration::from_secs(120);

/// How often the exits of the runtimes a switch stopped are looked for.
const DRAIN_POLL: Duration = Duration::from_millis(250);

/// When a box whose daemon runs another build than the one launched is switched to it (see
/// the module docs; [`crate::HubOptions::box_switch`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwitchTiming {
    /// How long the box must have been quiet ([`SWITCH_QUIET`]).
    pub quiet: Duration,
    /// How often it is checked ([`SWITCH_CHECK`]).
    pub check: Duration,
    /// Longest wait for the stopped runtimes' exits ([`SWITCH_DRAIN`]).
    pub drain: Duration,
}

impl Default for SwitchTiming {
    fn default() -> Self {
        SwitchTiming {
            quiet: SWITCH_QUIET,
            check: SWITCH_CHECK,
            drain: SWITCH_DRAIN,
        }
    }
}

/// A stream to a box's daemon: what ssh's stdout and stdin are to the hub.
pub trait BoxIo: AsyncRead + AsyncWrite + Unpin + Send {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send> BoxIo for T {}

/// [`crate::HubOptions::box_dialer`] (tests): given the box's name and whether the attempt
/// replaces its daemon (as `--replace` would), a stream to the daemon and the build the hub
/// takes for the one it launched there.
pub type BoxDialer = Arc<
    dyn Fn(String, bool) -> BoxFuture<'static, std::io::Result<(Box<dyn BoxIo>, String)>>
        + Send
        + Sync,
>;

/// Options of every background ssh, before the box's own words (see the module docs): ssh
/// takes the first value of a `-o` option it is given, so these hold over the box's words
/// (which may not name them anyway, [`twr_core::config::check_ssh_words`]) and over your ssh
/// config files. [`SSH_NO_FORWARDING`] follows the box's words.
pub const SSH_OPTIONS: &[&str] = &[
    "-T",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=20",
    "-o",
    "ServerAliveInterval=15",
    "-o",
    "ServerAliveCountMax=3",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "Tunnel=no",
];

/// What the probe's output starts with (what comes before it is a login script's).
const PROBE_MARK: &str = "twr-probe 1";

/// What the upload script's answer lines start with.
const UPLOAD_MARK: &str = "twr-upload: ";

/// The exit status of an upload script ([`upload_script`], [`install_script`],
/// [`release_install_script`]) when what arrived was refused (its answer says why).
pub const UPLOAD_MISMATCH: i32 = 3;

/// The ssh program: `TWR_SSH` when set and not empty, else `ssh`.
pub fn ssh_program() -> OsString {
    std::env::var_os("TWR_SSH")
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "ssh".into())
}

/// The arguments of a background ssh to the box whose `ssh` setting is `ssh`, running
/// `remote` there (one argument: ssh hands it to the box's shell as it is): [`SSH_OPTIONS`],
/// the box's words ([`ssh_words`]: an error when they forward anything or change what ssh
/// runs or how), [`SSH_NO_FORWARDING`], `--` and `remote`.
pub fn ssh_args(ssh: &str, remote: &str) -> Result<Vec<String>, String> {
    ssh_args_with(SSH_OPTIONS, ssh, remote)
}

/// [`ssh_args`] for an ssh run in the user's terminal (`twr box add`, `twr box install`): the
/// same options but `BatchMode=yes`, so ssh may ask about an unknown host key or for a key's
/// passphrase.
pub fn interactive_ssh_args(ssh: &str, remote: &str) -> Result<Vec<String>, String> {
    let mut options: Vec<&str> = Vec::with_capacity(SSH_OPTIONS.len());
    for o in SSH_OPTIONS {
        if *o == "BatchMode=yes" && options.last() == Some(&"-o") {
            options.pop();
        } else {
            options.push(o);
        }
    }
    ssh_args_with(&options, ssh, remote)
}

fn ssh_args_with(options: &[&str], ssh: &str, remote: &str) -> Result<Vec<String>, String> {
    let words = ssh_words(ssh)?;
    Ok(options
        .iter()
        .chain(&words)
        .chain(SSH_NO_FORWARDING)
        .map(|s| s.to_string())
        .chain(["--".to_string(), remote.to_string()])
        .collect())
}

/// A background ssh ([`ssh_args`]; an error when the box's words are refused) of
/// [`ssh_program`]: no stdin, stdout and stderr piped, its own process group, the child killed
/// when dropped (the hub's own ssh children also kill their group).
pub fn ssh_command(ssh: &str, remote: &str) -> Result<Command, String> {
    let mut c = Command::new(ssh_program());
    c.args(ssh_args(ssh, remote)?)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    Ok(c)
}

/// `script` run by `sh` on the box, whatever the user's login shell runs it with.
pub fn sh_c(script: &str) -> String {
    format!("sh -c {}", sh_quote(script))
}

/// A spawned ssh ([`ssh_command`]: the leader of a process group of its own) whose whole
/// process group is killed when it is dropped unreaped (the child itself with it:
/// `kill_on_drop`), and when it ends ([`Ssh::finish`]).
pub struct Ssh {
    /// The ssh process.
    pub child: Child,
}

impl Ssh {
    /// Spawns `c`, which makes its child a process group's leader (`process_group(0)`).
    pub fn spawn(mut c: Command) -> std::io::Result<Ssh> {
        Ok(Ssh { child: c.spawn()? })
    }

    /// Waits for ssh to end, kills what is left of its process group, then reaps it
    /// ([`finish_group`]): its exit status.
    pub async fn finish(&mut self) -> std::io::Result<std::process::ExitStatus> {
        finish_group(&mut self.child).await
    }
}

/// Waits for `child`, spawned as the leader of a process group of its own, to exit, kills
/// what is left of that group (helpers a `ProxyCommand` or an ssh wrapper started there,
/// which would otherwise outlive it), then reaps it: its exit status. The group is killed
/// while the child is not reaped yet (it waits with `WNOWAIT`): until then its pid, which is
/// the group's id, cannot name another process or group. Without that wait (it fails), the
/// child is reaped without the kill.
pub async fn finish_group(child: &mut Child) -> std::io::Result<std::process::ExitStatus> {
    if let Some(pid) = child.id() {
        let exited = tokio::task::spawn_blocking(move || exited_unreaped(pid)).await;
        if matches!(exited, Ok(Ok(()))) {
            // SAFETY: kill(2) with a negative pid signals that process group; no memory is
            // passed. The group's id is the unreaped child's pid.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
    child.wait().await
}

/// Blocks until process `pid` (a child of this process) has exited, without reaping it.
fn exited_unreaped(pid: u32) -> std::io::Result<()> {
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value; waitid(2) writes into it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) with a valid pointer to `info`; WNOWAIT leaves the child
        // waitable (tokio reaps it).
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

impl Drop for Ssh {
    fn drop(&mut self) {
        // Only while the child is not reaped: its pid (its group's id) is still its own.
        if let Some(pid) = self.child.id() {
            // SAFETY: kill(2) with a negative pid signals that process group; no memory is
            // passed.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }
}

/// Shell functions of the scripts that trust or write the builds tree (the probe, the
/// uploads, the prune), which call `unsafe PATH WHY` (theirs to define: it returns 1, or
/// exits) for what they refuse:
///
/// - `owned PATH KIND`: PATH, when it exists, is of KIND (`d` a directory, `-` a file), not a
///   symbolic link, this user's, and neither its group nor others may write to it (one that
///   exists but cannot be read is refused too);
/// - `ancestors DIR`: `twr_core::paths::check_ancestors` in shell, the same walk: DIR is
///   resolved as the kernel resolves it, from `/`, one component at a time, each symbolic
///   link met followed where it points (a relative target from the link's directory, an
///   absolute one from `/`; at most 40 links), `..` after it; every directory a component is
///   looked up in must be this user's or root's and not one every user may write to without
///   the sticky bit (`/tmp`; a directory its group may write to passes), and every symbolic
///   link met this user's or root's. So the directories above a link's target are checked as
///   well as the link's own. What does not exist yet ends the walk (the caller makes it, in
///   a directory checked). DIR itself is its caller's to check;
/// - `tree DATA`: `ancestors DATA/twr` (DATA and everything above it, through any link), and
///   `owned` of `DATA/twr`, `DATA/twr/runner` and `DATA/twr/runner/builds`.
///
/// Another user who could write to any of these could put a build of theirs where the hub
/// runs it, or replace an upload after its checks. `ls -ldn` reads owners and modes the same
/// way on Linux and macOS.
const TREE_FNS: &str = r#"_me=$(id -u)
owned() {
  [ -e "$1" ] || [ -L "$1" ] || return 0
  _l=$(LC_ALL=C ls -ldn -- "$1" 2>/dev/null) || { unsafe "$1" 'cannot be read'; return; }
  _o=$1; _k=$2
  set -f; set -- $_l; set +f
  case $1 in l*) unsafe "$_o" 'is a symbolic link'; return ;; "$_k"*) ;; *) unsafe "$_o" 'is not what twr made there'; return ;; esac
  [ "$3" = "$_me" ] || { unsafe "$_o" "belongs to uid $3, not $_me"; return; }
  case $1 in ?????w*|????????w*) unsafe "$_o" 'other users can write to it'; return ;; esac
}
ancestors() {
  _d=$1; _at=/; _left=${1#/}; _links=0
  while [ -n "$_left" ]; do
    case $_left in */*) _name=${_left%%/*}; _left=${_left#*/} ;; *) _name=$_left; _left= ;; esac
    case $_name in
      ''|.) continue ;;
      ..) _at=${_at%/*}; [ -n "$_at" ] || _at=/; continue ;;
    esac
    [ -e "$_at" ] || return 0
    _l=$(LC_ALL=C ls -ldn -- "$_at" 2>/dev/null) || { unsafe "$_at" 'cannot be read'; return; }
    set -f; set -- $_l; set +f
    [ "$3" = "$_me" ] || [ "$3" = 0 ] || { unsafe "$_at" "belongs to uid $3"; return; }
    case $1 in ????????w[!tT]*) unsafe "$_at" 'every user can write to it'; return ;; esac
    _next=${_at%/}/$_name
    [ -e "$_next" ] || [ -L "$_next" ] || { _at=$_next; continue; }
    _l=$(LC_ALL=C ls -ldn -- "$_next" 2>/dev/null) || { unsafe "$_next" 'cannot be read'; return; }
    case $_l in
      l*)
        set -f; set -- $_l; set +f
        [ "$3" = "$_me" ] || [ "$3" = 0 ] || { unsafe "$_next" "is a symbolic link of uid $3"; return; }
        _links=$((_links + 1))
        [ "$_links" -le 40 ] || { unsafe "$_d" 'goes through too many symbolic links'; return; }
        _t=$(readlink -- "$_next") || { unsafe "$_next" 'cannot be read'; return; }
        case $_t in /*) _at=/ ;; esac
        _t=${_t#/}
        _left=$_t${_left:+/$_left} ;;
      *) _at=$_next ;;
    esac
  done
}
tree() {
  ancestors "$1/twr" || return
  for _x in "$1/twr" "$1/twr/runner" "$1/twr/runner/builds"; do owned "$_x" d || return; done
}
"#;

/// The probe's script (see the module docs): `twr-probe 1`, then `os=`, `arch=`, `data=` (only
/// when it is an absolute path; one with a control character, which could split its line,
/// is `tree=unsafe` instead), then, when the builds tree is safe to use ([`TREE_FNS`]), one
/// `has=<build id>` per installed build whose directory and executable are this user's alone;
/// each path it does not trust as an `unsafe=<path>: <why>` line, and `tree=unsafe` (and
/// nothing more) when that is the builds tree itself.
pub fn probe_script() -> String {
    format!(
        "echo '{PROBE_MARK}'\n\
         echo \"os=$(uname -s 2>/dev/null)\"\n\
         echo \"arch=$(uname -m 2>/dev/null)\"\n\
         case ${{XDG_DATA_HOME:-}} in /*) data=$XDG_DATA_HOME ;; *) case ${{HOME:-}} in /*) data=$HOME/.local/share ;; *) exit 0 ;; esac ;; esac\n\
         case $data in *[[:cntrl:]]*) echo 'unsafe=the data directory: its path holds a control character (a newline, say)'; echo 'tree=unsafe'; exit 0 ;; esac\n\
         echo \"data=$data\"\n\
         unsafe() {{ echo \"unsafe=$1: $2\"; return 1; }}\n\
         {TREE_FNS}\
         tree \"$data\" || {{ echo 'tree=unsafe'; exit 0; }}\n\
         for f in \"$data\"/twr/runner/builds/*/twr; do\n\
         \x20 [ -f \"$f\" ] && [ -x \"$f\" ] || continue\n\
         \x20 b=${{f%/twr}}\n\
         \x20 owned \"$b\" d && owned \"$f\" - && echo \"has=${{b##*/}}\"\n\
         done\n\
         exit 0\n"
    )
}

/// What the probe found on a box.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Probe {
    /// `uname -s`.
    pub os: String,
    /// `uname -m`.
    pub arch: String,
    /// The box's data directory (absolute), `None` when neither `XDG_DATA_HOME` nor `HOME` is
    /// an absolute path there.
    pub data: Option<String>,
    /// The builds installed there that twr trusts (their directory and executable the user's
    /// alone).
    pub has: BTreeSet<String>,
    /// The builds tree (`<data>/twr/runner/builds`, or a directory above it) is not safe to
    /// use: another user could write to it ([`TREE_FNS`]). Nothing there is run, and nothing
    /// is uploaded.
    pub tree_unsafe: bool,
    /// What the probe did not trust, and why (`<path>: <why>`): the builds tree (when
    /// `tree_unsafe`), or builds left out of `has`. At most [`PROBE_UNSAFE_KEPT`].
    pub unsafe_paths: Vec<String>,
}

/// The probe's `unsafe=` lines kept ([`Probe::unsafe_paths`]).
const PROBE_UNSAFE_KEPT: usize = 10;

impl Probe {
    /// Why the builds tree is refused, when it is ([`Probe::tree_unsafe`]): for box `name`,
    /// with what to do.
    pub fn tree_refusal(&self, name: &str) -> Option<String> {
        self.tree_unsafe.then(|| {
            format!(
                "twr does not use box {name}'s builds directory: {}. Another user could put \
                 a program there that the hub would run. Make each of those directories yours, \
                 and writable by you alone (`chmod go-w`)",
                if self.unsafe_paths.is_empty() {
                    "it is not safe".to_string()
                } else {
                    self.unsafe_paths.join("; ")
                }
            )
        })
    }
}

/// Parses the probe's output: the lines after its first `twr-probe 1` line; unknown lines,
/// a `data` that is not absolute and a `has` that is not a build id are ignored (a login
/// script may print anything). An error without the mark or an `os`.
pub fn parse_probe(out: &str) -> anyhow::Result<Probe> {
    let mut lines = out.lines().map(|l| l.trim_end_matches('\r'));
    if !lines.by_ref().any(|l| l == PROBE_MARK) {
        anyhow::bail!("the probe printed nothing twr understands (is the login shell POSIX?)");
    }
    let mut p = Probe::default();
    for line in lines {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k {
            "os" if p.os.is_empty() => p.os = v.trim().to_string(),
            "arch" if p.arch.is_empty() => p.arch = v.trim().to_string(),
            "data" if p.data.is_none() && v.starts_with('/') => p.data = Some(v.to_string()),
            "has" if twr_core::valid_build_id(v) => {
                p.has.insert(v.to_string());
            }
            "unsafe" if p.unsafe_paths.len() < PROBE_UNSAFE_KEPT => p
                .unsafe_paths
                .push(v.chars().take(NOTE_MAX_BYTES).collect()),
            "tree" if v == "unsafe" => p.tree_unsafe = true,
            _ => {}
        }
    }
    if p.os.is_empty() {
        anyhow::bail!("the probe did not tell the box's OS (`uname -s`)");
    }
    Ok(p)
}

/// The Rust name ([`std::env::consts::OS`]) of an OS as `uname -s` prints it, for the OSes twr
/// builds for.
pub fn os_name(uname_s: &str) -> Option<&'static str> {
    match uname_s {
        "Linux" => Some("linux"),
        "Darwin" => Some("macos"),
        _ => None,
    }
}

/// The Rust name ([`std::env::consts::ARCH`]) of a CPU as `uname -m` prints it, for the CPUs
/// twr builds for.
pub fn arch_name(uname_m: &str) -> Option<&'static str> {
    match uname_m {
        "x86_64" | "amd64" => Some("x86_64"),
        "aarch64" | "arm64" => Some("aarch64"),
        _ => None,
    }
}

/// Which build the hub runs on a box ([`choose_build`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildChoice {
    /// The hub's own (uploaded when missing).
    Own(String),
    /// `[boxes.<name>] build` (installed by `twr box install`; never uploaded by the hub).
    Configured(String),
    /// None: the box needs `twr box install`; why.
    NeedsInstall(String),
}

/// Which build to run on box `name` (see the module docs): `probe` of the box, the hub's own
/// target (`hub_os`, `hub_arch`: Rust's names) and build (`own`), and the box's configured
/// one (`configured`).
pub fn choose_build(
    name: &str,
    probe: &Probe,
    (hub_os, hub_arch): (&str, &str),
    own: Option<&str>,
    configured: Option<&str>,
) -> BuildChoice {
    let os = os_name(&probe.os);
    let arch = arch_name(&probe.arch);
    let same = os == Some(hub_os) && arch == Some(hub_arch);
    match (same, own, configured) {
        (true, Some(own), _) => BuildChoice::Own(own.to_string()),
        (_, _, Some(id)) => BuildChoice::Configured(id.to_string()),
        (true, None, None) => BuildChoice::NeedsInstall(format!(
            "the hub cannot read its own executable to install it on box {name}: run `twr box \
             install {name}`"
        )),
        (false, _, None) => BuildChoice::NeedsInstall(format!(
            "box {name} is {}/{} and the hub {hub_os}/{hub_arch}: run `twr box install {name}`",
            os.unwrap_or(&probe.os),
            arch.unwrap_or(&probe.arch)
        )),
    }
}

/// The builds directory under a box's data directory `data`.
pub fn builds_dir(data: &str) -> String {
    format!("{}/twr/runner/builds", data.trim_end_matches('/'))
}

/// Build `id`'s executable under a box's data directory `data`.
pub fn build_path(data: &str, id: &str) -> String {
    format!("{}/{id}/twr", builds_dir(data))
}

/// The script that receives the hub's executable on its stdin (see the module docs): into
/// `<builds>/<id>/.twr.upload.<tag>` (umask 077), checked against `size` bytes, the first
/// digits of its SHA-256 (`id`, a build id is that) and `twr <version>`, then moved to
/// `<builds>/<id>/twr`. Its last line is `twr-upload: ok`, or `twr-upload: <what differs>`
/// with exit status 3 (the temporary file removed).
pub fn upload_script(data: &str, id: &str, size: u64, version: &str, tag: &str) -> String {
    upload_script_with(data, id, size, Some(version), tag)
}

/// The script `twr box install --binary` sends a build for another OS or CPU with: as
/// [`upload_script`], except that the build may say any version (the hub's protocol decides
/// at the handshake whether it serves): it must run on the box and say `twr <something>`,
/// which a `twr-upload: version <what it says>` line reports before the move.
pub fn install_script(data: &str, id: &str, size: u64, tag: &str) -> String {
    upload_script_with(data, id, size, None, tag)
}

/// The upload scripts' shell function `digest FILE WHAT` (defined after their `bad`): sets
/// `h` to the SHA-256 of FILE, 64 lowercase hex digits, with `sha256sum`, else `shasum -a
/// 256`. When the box has neither it returns 1 with `h` empty (the caller decides whether
/// size and version then suffice); a tool that is there but fails, or prints no digest, is
/// `bad` (the upload is refused and removed), never taken for a missing one.
const DIGEST_FN: &str = "digest() {\n\
    if command -v sha256sum >/dev/null 2>&1; then h=$(sha256sum < \"$1\") || bad \"sha256sum failed on $2\";\n\
    elif command -v shasum >/dev/null 2>&1; then h=$(shasum -a 256 < \"$1\") || bad \"shasum failed on $2\";\n\
    else h=; return 1; fi\n\
    h=${h%% *}\n\
    case $h in *[!0-9a-f]*|'') bad \"the box's SHA-256 tool printed no digest of $2\" ;; esac\n\
    [ ${#h} -eq 64 ] || bad \"the box's SHA-256 tool printed no digest of $2\"\n\
    }\n";

fn upload_script_with(data: &str, id: &str, size: u64, version: Option<&str>, tag: &str) -> String {
    let dir = sh_quote(&format!("{}/{id}", builds_dir(data)));
    let tmp = sh_quote(&format!("{}/{id}/.twr.upload.{tag}", builds_dir(data)));
    let id = sh_quote(id);
    let check_version = match version {
        Some(version) => {
            let want = sh_quote(&format!("twr {version}"));
            format!("[ \"$v\" = {want} ] || bad \"it says $v when run, not \"{want}\n")
        }
        None => format!(
            "case $v in 'twr '?*) ;; *) bad \"it does not run on the box: $v\" ;; esac\n\
             echo \"{UPLOAD_MARK}version $v\"\n"
        ),
    };
    let data = sh_quote(data);
    format!(
        "umask 077\n\
         d={dir}\n\
         t={tmp}\n\
         {TREE_FNS}\
         unsafe() {{ echo \"{UPLOAD_MARK}not safe to install into: $1 $2\"; cat > /dev/null; exit {UPLOAD_MISMATCH}; }}\n\
         tree {data}\n\
         mkdir -p -- \"$d\" || exit 1\n\
         tree {data}\n\
         owned \"$d\" d\n\
         cat > \"$t\" || {{ rm -f -- \"$t\"; exit 1; }}\n\
         bad() {{ rm -f -- \"$t\"; echo \"{UPLOAD_MARK}$1\"; exit {UPLOAD_MISMATCH}; }}\n\
         {DIGEST_FN}\
         size=$(wc -c < \"$t\" | tr -d ' ')\n\
         [ \"$size\" = {size} ] || bad \"it arrived with $size bytes, not {size}\"\n\
         digest \"$t\" it || h=\n\
         case $h in '') ;; {id}*) ;; *) bad \"its SHA-256 is $h, not the hub's\" ;; esac\n\
         chmod 700 \"$t\" || bad 'it cannot be made executable'\n\
         v=$(\"$t\" --version 2>&1 | head -n 1)\n\
         {check_version}\
         mv -f -- \"$t\" \"$d/twr\" || bad 'it cannot be moved into place'\n\
         echo '{UPLOAD_MARK}ok'\n"
    )
}

/// The script `twr box install` sends a release archive for the box's OS and CPU with (the
/// archive's signed checksum verified before): into a temporary directory under the builds
/// directory (umask 077), the archive checked against `size` bytes and its SHA-256 `sha256`
/// (the box must have `sha256sum` or `shasum`), `twr` extracted from it, its build id (the
/// head of its SHA-256) computed there, its `--version` checked to be `twr <version>`, and it
/// is moved to `<builds>/<id>/twr`. Its last line is `twr-upload: ok <id>`, or `twr-upload:
/// <what is wrong>` with exit status 3; the temporary directory is removed either way.
pub fn release_install_script(
    data: &str,
    size: u64,
    sha256: &str,
    version: &str,
    tag: &str,
) -> String {
    let builds = sh_quote(&builds_dir(data));
    let tmp = sh_quote(&format!(".twr.extract.{tag}"));
    let sha256 = sh_quote(sha256);
    let want = sh_quote(&format!("twr {version}"));
    let len = twr_core::BUILD_ID_LEN;
    let data = sh_quote(data);
    format!(
        "umask 077\n\
         b={builds}\n\
         t=\n\
         {TREE_FNS}\
         unsafe() {{ [ -z \"$t\" ] || rm -rf -- \"$t\"; echo \"{UPLOAD_MARK}not safe to install into: $1 $2\"; cat > /dev/null; exit {UPLOAD_MISMATCH}; }}\n\
         tree {data}\n\
         mkdir -p -- \"$b\" || exit 1\n\
         tree {data}\n\
         t=\"$b\"/{tmp}\n\
         mkdir -- \"$t\" || exit 1\n\
         bad() {{ rm -rf -- \"$t\"; echo \"{UPLOAD_MARK}$1\"; exit {UPLOAD_MISMATCH}; }}\n\
         cat > \"$t/twr.tar.gz\" || {{ rm -rf -- \"$t\"; exit 1; }}\n\
         {DIGEST_FN}\
         size=$(wc -c < \"$t/twr.tar.gz\" | tr -d ' ')\n\
         [ \"$size\" = {size} ] || bad \"the archive arrived with $size bytes, not {size}\"\n\
         digest \"$t/twr.tar.gz\" 'the archive' || bad 'the box has neither sha256sum nor shasum to check it with'\n\
         [ \"$h\" = {sha256} ] || bad \"the archive's SHA-256 is $h, not the release's\"\n\
         tar -xzf \"$t/twr.tar.gz\" -C \"$t\" twr || bad 'tar cannot extract twr from the archive'\n\
         [ -f \"$t/twr\" ] || bad 'the archive holds no twr'\n\
         digest \"$t/twr\" twr || bad 'its SHA-256 cannot be computed'\n\
         id=$(printf '%s' \"$h\" | cut -c1-{len})\n\
         chmod 700 \"$t/twr\" || bad 'it cannot be made executable'\n\
         v=$(\"$t/twr\" --version 2>&1 | head -n 1)\n\
         [ \"$v\" = {want} ] || bad \"it says $v when run, not \"{want}\n\
         mkdir -p -- \"$b/$id\" || bad \"$b/$id cannot be made\"\n\
         owned \"$b/$id\" d || bad \"$b/$id is not safe to install into\"\n\
         mv -f -- \"$t/twr\" \"$b/$id/twr\" || bad 'it cannot be moved into place'\n\
         rm -rf -- \"$t\"\n\
         echo \"{UPLOAD_MARK}ok $id\"\n"
    )
}

/// The answer of an upload script ([`upload_script`], [`install_script`],
/// [`release_install_script`]) in its output `out`: the text after the last `twr-upload: `
/// mark, and the version it reported (`twr-upload: version <v>`), if any.
pub fn upload_answer(out: &str) -> (Option<String>, Option<String>) {
    let mut answer = None;
    let mut version = None;
    for l in out.lines() {
        if let Some(a) = l.strip_prefix(UPLOAD_MARK) {
            match a.strip_prefix("version ") {
                Some(v) => version = Some(v.to_string()),
                None => answer = Some(a.to_string()),
            }
        }
    }
    (answer, version)
}

/// The script (for `sh`) that stops the daemon of box `name` for hub key `hub_key` with build
/// `id` (`twr runner --stdio --stop`) under the box's data directory, found as the probe finds
/// it ([`probe_script`]): what `twr box remove` prints for the user to run (one line; each
/// part quoted only when it needs it, so it reads plainly once quoted again). An error for an
/// `id` that is no build id or a `name` that is no box name.
pub fn stop_script(id: &str, name: &str, hub_key: &str) -> Result<String, String> {
    if !twr_core::valid_build_id(id) {
        return Err(format!("{id:?} is not a build id"));
    }
    twr_core::runner::validate_box_name(name).map_err(|e| e.to_string())?;
    Ok(format!(
        "case ${{XDG_DATA_HOME:-}} in /*) d=$XDG_DATA_HOME ;; *) d=$HOME/.local/share ;; esac; \
         exec \"$d\"/twr/runner/builds/{} runner --stdio {} --hub-key {} --stop",
        twr_core::paths::sh_word(&format!("{id}/twr")),
        twr_core::paths::sh_word(&format!("--box={name}")),
        twr_core::paths::sh_word(hub_key)
    ))
}

/// The command that runs the relay of box `name` with build `id` under `data`:
/// `exec '<build>' runner --stdio '--box=<name>' --hub-key '<hex>' '--build=<id>'
/// [--replace]` (the name joined to its flag: never taken for an option; `--build` tells the
/// relay its own build when it cannot read its executable).
pub fn connect_command(data: &str, id: &str, name: &str, hub_key: &str, replace: bool) -> String {
    let mut c = format!(
        "exec {} runner --stdio {} --hub-key {} {}",
        sh_quote(&build_path(data, id)),
        sh_quote(&format!("--box={name}")),
        sh_quote(hub_key),
        sh_quote(&format!("--build={id}")),
    );
    if replace {
        c.push_str(" --replace");
    }
    c
}

// The builds directory's references.
use twr_core::BUILD_REFS_DIR as REFS_DIR;

/// How long a builds reference no hub rewrote stays in force: a hub instance unused for this
/// long no longer keeps its builds (they are removed by the next prune of another one; an
/// SSH box of the hub's own OS and CPU gets its build uploaded again).
pub const REFS_KEPT_DAYS: u32 = 90;

/// A build directory modified within this many minutes is never pruned: an upload or an
/// install may be under way, or just done and not referenced yet.
pub const PRUNE_MIN_AGE_MINUTES: u32 = 30;

/// The name of hub `hub_id`'s builds reference for box `name` ([`REFS_DIR`]).
pub fn ref_name(hub_id: &str, name: &str) -> String {
    format!("{hub_id}-{name}")
}

/// A shell command that adds build `id` (a build id, or a shell word that expands to one,
/// such as `"$id"`) to the builds references under `data` for hub reference `reference`, as
/// its own file: `<reference>.install.<id>` ([`REFS_DIR`]), written to a temporary file and
/// moved into place. `twr box install` runs it once the build is in place, so that no prune
/// removes it before the hub runs it (a switch may wait for the box to be quiet). Only the
/// hub's prune removes that file again, once its own reference names the build
/// ([`prune_script`]): a file per writer, so neither can drop what the other recorded. It
/// prints nothing and never fails the script it is part of. Only for a script that checked
/// the builds tree first ([`TREE_FNS`]).
pub fn install_ref_script(data: &str, reference: &str, id: &str) -> String {
    format!(
        "( umask 077; cd {} || exit 0; mkdir -p {REFS_DIR} || exit 0; \
         r={REFS_DIR}/{}.install.{id}; printf '%s\\n' {id} > \"$r.tmp.$$\" && \
         mv -f \"$r.tmp.$$\" \"$r\" ) >/dev/null 2>&1\n",
        sh_quote(&builds_dir(data)),
        sh_quote(reference),
    )
}

/// Shell functions of [`prune_script`], run in the builds directory, for the writer whose
/// token is `token` (random: [`refs_token`]). `refs_lock` takes [`twr_core::BUILD_REFS_LOCK`]
/// (a directory made with `mkdir`, the token in its `owner` file), waiting at most
/// [`twr_core::BUILD_REFS_LOCK_WAIT_SECS`] (status 1 when it could not). A lock older than
/// [`twr_core::BUILD_REFS_LOCK_STALE_SECS`] was left by a writer that died: it is renamed
/// aside (atomically: of two writers that find it stale, one renames it) and checked again
/// there: still stale, it is removed; fresh (another writer took over in between), it is put
/// back when nothing took its place. `refs_unlock` removes the lock only when its token is
/// this writer's: a writer whose stale lock was taken over never removes the new one.
fn refs_lock_fns(token: &str) -> String {
    let lock = twr_core::BUILD_REFS_LOCK;
    let wait = twr_core::BUILD_REFS_LOCK_WAIT_SECS;
    let stale = twr_core::BUILD_REFS_LOCK_STALE_SECS.div_ceil(60);
    let token = sh_quote(token);
    format!(
        "_tok={token}\n\
         refs_stale() {{ [ -n \"$(find \"$1\" -prune -mmin +{stale} 2>/dev/null)\" ]; }}\n\
         refs_lock() {{\n\
         \x20 _i=0\n\
         \x20 while ! mkdir {lock} 2>/dev/null; do\n\
         \x20   if refs_stale {lock}; then\n\
         \x20     if mv {lock} \"{lock}.$_tok\" 2>/dev/null; then\n\
         \x20       if refs_stale \"{lock}.$_tok\" || [ -e {lock} ]; then rm -rf -- \"{lock}.$_tok\";\n\
         \x20       else mv \"{lock}.$_tok\" {lock} 2>/dev/null || rm -rf -- \"{lock}.$_tok\"; fi\n\
         \x20     fi\n\
         \x20     continue\n\
         \x20   fi\n\
         \x20   _i=$((_i + 1)); [ \"$_i\" -le {wait} ] || return 1\n\
         \x20   sleep 1\n\
         \x20 done\n\
         \x20 printf '%s\\n' \"$_tok\" > {lock}/owner\n\
         }}\n\
         refs_unlock() {{\n\
         \x20 [ \"$(cat {lock}/owner 2>/dev/null)\" = \"$_tok\" ] || return 0\n\
         \x20 rm -f {lock}/owner; rmdir {lock} 2>/dev/null\n\
         }}\n"
    )
}

/// A random token for a writer of the builds references' lock ([`refs_lock_fns`]): 16 hex
/// digits, or the time when the system's random source fails.
fn refs_token() -> String {
    twr_core::runner::new_nonce()
        .map(|n| n[..16].to_string())
        .unwrap_or_else(|_| {
            format!(
                "{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or_default()
            )
        })
}

/// The script that prunes the builds under `data` for hub reference `reference` (nothing at
/// all in a builds tree that is not safe to use, [`TREE_FNS`], or when the references' lock
/// cannot be taken, `refs_lock_fns`). Under that lock: it replaces that reference with `keep`
/// (the builds this hub uses now: launched, running, configured), removes this hub's
/// install files ([`install_ref_script`]) of the builds `keep` names (its reference has them
/// now; an install file of another build stays, whatever `keep` says: `keep` may predate
/// that install), drops references no hub rewrote for [`REFS_KEPT_DAYS`], then removes every
/// build directory but those any reference names, those in `keep`, the newest other one (by
/// modification time) when `keep` has one only, one holding an upload's temporary file, and
/// one modified in the last [`PRUNE_MIN_AGE_MINUTES`]. Only entries named like a build id are
/// touched.
pub fn prune_script(data: &str, reference: &str, keep: &[&str]) -> String {
    let keep: Vec<&str> = keep
        .iter()
        .copied()
        .filter(|k| twr_core::valid_build_id(k))
        .collect();
    let pattern = "[0-9a-f]".repeat(twr_core::BUILD_ID_LEN);
    let kept: Vec<String> = keep.iter().map(|k| sh_quote(k)).collect();
    let newest = if keep.len() < 2 {
        let skip: String = kept.iter().map(|k| format!(" -e {k}")).collect();
        format!(
            "n=$(ls -1t | grep -x '[0-9a-f]\\{{{}\\}}' | grep -v -x -F{skip} | head -n 1)\n",
            twr_core::BUILD_ID_LEN
        )
    } else {
        "n=\n".to_string()
    };
    let reference = sh_quote(reference);
    let installed: String = keep
        .iter()
        .map(|k| format!(" {REFS_DIR}/{reference}.install.{k}"))
        .collect();
    format!(
        "{TREE_FNS}\
         unsafe() {{ return 1; }}\n\
         tree {data} || exit 0\n\
         cd {builds} 2>/dev/null || exit 0\n\
         {lock_fns}\
         umask 077\n\
         mkdir -p {REFS_DIR} || exit 0\n\
         refs_lock || exit 0\n\
         r={REFS_DIR}/{reference}\n\
         printf '%s\\n' {own} | sort -u > \"$r.tmp.$$\" && mv -f \"$r.tmp.$$\" \"$r\"\n\
         rm -f --{installed}\n\
         find {REFS_DIR} -type f -mtime +{REFS_KEPT_DAYS} -exec rm -f {{}} \\; 2>/dev/null\n\
         refs=\" $(cat {REFS_DIR}/* 2>/dev/null | tr '\\n' ' ') {own} \"\n\
         {newest}\
         for b in {pattern}; do\n\
         \x20 [ -d \"$b\" ] || continue\n\
         \x20 case $refs in *\" $b \"*) continue ;; esac\n\
         \x20 [ -n \"$n\" ] && [ \"$b\" = \"$n\" ] && continue\n\
         \x20 for u in \"$b\"/.twr.upload.*; do [ -e \"$u\" ] && continue 2; done\n\
         \x20 [ -n \"$(find \"$b\" -prune -mmin -{PRUNE_MIN_AGE_MINUTES} 2>/dev/null)\" ] && continue\n\
         \x20 rm -rf -- \"$b\"\n\
         done\n\
         refs_unlock\n",
        lock_fns = refs_lock_fns(&refs_token()),
        builds = sh_quote(&builds_dir(data)),
        data = sh_quote(data),
        own = keep.join(" "),
    )
}

/// What ssh's own failure (exit 255) means, from its stderr `lines`, for box `name`: a plain
/// sentence for the failures a user can fix, else `None`.
pub fn ssh_failure(name: &str, lines: &[String]) -> Option<String> {
    lines.iter().rev().find_map(|line| {
        let l = line.trim();
        if l.contains("Permission denied") {
            Some(format!(
                "ssh to box {name} was refused ({l}): the hub runs ssh without prompting \
                 (BatchMode), so load the key into an ssh-agent the hub can reach \
                 (SSH_AUTH_SOCK), or use a key without a passphrase"
            ))
        } else if l.contains("Host key verification failed") {
            Some(format!(
                "ssh could not verify box {name}'s host key ({l}): connect to it once with ssh \
                 yourself and accept its key"
            ))
        } else if l.contains("Could not resolve hostname") {
            Some(format!(
                "ssh could not resolve box {name}'s host name ({l}): check [boxes.{name}] ssh"
            ))
        } else {
            None
        }
    })
}

/// Whether a line the relay printed reads like an error or a warning (it becomes the box's
/// `last_error`).
fn error_like(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    [
        "error",
        "fail",
        "denied",
        "refused",
        "cannot",
        "can't",
        "could not",
        "unable",
        "not found",
        "no such",
        "warning",
        "killed",
    ]
    .iter()
    .any(|w| l.contains(w))
}

/// The wait before attempt `attempt` (0-based) of an outage that has lasted `outage`
/// (see the module docs), with ±20% jitter from `unit` (in `[0, 1)`), never above the cap.
pub fn backoff_delay(attempt: u32, outage: Duration, unit: f64) -> Duration {
    let cap = if outage < SHORT_OUTAGE {
        SHORT_OUTAGE_CAP
    } else {
        LONG_OUTAGE_CAP
    };
    let base = BACKOFF_MIN
        .checked_mul(1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX))
        .unwrap_or(cap)
        .min(cap);
    base.mul_f64(0.8 + 0.4 * unit.clamp(0.0, 1.0)).min(cap)
}

/// The attempts of one box ([`backoff_delay`]).
struct Backoff {
    attempt: u32,
    since: tokio::time::Instant,
    seed: u64,
}

impl Backoff {
    fn new() -> Backoff {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            | 1;
        Backoff {
            attempt: 0,
            since: tokio::time::Instant::now(),
            seed,
        }
    }

    /// A welcomed session just ended: a new outage starts now.
    fn reset(&mut self) {
        self.attempt = 0;
        self.since = tokio::time::Instant::now();
    }

    fn next(&mut self) -> Duration {
        // xorshift64*
        let mut x = self.seed;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.seed = x;
        let unit = (x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 / (1u64 << 53) as f64;
        let d = backoff_delay(self.attempt, self.since.elapsed(), unit);
        self.attempt = self.attempt.saturating_add(1);
        d
    }
}

/// What the supervisor is doing (see the module docs; `online` is the hub's to say).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum State {
    /// Probing, installing or opening the connection.
    #[default]
    Connecting,
    /// A connection is open (`online` once the box is ready).
    Connected,
    /// Waiting for the next attempt.
    Offline,
    /// The box needs `twr box install` (or an upload failed its checks).
    NeedsInstall,
    /// The hub cannot dial any SSH box (its box key).
    Error,
}

impl State {
    /// As `hub.info` says it (`connected` is `connecting` until the box is ready).
    pub fn as_str(self) -> &'static str {
        match self {
            State::Connecting | State::Connected => "connecting",
            State::Offline => "offline",
            State::NeedsInstall => "needs_install",
            State::Error => "error",
        }
    }
}

/// An SSH box's state as `hub.info` shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Status {
    /// What the supervisor is doing.
    pub state: State,
    /// Why the latest attempt failed, or the newest error-like note (one line).
    pub last_error: Option<String>,
    /// The relay's latest stderr lines, oldest first.
    pub notes: VecDeque<String>,
    /// `uname -s` of the box (from the probe).
    pub os: Option<String>,
    /// `uname -m` of the box (from the probe).
    pub arch: Option<String>,
    /// The build the hub launched there.
    pub build: Option<String>,
    /// The build the box's daemon runs, as its inventory said, while a session with it is
    /// welcomed.
    pub running: Option<String>,
    /// The box's runner is another build than the one launched (it was running already, and
    /// speaks the hub's protocol), or than the one `twr box install` recorded since: it is
    /// switched to that one once quiet (see the module docs).
    pub switch_pending: bool,
}

/// An SSH box the hub dials: its settings, its state, and its supervisor.
pub struct SshBox {
    name: String,
    /// `[boxes.<name>] ssh`.
    ssh: String,
    /// `[boxes.<name>] build`, as the files said when the box was added or `box.install` last
    /// read them (a session on the configured build watches it: see the module docs).
    build: watch::Sender<Option<String>>,
    status: Mutex<Status>,
    /// Wakes the supervisor out of its wait (`box.install`).
    wake: Notify,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The build a switch began to switch the box to ([`switch_when_quiet`]), until the next
    /// welcomed session says whether it took.
    switching_to: Mutex<Option<String>>,
    /// A switch that did not take: the build the box still ran after it, and the build it was
    /// to switch to. Not tried again while the box runs that build and the switch would be to
    /// that one (until the hub restarts).
    failed_switch: Mutex<Option<(Option<String>, String)>>,
}

impl std::fmt::Debug for SshBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshBox").field("name", &self.name).finish()
    }
}

impl SshBox {
    /// The state of SSH box `name` configured as `cfg`, before its supervisor starts.
    pub fn new(name: &str, cfg: &BoxConfig) -> SshBox {
        SshBox {
            name: name.to_string(),
            ssh: cfg.ssh.clone().unwrap_or_default(),
            build: watch::Sender::new(cfg.build.clone()),
            status: Mutex::default(),
            wake: Notify::new(),
            task: Mutex::default(),
            switching_to: Mutex::default(),
            failed_switch: Mutex::default(),
        }
    }

    /// The box's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn st(&self) -> MutexGuard<'_, Status> {
        self.status.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The box's state now.
    pub fn status(&self) -> Status {
        self.st().clone()
    }

    /// `[boxes.<name>] build` from now on (`box.install` read the files again: `twr box
    /// install` recorded a build for the box's OS and CPU). A session on the configured build
    /// that sees another one recorded becomes `switch_pending` (see the module docs); the next
    /// attempt launches it.
    pub fn set_build(&self, build: Option<String>) {
        self.build.send_replace(build);
    }

    /// `[boxes.<name>] build`, as last read.
    fn configured_build(&self) -> Option<String> {
        self.build.borrow().clone()
    }

    /// Wakes the supervisor out of its wait at once (a wakeup while it works is kept).
    pub fn wake(&self) {
        self.wake.notify_one();
    }

    fn set_state(&self, state: State) {
        self.st().state = state;
    }

    /// The attempt failed: the box is `state`, and `why`.
    fn failed(&self, state: State, why: String) {
        tracing::warn!(box_name = %self.name, state = state.as_str(), "{why}");
        let mut st = self.st();
        st.state = state;
        st.last_error = Some(why);
    }

    /// A line of the hub's own about the box (a switch begun or over), among its notes.
    pub(crate) fn remark(&self, mut line: String) {
        // (As long as a note at most: it may quote what the box said.)
        if line.len() > NOTE_MAX_BYTES {
            let mut end = NOTE_MAX_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
        }
        tracing::info!(box_name = %self.name, "{line}");
        let mut st = self.st();
        st.notes.push_back(line);
        while st.notes.len() > NOTES_KEPT {
            st.notes.pop_front();
        }
    }

    /// A session was welcomed with the daemon running `running`: when a switch began before
    /// it, whether it took. One that did not (the box still runs another build than the one
    /// it switched to: the relay could not replace its daemon, say) is recorded, noted and
    /// made the box's last error, and not tried again for the same builds
    /// ([`SshBox::switch_failed`]).
    fn check_switch(&self, running: Option<&str>) {
        let to = self
            .switching_to
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        let Some(to) = to else { return };
        if running == Some(to.as_str()) {
            return;
        }
        let why = format!(
            "the box still runs build {} after a switch to build {to}: the hub does not switch \
             it again while that stays so (until it restarts); see the box's daemon.log",
            running.unwrap_or("unknown")
        );
        tracing::warn!(box_name = %self.name, "{why}");
        *self.failed_switch.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((running.map(str::to_string), to));
        self.remark(why.clone());
        self.st().last_error = Some(why);
    }

    /// Whether a switch from build `running` to build `to` did not take before
    /// ([`SshBox::check_switch`]).
    fn switch_failed(&self, running: Option<&str>, to: &str) -> bool {
        self.failed_switch
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|(r, t)| r.as_deref() == running && t == to)
    }

    /// A line the relay printed on stderr.
    fn note(&self, line: &str) {
        let mut line = line.trim_end().to_string();
        if line.is_empty() {
            return;
        }
        if line.len() > NOTE_MAX_BYTES {
            let mut end = NOTE_MAX_BYTES;
            while !line.is_char_boundary(end) {
                end -= 1;
            }
            line.truncate(end);
        }
        tracing::info!(box_name = %self.name, "box: {line}");
        let mut st = self.st();
        if error_like(&line) {
            st.last_error = Some(line.clone());
        }
        st.notes.push_back(line);
        while st.notes.len() > NOTES_KEPT {
            st.notes.pop_front();
        }
    }

    /// Waits for the supervisor to end (bounded by `within`), after the hub stopped; one that
    /// has not by then is aborted and waited for again (as long), so it no longer runs (its
    /// ssh killed with it) once this returns.
    pub(crate) async fn ended(&self, within: Duration) {
        let task = self.task.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(mut t) = task {
            if tokio::time::timeout(within, &mut t).await.is_err() {
                t.abort();
                if tokio::time::timeout(within, &mut t).await.is_err() {
                    tracing::warn!(box_name = %self.name, "the box's supervisor did not end");
                }
            }
        }
    }
}

/// Starts box `rbox`'s supervisor ([`SshBox`] `sbox`) for the hub `hub`.
pub(crate) fn start(hub: WeakHub, rbox: RemoteBox, sbox: Arc<SshBox>) {
    let task = tokio::spawn(supervise(hub, rbox, Arc::clone(&sbox)));
    *sbox.task.lock().unwrap_or_else(|e| e.into_inner()) = Some(task);
}

/// How one attempt ended.
enum Attempt {
    /// A session ran: whether it was welcomed, and whether the next attempt replaces the
    /// box's daemon.
    Session { welcomed: bool, replace: bool },
    /// It failed before a session; the box shows `.0` with `.1`.
    Failed(State, String),
}

/// The supervisor of one box: attempts until the hub stops (see the module docs).
async fn supervise(weak: WeakHub, rbox: RemoteBox, sbox: Arc<SshBox>) {
    let mut backoff = Backoff::new();
    let mut replace = false;
    // Sessions in a row that were to replace the daemon and were not welcomed.
    let mut refused_replaces = 0u32;
    loop {
        let Some(hub) = weak.upgrade() else { return };
        let mut state = hub.state();
        // Nothing new while a shutdown is under way.
        if *state.borrow_and_update() != HubState::Running {
            drop(hub);
            let _ = state.wait_for(|s| *s == HubState::Stopped).await;
            return;
        }
        let outcome = tokio::select! {
            o = attempt(&hub, &rbox, &sbox, replace) => o,
            _ = state.wait_for(|s| *s == HubState::Stopped) => return,
        };
        drop(hub);
        // An attempt that failed before a session keeps `replace` for the next one.
        let wait = match outcome {
            Attempt::Session {
                welcomed,
                replace: r,
            } => {
                if welcomed {
                    backoff.reset();
                }
                sbox.set_state(State::Offline);
                replace = r;
                refused_replaces = next_refused_replaces(refused_replaces, welcomed, r);
                if replace_at_once(refused_replaces, r) {
                    Duration::ZERO
                } else {
                    backoff.next()
                }
            }
            Attempt::Failed(State::NeedsInstall, why) => {
                sbox.failed(State::NeedsInstall, why);
                INSTALL_RETRY
            }
            Attempt::Failed(s, why) => {
                sbox.failed(s, why);
                backoff.next()
            }
        };
        if wait.is_zero() {
            continue;
        }
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = sbox.wake.notified() => {}
            _ = state.wait_for(|s| *s == HubState::Stopped) => return,
        }
    }
}

/// How many sessions in a row were to replace the daemon (`replace`) and were not welcomed,
/// after one that ended so (`welcomed`), `before` such sessions before it.
fn next_refused_replaces(before: u32, welcomed: bool, replace: bool) -> u32 {
    if replace && !welcomed {
        before.saturating_add(1)
    } else {
        0
    }
}

/// Whether the next attempt, which is to replace the daemon (`replace`), goes at once: only
/// the first after a session that asked for it (a switch, a refused protocol); one more that
/// was not welcomed either backs off like any failed attempt (a daemon that keeps being
/// refused never makes the hub dial in a loop).
fn replace_at_once(refused_replaces: u32, replace: bool) -> bool {
    replace && refused_replaces <= 1
}

/// One attempt: probe, choose the build, upload it when it is the hub's and missing, connect
/// and serve the session to its end.
async fn attempt(hub: &Hub, rbox: &RemoteBox, sbox: &SshBox, replace: bool) -> Attempt {
    sbox.set_state(State::Connecting);
    let name = sbox.name();
    let key = match hub.box_key().await {
        Ok(k) => k,
        Err(e) => {
            return Attempt::Failed(
                State::Error,
                format!("the hub's box key cannot be loaded: {e:#}"),
            )
        }
    };
    if let Some(dial) = hub.box_dialer() {
        return dialed(hub, rbox, sbox, &key, dial, replace).await;
    }
    let probe = match probe(sbox).await {
        Ok(p) => p,
        Err(why) => return Attempt::Failed(State::Offline, why),
    };
    {
        let mut st = sbox.st();
        st.os = Some(probe.os.clone());
        st.arch = Some(probe.arch.clone());
    }
    // A builds tree another user could write to: nothing there is run or uploaded.
    if let Some(why) = probe.tree_refusal(name) {
        return Attempt::Failed(State::NeedsInstall, why);
    }
    let Some(data) = probe.data.clone() else {
        return Attempt::Failed(
            State::Offline,
            format!(
                "box {name} has neither XDG_DATA_HOME nor HOME set to an absolute path: twr \
                 keeps its builds and data under it"
            ),
        );
    };
    for u in &probe.unsafe_paths {
        // (Once among the notes: each attempt probes again.)
        let note = format!("not trusted (left out): {u}");
        if !sbox.status().notes.contains(&note) {
            sbox.remark(note);
        }
    }
    let hub_target = (std::env::consts::OS, std::env::consts::ARCH);
    let configured = sbox.configured_build();
    let (id, follows_config) = match choose_build(
        name,
        &probe,
        hub_target,
        twr_core::build_id(),
        configured.as_deref(),
    ) {
        BuildChoice::NeedsInstall(why) => return Attempt::Failed(State::NeedsInstall, why),
        BuildChoice::Configured(id) if !probe.has.contains(&id) => {
            return Attempt::Failed(
                State::NeedsInstall,
                format!(
                    "build {id} ([boxes.{name}] build) is not installed on box {name}: run `twr \
                     box install {name}`"
                ),
            )
        }
        BuildChoice::Configured(id) => (id, true),
        BuildChoice::Own(id) => {
            if !probe.has.contains(&id) {
                if let Err((state, why)) = upload(sbox, &data, &id).await {
                    return Attempt::Failed(state, why);
                }
            }
            (id, false)
        }
    };
    sbox.st().build = Some(id.clone());
    connect(hub, rbox, sbox, &key, &data, &id, follows_config, replace).await
}

/// Runs `remote` on the box with `input` (when given) as its stdin, within `timeout`: its exit
/// status (`None`: killed by a signal), stdout and stderr lines. An error when ssh cannot be
/// started or the time is up.
async fn run_ssh(
    sbox: &SshBox,
    remote: &str,
    input: Option<PathBuf>,
    timeout: Duration,
) -> Result<(Option<i32>, String, Vec<String>), String> {
    let cmd = ssh_command(&sbox.ssh, remote).map_err(|why| refused_words(sbox, &why))?;
    run_ssh_command(cmd, &sbox.name, input, timeout).await
}

/// [`run_ssh`] of ssh command `cmd` ([`ssh_command`]) to box `name`.
async fn run_ssh_command(
    mut cmd: Command,
    name: &str,
    input: Option<PathBuf>,
    timeout: Duration,
) -> Result<(Option<i32>, String, Vec<String>), String> {
    if input.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    let mut ssh = Ssh::spawn(cmd).map_err(|e| {
        format!(
            "cannot run {}: {e}",
            std::path::Path::new(&ssh_program()).display()
        )
    })?;
    let stdin = ssh.child.stdin.take();
    let mut stdout = ssh.child.stdout.take().expect("piped");
    let mut stderr = ssh.child.stderr.take().expect("piped");
    let run = async {
        let feed = async {
            if let (Some(mut stdin), Some(path)) = (stdin, input) {
                let mut f = tokio::fs::File::open(&path)
                    .await
                    .map_err(|e| format!("reading {}: {e}", path.display()))?;
                // A box that stops reading shows in its answer: not an error here.
                let _ = tokio::io::copy(&mut f, &mut stdin).await;
                let _ = stdin.shutdown().await;
            }
            Ok::<(), String>(())
        };
        // The first to fail (more output than allowed) ends the others; `ssh` is killed then.
        let (out_what, err_what) = (
            format!("ssh to box {name} (stdout)"),
            format!("ssh to box {name} (stderr)"),
        );
        let (_, out, err) = tokio::try_join!(
            feed,
            read_capped(&mut stdout, SSH_OUTPUT_MAX, &out_what),
            read_capped(&mut stderr, SSH_OUTPUT_MAX, &err_what),
        )?;
        let status = ssh.finish().await.map_err(|e| e.to_string())?;
        Ok::<_, String>((status.code(), out, err))
    };
    let (code, out, err) = tokio::time::timeout(timeout, run)
        .await
        .map_err(|_| format!("ssh to box {name} took over {} s", timeout.as_secs()))??;
    let err = String::from_utf8_lossy(&err)
        .lines()
        .map(str::to_string)
        .collect();
    Ok((code, String::from_utf8_lossy(&out).into_owned(), err))
}

/// Reads `r` to its end, at most `max` bytes: more is an error naming `what` (the caller
/// kills what writes it).
pub async fn read_capped<R: AsyncRead + Unpin>(
    r: R,
    max: usize,
    what: &str,
) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    r.take(max as u64 + 1)
        .read_to_end(&mut buf)
        .await
        .map_err(|e| format!("reading {what}: {e}"))?;
    if buf.len() > max {
        return Err(format!(
            "{what} printed over {} KiB, more than twr reads from it (a login script that \
             prints without end?)",
            max / 1024
        ));
    }
    Ok(buf)
}

/// The next line of `r` without its line end (`\n`, and a `\r` before it), cut to at most
/// `max` bytes (at a character boundary) while it is read: the rest of a longer line is
/// skipped, never kept. `None` at the end of `r` (a last line without a line end is a line).
pub async fn read_line_capped<R: AsyncBufRead + Unpin>(
    r: &mut R,
    max: usize,
) -> std::io::Result<Option<String>> {
    let mut line: Vec<u8> = Vec::new();
    let mut any = false;
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            if !any {
                return Ok(None);
            }
            break;
        }
        any = true;
        let (chunk, used, end) = match buf.iter().position(|b| *b == b'\n') {
            Some(i) => (&buf[..i], i + 1, true),
            None => (buf, buf.len(), false),
        };
        let room = max.saturating_sub(line.len());
        line.extend_from_slice(&chunk[..chunk.len().min(room)]);
        r.consume(used);
        if end {
            break;
        }
    }
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    let mut line = String::from_utf8_lossy(&line).into_owned();
    // (A cut through a character leaves a replacement character, which may be longer.)
    if line.len() > max {
        let mut end = max;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    Ok(Some(line))
}

/// Why the hub does not run ssh with box `sbox`'s `ssh` words (`why`, from [`ssh_words`]):
/// they were admitted, so this is a defence only.
fn refused_words(sbox: &SshBox, why: &str) -> String {
    format!("[boxes.{}] ssh = {:?}: {why}", sbox.name, sbox.ssh)
}

/// Why an ssh that exited `code` with stderr `err` failed, in one line.
fn failure(sbox: &SshBox, what: &str, code: Option<i32>, err: &[String]) -> String {
    if code == Some(255) {
        if let Some(why) = ssh_failure(&sbox.name, err) {
            return why;
        }
    }
    let last = err
        .iter()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map_or_else(String::new, |l| format!(": {}", l.trim()));
    let code = code.map_or_else(|| "a signal".to_string(), |c| format!("exit {c}"));
    format!("{what} on box {} failed ({code}){last}", sbox.name)
}

/// Probes the box (see the module docs).
async fn probe(sbox: &SshBox) -> Result<Probe, String> {
    let (code, out, err) = run_ssh(sbox, &sh_c(&probe_script()), None, PROBE_TIMEOUT).await?;
    if code != Some(0) {
        return Err(failure(sbox, "probing", code, &err));
    }
    parse_probe(&out).map_err(|e| format!("probing box {}: {e:#}", sbox.name))
}

/// Uploads the hub's own executable as build `id` under `data` (see the module docs). On
/// failure the state to show and why: `needs_install` when what arrived is not the hub's
/// build (or the hub cannot read its executable), `offline` when ssh failed.
async fn upload(sbox: &SshBox, data: &str, id: &str) -> Result<(), (State, String)> {
    let exe = if cfg!(target_os = "linux") {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().map_err(|e| {
            (
                State::NeedsInstall,
                format!("the hub cannot find its own executable to install it: {e}"),
            )
        })?
    };
    let size = std::fs::metadata(&exe)
        .map_err(|e| {
            (
                State::NeedsInstall,
                format!("the hub cannot read its own executable to install it: {e}"),
            )
        })?
        .len();
    let tag = twr_core::runner::new_nonce()
        .map(|n| n[..16].to_string())
        .map_err(|e| (State::Offline, format!("{e:#}")))?;
    tracing::info!(box_name = %sbox.name, build = id, size, "installing the hub's build on the box");
    let script = upload_script(data, id, size, twr_core::version(), &tag);
    let (code, out, err) = run_ssh(sbox, &sh_c(&script), Some(exe), UPLOAD_TIMEOUT)
        .await
        .map_err(|e| (State::Offline, e))?;
    let answer = out
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(UPLOAD_MARK))
        .map(str::to_string);
    match (code, answer.as_deref()) {
        (Some(0), Some("ok")) => {
            tracing::info!(box_name = %sbox.name, build = id, "installed the hub's build on the box");
            Ok(())
        }
        (Some(UPLOAD_MISMATCH), Some(why)) => Err((
            State::NeedsInstall,
            format!(
                "the twr the hub uploaded to box {} was refused: {why}; run `twr box install {}`",
                sbox.name, sbox.name
            ),
        )),
        _ => Err((State::Offline, failure(sbox, "installing twr", code, &err))),
    }
}

/// Connects to the box with build `id` under `data` (the configured one: `follows_config`)
/// and serves the session (see the module docs).
#[allow(clippy::too_many_arguments)]
async fn connect(
    hub: &Hub,
    rbox: &RemoteBox,
    sbox: &SshBox,
    key: &HubKey,
    data: &str,
    id: &str,
    follows_config: bool,
    replace: bool,
) -> Attempt {
    let name = rbox.name();
    // (The relay adds its build to this hub's builds reference as it starts, and this hub's
    // prune after the welcome sets the whole reference: [`prune_script`].)
    let remote = connect_command(data, id, name, &key.public_hex(), replace);
    let mut cmd = match ssh_command(&sbox.ssh, &remote) {
        Ok(c) => c,
        Err(why) => return Attempt::Failed(State::Offline, refused_words(sbox, &why)),
    };
    cmd.stdin(std::process::Stdio::piped());
    let mut ssh = match Ssh::spawn(cmd) {
        Ok(s) => s,
        Err(e) => {
            return Attempt::Failed(
                State::Offline,
                format!(
                    "cannot run {}: {e}",
                    std::path::Path::new(&ssh_program()).display()
                ),
            )
        }
    };
    let stdin = ssh.child.stdin.take().expect("piped");
    let stdout = ssh.child.stdout.take().expect("piped");
    let stderr = ssh.child.stderr.take().expect("piped");
    // The relay's lines, as they come: the box's notes, and this connection's (for ssh's own
    // failures).
    let lines: Mutex<VecDeque<String>> = Mutex::default();
    let reader = read_notes(sbox, stderr, &lines);
    let session = session(
        hub,
        rbox,
        sbox,
        key,
        Some(data),
        id,
        follows_config,
        tokio::io::join(stdout, stdin),
    );
    tokio::pin!(session, reader);
    let mut read_all = false;
    let result = tokio::select! {
        r = &mut session => r,
        // ssh closed its stderr (it is ending): the session sees the end of its stdout too.
        () = &mut reader => {
            read_all = true;
            session.await
        }
    };
    // ssh's exit when it has exited (what is left of its group killed before it is reaped),
    // and what it said last; else it is killed with its group (dropped unreaped).
    let code = match tokio::time::timeout(Duration::from_millis(500), ssh.finish()).await {
        Ok(Ok(s)) => s.code(),
        _ => None,
    };
    drop(ssh);
    if !read_all {
        let _ = tokio::time::timeout(Duration::from_millis(500), &mut reader).await;
    }
    let lines: Vec<String> = lines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .cloned()
        .collect();
    match result {
        Ok((end, hello, switched)) => {
            if code == Some(255) {
                if let Some(why) = ssh_failure(name, &lines) {
                    sbox.st().last_error = Some(why);
                }
            }
            session_ended(name, id, end, &hello, switched)
        }
        Err(why) => {
            let why = match code {
                Some(255) => ssh_failure(name, &lines).unwrap_or(why),
                _ => match lines.iter().rev().find(|l| error_like(l)) {
                    Some(l) => format!("{why} ({})", l.trim()),
                    None => why,
                },
            };
            Attempt::Failed(State::Offline, why)
        }
    }
}

/// Reads the relay's stderr `stderr` to its end: each line becomes a note of `sbox`, and the
/// latest `2 * NOTES_KEPT` are kept in `lines` too (for ssh's own failures). Bounded, as a
/// relay that prints without end must not grow the hub or its log: each line is cut while it
/// is read ([`NOTE_MAX_BYTES`]), and at most [`SESSION_NOTES_MAX`] are taken at all (the rest
/// are read and dropped, with one note saying so).
async fn read_notes<R: AsyncRead + Unpin>(
    sbox: &SshBox,
    stderr: R,
    lines: &Mutex<VecDeque<String>>,
) {
    let mut r = tokio::io::BufReader::new(stderr);
    let mut taken = 0;
    while let Ok(Some(line)) = read_line_capped(&mut r, NOTE_MAX_BYTES).await {
        taken += 1;
        if taken > SESSION_NOTES_MAX {
            if taken == SESSION_NOTES_MAX + 1 {
                sbox.remark(format!(
                    "the box printed over {SESSION_NOTES_MAX} lines on this connection: the \
                     rest is dropped"
                ));
            }
            continue;
        }
        sbox.note(&line);
        let mut l = lines.lock().unwrap_or_else(|e| e.into_inner());
        if l.len() >= 2 * NOTES_KEPT {
            l.pop_front();
        }
        l.push_back(line);
    }
}

/// An attempt through the test dialer ([`crate::HubOptions::box_dialer`]) in place of the
/// probe, the upload and ssh: the dialer's stream is served as ssh's would be, its build taken
/// for the one launched.
async fn dialed(
    hub: &Hub,
    rbox: &RemoteBox,
    sbox: &SshBox,
    key: &HubKey,
    dial: BoxDialer,
    replace: bool,
) -> Attempt {
    let name = rbox.name();
    let (io, id) = match dial(name.to_string(), replace).await {
        Ok(d) => d,
        Err(e) => return Attempt::Failed(State::Offline, format!("dialing box {name}: {e}")),
    };
    sbox.st().build = Some(id.clone());
    // (The dialer's build stands for the configured one when the box has that one.)
    let follows_config = sbox.configured_build().as_deref() == Some(id.as_str());
    match session(hub, rbox, sbox, key, None, &id, follows_config, io).await {
        Ok((end, hello, switched)) => session_ended(name, &id, end, &hello, switched),
        Err(why) => Attempt::Failed(State::Offline, why),
    }
}

/// How a session with box `name`'s daemon (launched: build `id`) that ended as `end` went for
/// the next attempt: it replaces the daemon after a switch began (`switched`), and when the
/// daemon spoke another protocol and is not the launched build (see the module docs).
fn session_ended(
    name: &str,
    id: &str,
    end: SealedEnd,
    hello: &DaemonHello,
    switched: bool,
) -> Attempt {
    let SealedEnd {
        welcomed,
        protocol_refused,
        ..
    } = end;
    if let Some(p) = protocol_refused {
        tracing::warn!(box_name = %name, protocol = p, daemon = ?hello.build, launched = id, "the box's daemon speaks another runner protocol: replacing it");
    }
    let replace = switched || (protocol_refused.is_some() && hello.build.as_deref() != Some(id));
    Attempt::Session { welcomed, replace }
}

/// One session over `io` (ssh's stdout and stdin, or a test dialer's stream) with the daemon
/// of box `rbox`, launched as build `id` (the box's configured build: `follows_config`): the
/// sealed channel, the WebSocket inside it and the runner connection, served to its end. Once
/// it is welcomed: `switch_pending`, the removal of old builds under `data` (when known), and
/// the switch when it is pending, or, following the configured build, once another one is
/// recorded ([`SshBox::set_build`]; see the module docs). How it ended, the daemon's hello,
/// and whether a switch began.
#[allow(clippy::too_many_arguments)]
async fn session<IO>(
    hub: &Hub,
    rbox: &RemoteBox,
    sbox: &SshBox,
    key: &HubKey,
    data: Option<&str>,
    id: &str,
    follows_config: bool,
    io: IO,
) -> Result<(SealedEnd, DaemonHello, bool), String>
where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let name = rbox.name();
    let opened =
        tokio::time::timeout(OPEN_TIMEOUT, twr_core::sealed::hub_side(io, key, name)).await;
    let (sealed, hello) = match opened {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => return Err(format!("box {name}: {e:#}")),
        Err(_) => {
            return Err(format!(
                "box {name} did not open the channel within {} s",
                OPEN_TIMEOUT.as_secs()
            ))
        }
    };
    let ws = tokio::time::timeout(
        WS_TIMEOUT,
        tokio_tungstenite::accept_async_with_config(
            sealed,
            Some(crate::listener::pre_auth_ws_config()),
        ),
    )
    .await;
    let ws = match ws {
        Ok(Ok(ws)) => ws,
        Ok(Err(e)) => return Err(format!("box {name}: the WebSocket handshake failed: {e}")),
        Err(_) => return Err(format!("box {name}: the WebSocket handshake timed out")),
    };
    sbox.set_state(State::Connected);
    let (welcomed_tx, welcomed_rx) = oneshot::channel();
    let served = hub.serve_sealed_session(ws, name, welcomed_tx);
    let switched = AtomicBool::new(false);
    // Ends only once a switch asked the session to end; else the session's end ends it.
    let after_welcome = async {
        let Ok(build) = welcomed_rx.await else {
            return std::future::pending().await;
        };
        // (Subscribed before it is read: a `box.install` from now on is seen.)
        let mut configured = sbox.build.subscribe();
        let pending = build.as_deref() != Some(id);
        {
            let mut st = sbox.st();
            st.last_error = None;
            st.switch_pending = pending;
            st.running = build.clone();
        }
        if pending {
            tracing::info!(box_name = %name, launched = id, running = ?build, "the box runs another build than the hub launched (switch pending)");
        }
        sbox.check_switch(build.as_deref());
        if let Some(data) = data {
            prune(sbox, data, &ref_name(&key.id(), name), id, build.as_deref()).await;
        }
        // (A switch that did not take is not tried again for the same builds.)
        let worth = |to: &str| !sbox.switch_failed(build.as_deref(), to);
        let to = if pending && worth(id) {
            id.to_string()
        } else if follows_config {
            // Until `twr box install` records another build for the box.
            loop {
                let want = configured.borrow_and_update().clone();
                if let Some(want) =
                    want.filter(|w| Some(w.as_str()) != build.as_deref() && worth(w))
                {
                    break want;
                }
                if configured.changed().await.is_err() {
                    return std::future::pending().await;
                }
            }
        } else {
            return std::future::pending().await;
        };
        if !pending {
            sbox.st().switch_pending = true;
            tracing::info!(box_name = %name, running = ?build, installed = %to, "another build was installed for the box (switch pending)");
        }
        switch_when_quiet(hub, sbox, name, &to, &switched).await;
    };
    tokio::pin!(served);
    let end = tokio::select! {
        end = &mut served => end,
        () = after_welcome => served.await,
    };
    sbox.st().running = None;
    Ok((end, hello, switched.load(Ordering::Relaxed)))
}

/// Switches box `name`, whose daemon runs another build, to build `to` (launched, or the
/// configured one installed since) once it is quiet (see the module docs): checks it every
/// [`SwitchTiming::check`], and once it has been quiet for [`SwitchTiming::quiet`] with
/// nothing recorded on its threads meanwhile, has the hub set its gate and stop its runtimes
/// (`switched` is set then), waits for their exits (at most [`SwitchTiming::drain`]) and ends
/// the session. Returns once it asked for that end.
async fn switch_when_quiet(hub: &Hub, sbox: &SshBox, name: &str, to: &str, switched: &AtomicBool) {
    let timing = hub.box_switch_timing();
    // Since when the box has been quiet, and the mark of its threads' log then.
    let mut quiet: Option<(tokio::time::Instant, u64)> = None;
    loop {
        tokio::time::sleep(timing.check).await;
        let mark = match hub.box_switch_quiet(name) {
            Ok(mark) => mark,
            Err(why) => {
                tracing::debug!(box_name = %name, %why, "the box is not quiet");
                quiet = None;
                continue;
            }
        };
        match quiet {
            Some((since, m)) if m == mark => {
                if since.elapsed() < timing.quiet {
                    continue;
                }
            }
            _ => {
                quiet = Some((tokio::time::Instant::now(), mark));
                continue;
            }
        }
        if hub.begin_box_switch(name, mark) {
            break;
        }
        quiet = None;
    }
    switched.store(true, Ordering::Relaxed);
    *sbox.switching_to.lock().unwrap_or_else(|e| e.into_inner()) = Some(to.to_string());
    sbox.remark(format!("switching to build {to}"));
    let deadline = tokio::time::Instant::now() + timing.drain;
    while !hub.box_switch_drained(name) {
        if tokio::time::Instant::now() >= deadline {
            tracing::warn!(box_name = %name, "the box's runtimes did not all exit within {} s; replacing its daemon all the same", timing.drain.as_secs());
            break;
        }
        tokio::time::sleep(DRAIN_POLL).await;
    }
    hub.end_box_session(name, "switching the box to the build the hub launched");
}

/// Records the builds this hub uses on the box (`id`, launched; the one the daemon runs,
/// `running`; the configured one) as its reference `reference`, and removes the builds no
/// hub references, but the newest other one and those an upload may be at
/// ([`prune_script`]; see the module docs). Best effort.
async fn prune(sbox: &SshBox, data: &str, reference: &str, id: &str, running: Option<&str>) {
    let configured = sbox.configured_build();
    let mut keep = vec![id];
    for b in [running, configured.as_deref()].into_iter().flatten() {
        if twr_core::valid_build_id(b) && !keep.contains(&b) {
            keep.push(b);
        }
    }
    let script = prune_script(data, reference, &keep);
    match run_ssh(sbox, &sh_c(&script), None, PROBE_TIMEOUT).await {
        Ok((Some(0), _, _)) => {}
        Ok((code, _, err)) => {
            tracing::debug!(box_name = %sbox.name, ?code, ?err, "removing old builds failed")
        }
        Err(e) => tracing::debug!(box_name = %sbox.name, error = %e, "removing old builds failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes group and other write off `root` and everything under it (not following links):
    /// what a test made under a umask that leaves group write on, made as twr makes it.
    fn go_w_off(root: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt;
        let meta = std::fs::symlink_metadata(root).unwrap();
        if meta.file_type().is_symlink() {
            return;
        }
        let m = meta.permissions().mode() & 0o7777 & !0o022;
        std::fs::set_permissions(root, std::fs::Permissions::from_mode(m)).unwrap();
        if meta.is_dir() {
            for e in std::fs::read_dir(root).unwrap() {
                go_w_off(&e.unwrap().path());
            }
        }
    }

    /// Runs `script` with `sh -c` (as the box would), `stdin` its input, in environment `env`
    /// only: its exit status and stdout.
    fn sh(script: &str, stdin: Option<&std::path::Path>, env: &[(&str, &str)]) -> (i32, String) {
        let mut c = std::process::Command::new("sh");
        c.arg("-c")
            .arg(script)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap());
        for (k, v) in env {
            c.env(k, v);
        }
        c.stdin(match stdin {
            Some(p) => std::process::Stdio::from(std::fs::File::open(p).unwrap()),
            None => std::process::Stdio::null(),
        });
        let out = c.output().unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    #[test]
    fn ssh_never_prompts_or_forwards_and_quotes_the_remote_command() {
        let remote = connect_command(
            "/home/me/.local/share",
            "0123456789abcdef",
            "gpu",
            "ab",
            true,
        );
        assert_eq!(
            remote,
            "exec '/home/me/.local/share/twr/runner/builds/0123456789abcdef/twr' runner --stdio \
             '--box=gpu' --hub-key 'ab' '--build=0123456789abcdef' --replace"
        );
        let args = ssh_args("-p 2222  me@gpu", &remote).unwrap();
        assert_eq!(
            args,
            [
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=20",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ForwardX11=no",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "Tunnel=no",
                "-p",
                "2222",
                "me@gpu",
                "-a",
                "-x",
                "-k",
                "--",
                &remote,
            ]
        );
        // The CLI's ssh in the user's terminal: the same, but it may prompt.
        let mut interactive = args.clone();
        interactive.drain(1..3);
        assert_eq!(
            interactive_ssh_args("-p 2222  me@gpu", &remote).unwrap(),
            interactive
        );
        // Words that would forward something or change how ssh runs are refused, by every
        // builder.
        for bad in [
            "-A me@gpu",
            "-o ForwardAgent=yes me@gpu",
            "-t me@gpu",
            "me@gpu cmd",
        ] {
            assert!(ssh_args(bad, &remote).is_err(), "{bad}");
            assert!(interactive_ssh_args(bad, &remote).is_err(), "{bad}");
            assert!(ssh_command(bad, &remote).is_err(), "{bad}");
        }
        // Every variable part is quoted: a data directory with a quote, spaces or `$` stays
        // one word, unexpanded.
        let odd = connect_command("/srv/it's $HOME", "0123456789abcdef", "gpu", "ab", false);
        assert!(!odd.contains("--replace"));
        let (code, out) = sh(
            &format!(
                "set -- {}; printf '%s\\n' \"$@\"",
                odd.trim_start_matches("exec ")
            ),
            None,
            &[],
        );
        assert_eq!(code, 0);
        assert_eq!(
            out.lines().collect::<Vec<_>>(),
            [
                "/srv/it's $HOME/twr/runner/builds/0123456789abcdef/twr",
                "runner",
                "--stdio",
                "--box=gpu",
                "--hub-key",
                "ab",
                "--build=0123456789abcdef"
            ]
        );
        assert_eq!(sh_c("echo 'hi'"), r"sh -c 'echo '\''hi'\'''");
    }

    /// What OpenSSH makes of the hub's and `twr attach`'s ssh (`ssh -G` prints the settings
    /// it would use): no agent, X11 or GSSAPI forwarding, no forwarded port, tunnel or local
    /// command, never a prompt (the hub's), whatever an ssh config file (`-F`, as
    /// `~/.ssh/config` would) asks for. And the forced `-a -x -k` after the words win even
    /// over `-A -X -Y -K` before them (words the builders refuse; passed here directly).
    #[test]
    fn openssh_forwards_nothing_whatever_the_config_says() {
        if std::process::Command::new("ssh")
            .arg("-V")
            .output()
            .is_err()
        {
            eprintln!("no ssh here: skipped");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().join("config");
        std::fs::write(
            &cfg,
            "Host *\n  ForwardAgent yes\n  ForwardX11 yes\n  ForwardX11Trusted yes\n  \
             GSSAPIDelegateCredentials yes\n  LocalForward 8080 localhost:80\n  \
             RemoteForward 9090 localhost:90\n  DynamicForward 1080\n  Tunnel yes\n  \
             PermitLocalCommand yes\n  LocalCommand touch /nonexistent/x\n  BatchMode no\n",
        )
        .unwrap();
        let cfg = cfg.to_str().unwrap();
        let settings = |args: &[String]| -> std::collections::BTreeMap<String, String> {
            let out = std::process::Command::new("ssh")
                .arg("-G")
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "{}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter_map(|l| l.split_once(' '))
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        let words = format!("-F {cfg} -p 2222 me@gpu.invalid");
        let check = |s: &std::collections::BTreeMap<String, String>, hub: bool| {
            for (k, v) in [
                ("forwardagent", "no"),
                ("forwardx11", "no"),
                ("gssapidelegatecredentials", "no"),
                ("hostname", "gpu.invalid"),
                ("port", "2222"),
            ] {
                assert_eq!(s.get(k).map(String::as_str), Some(v), "{k}: {s:?}");
            }
            for (k, v) in [
                ("clearallforwardings", "yes"),
                ("permitlocalcommand", "no"),
                ("tunnel", "false"),
            ] {
                assert_eq!(s.get(k).map(String::as_str), Some(v), "{k}: {s:?}");
            }
            for k in ["localforward", "remoteforward", "dynamicforward"] {
                assert_eq!(s.get(k), None, "{k}: {s:?}");
            }
            assert_eq!(
                s.get("batchmode").map(String::as_str),
                Some(if hub { "yes" } else { "no" })
            );
        };
        let hub = ssh_args(&words, "true").unwrap();
        check(&settings(&hub), true);
        let attach = crate::remote::ssh_command(
            &words,
            &twr_core::runner::AttachInfo {
                program: "claude".into(),
                args: Vec::new(),
                cwd: None,
                env: Vec::new(),
            },
            None,
        )
        .unwrap();
        let attach: Vec<String> = attach
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(attach[0], "-t");
        check(&settings(&attach), false);
        // Refused words, passed directly: the forced flags after them still win.
        let raw: Vec<String> = SSH_OPTIONS
            .iter()
            .map(|s| s.to_string())
            .chain(
                [
                    "-A",
                    "-X",
                    "-Y",
                    "-K",
                    "-F",
                    cfg,
                    "-p",
                    "2222",
                    "me@gpu.invalid",
                    "-A",
                ]
                .map(String::from),
            )
            .chain(SSH_NO_FORWARDING.iter().map(|s| s.to_string()))
            .chain(["--".into(), "true".into()])
            .collect();
        check(&settings(&raw), true);
    }

    #[test]
    fn os_and_cpu_names_map_to_rusts() {
        assert_eq!(os_name("Linux"), Some("linux"));
        assert_eq!(os_name("Darwin"), Some("macos"));
        assert_eq!(os_name("FreeBSD"), None);
        for (m, want) in [
            ("x86_64", Some("x86_64")),
            ("amd64", Some("x86_64")),
            ("aarch64", Some("aarch64")),
            ("arm64", Some("aarch64")),
            ("armv7l", None),
        ] {
            assert_eq!(arch_name(m), want, "{m}");
        }
    }

    #[test]
    fn the_probe_is_parsed_defensively() {
        let out = "Welcome to gpu!\nos=Hacked\ntwr-probe 1\r\nos=Linux\narch=x86_64\nmotd line\n\
                   data=/home/me/.local/share\nhas=0123456789abcdef\nhas=../../etc\n\
                   has=fedcba9876543210\nos=Darwin\n";
        let p = parse_probe(out).unwrap();
        assert_eq!((p.os.as_str(), p.arch.as_str()), ("Linux", "x86_64"));
        assert_eq!(p.data.as_deref(), Some("/home/me/.local/share"));
        assert_eq!(
            p.has.iter().map(String::as_str).collect::<Vec<_>>(),
            ["0123456789abcdef", "fedcba9876543210"]
        );
        // A relative data directory is none; no mark or no OS is an error.
        let p = parse_probe("twr-probe 1\nos=Linux\ndata=rel/share\n").unwrap();
        assert_eq!(p.data, None);
        assert!(parse_probe("os=Linux\n").is_err());
        assert!(parse_probe("twr-probe 1\narch=x86_64\n").is_err());
    }

    /// The probe script, run here: the data directory follows the relay's rule
    /// (`XDG_DATA_HOME` when absolute, else `$HOME/.local/share`), and only executable builds
    /// count.
    #[test]
    fn the_probe_script_finds_the_builds() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let builds = home.join(".local/share/twr/runner/builds");
        for (id, mode) in [("0123456789abcdef", 0o700), ("fedcba9876543210", 0o600)] {
            std::fs::create_dir_all(builds.join(id)).unwrap();
            let f = builds.join(id).join("twr");
            std::fs::write(&f, "").unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(mode)).unwrap();
        }
        go_w_off(&home.join(".local/share/twr"));
        let home_s = home.to_str().unwrap();
        for xdg in [None, Some(""), Some("relative")] {
            let mut env = vec![("HOME", home_s)];
            if let Some(x) = xdg {
                env.push(("XDG_DATA_HOME", x));
            }
            let (code, out) = sh(&probe_script(), None, &env);
            assert_eq!(code, 0, "{out}");
            let p = parse_probe(&out).unwrap();
            assert_eq!(
                p.os,
                std::process::Command::new("uname")
                    .arg("-s")
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap()
            );
            assert_eq!(p.data.as_deref(), Some(&*format!("{home_s}/.local/share")));
            assert_eq!(p.has.iter().collect::<Vec<_>>(), ["0123456789abcdef"]);
        }
        let xdg = dir.path().join("xdg");
        let (_, out) = sh(
            &probe_script(),
            None,
            &[("HOME", home_s), ("XDG_DATA_HOME", xdg.to_str().unwrap())],
        );
        let p = parse_probe(&out).unwrap();
        assert_eq!(p.data.as_deref(), xdg.to_str());
        assert!(p.has.is_empty());
        // No absolute HOME: no data directory.
        let (_, out) = sh(&probe_script(), None, &[("HOME", "rel")]);
        assert_eq!(parse_probe(&out).unwrap().data, None);
        // A data directory whose path holds a newline (it would split its line): refused.
        let odd = dir.path().join("x\ndata=/tmp/evil");
        std::fs::create_dir_all(&odd).unwrap();
        let (code, out) = sh(
            &probe_script(),
            None,
            &[("HOME", home_s), ("XDG_DATA_HOME", odd.to_str().unwrap())],
        );
        assert_eq!(code, 0, "{out}");
        let p = parse_probe(&out).unwrap();
        assert!(p.tree_unsafe, "{out}");
        assert_eq!(p.data, None, "{out}");
        assert!(p.tree_refusal("gpu").unwrap().contains("control character"));
    }

    /// A builds tree another user could write to is neither trusted nor written: the probe
    /// says so (`tree=unsafe`, the path and why) and lists no build; a build directory that is
    /// a symbolic link is left out; an upload, an install and a release install into such a
    /// tree are refused naming the path; a prune touches nothing there. (A directory of
    /// another user cannot be made here without root: the owner check is the same `ls -ldn`
    /// field as the modes these cases exercise.)
    #[test]
    fn a_builds_tree_others_can_write_to_is_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let data = home.join(".local/share");
        let data_s = data.to_str().unwrap();
        let builds = data.join("twr/runner/builds");
        let exe = dir.path().join("exe");
        std::fs::write(&exe, "#!/bin/sh\necho 'twr 9.9.9'\n").unwrap();
        mode(&exe, 0o755);
        let id = twr_core::build_id_of(&exe).unwrap();
        let size = std::fs::metadata(&exe).unwrap().len();
        std::fs::create_dir_all(builds.join(&id)).unwrap();
        std::fs::copy(&exe, builds.join(&id).join("twr")).unwrap();
        go_w_off(&data.join("twr"));
        let env = [("HOME", home.to_str().unwrap())];
        let probe = || {
            let (code, out) = sh(&probe_script(), None, &env);
            assert_eq!(code, 0, "{out}");
            parse_probe(&out).unwrap()
        };
        let p = probe();
        assert!(!p.tree_unsafe && p.unsafe_paths.is_empty(), "{p:?}");
        assert_eq!(p.has.iter().collect::<Vec<_>>(), [&id]);

        // The builds directory writable by its group.
        mode(&builds, 0o770);
        let p = probe();
        assert!(p.tree_unsafe, "{p:?}");
        assert!(p.has.is_empty());
        let why = p.tree_refusal("gpu").unwrap();
        assert!(
            why.contains(&format!(
                "{}: other users can write to it",
                builds.display()
            )),
            "{why}"
        );
        let refused = |script: &str| {
            let (code, out) = sh(script, Some(&exe), &env);
            assert_eq!(code, UPLOAD_MISMATCH, "{out}");
            let (answer, _) = upload_answer(&out);
            let answer = answer.unwrap();
            assert!(answer.starts_with("not safe to install into: "), "{answer}");
            answer
        };
        let other = "0000000000000001";
        let a = refused(&upload_script(data_s, other, size, "9.9.9", "t1"));
        assert!(a.contains(builds.to_str().unwrap()), "{a}");
        refused(&install_script(data_s, other, size, "t2"));
        refused(&release_install_script(
            data_s,
            size,
            &"0".repeat(64),
            "9.9.9",
            "t3",
        ));
        assert!(!builds.join(other).exists());
        // A prune leaves it all.
        std::fs::create_dir_all(builds.join("000000000000000f")).unwrap();
        let (code, _) = sh(&prune_script(data_s, "h-gpu", &[other]), None, &[]);
        assert_eq!(code, 0);
        assert!(builds.join("000000000000000f").exists());
        assert!(!builds.join(REFS_DIR).exists());
        mode(&builds, 0o700);

        // A directory above it every user may write to (no sticky bit).
        mode(&home, 0o777);
        let p = probe();
        assert!(p.tree_unsafe, "{p:?}");
        assert!(
            p.unsafe_paths[0].contains("every user can write to it"),
            "{p:?}"
        );
        mode(&home, 0o755);

        // A build directory that is a symbolic link: left out, and not uploaded into.
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::rename(builds.join(&id), elsewhere.join(&id)).unwrap();
        std::os::unix::fs::symlink(elsewhere.join(&id), builds.join(&id)).unwrap();
        let p = probe();
        assert!(!p.tree_unsafe, "{p:?}");
        assert!(p.has.is_empty(), "{p:?}");
        assert!(
            p.unsafe_paths[0].ends_with(&format!("{id}: is a symbolic link")),
            "{p:?}"
        );
        let a = refused(&upload_script(data_s, &id, size, "9.9.9", "t4"));
        assert!(a.contains("is a symbolic link"), "{a}");
        assert!(std::fs::read_dir(elsewhere.join(&id)).unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".twr")));
        // A group-writable executable is left out too.
        std::fs::remove_file(builds.join(&id)).unwrap();
        std::fs::create_dir_all(builds.join(&id)).unwrap();
        mode(&builds.join(&id), 0o755);
        std::fs::copy(&exe, builds.join(&id).join("twr")).unwrap();
        mode(&builds.join(&id).join("twr"), 0o775);
        let p = probe();
        assert!(p.has.is_empty(), "{p:?}");
        mode(&builds.join(&id).join("twr"), 0o755);
        assert_eq!(probe().has.iter().collect::<Vec<_>>(), [&id]);

        // A data directory reached through a symbolic link of the user's (relative, then
        // absolute) whose target lies under a directory every user may write to: what is above
        // the target is checked too.
        let open = dir.path().join("open");
        let hidden = open.join("data");
        std::fs::create_dir_all(hidden.join("twr/runner/builds").join(&id)).unwrap();
        std::fs::copy(&exe, hidden.join("twr/runner/builds").join(&id).join("twr")).unwrap();
        go_w_off(&hidden.join("twr"));
        let links = dir.path().join("links");
        std::fs::create_dir_all(&links).unwrap();
        mode(&links, 0o755);
        std::os::unix::fs::symlink("../open/data", links.join("rel")).unwrap();
        std::os::unix::fs::symlink(&hidden, links.join("abs")).unwrap();
        for link in ["rel", "abs"] {
            let xdg = links.join(link);
            let env = [
                ("HOME", home.to_str().unwrap()),
                ("XDG_DATA_HOME", xdg.to_str().unwrap()),
            ];
            let probe = || {
                let (code, out) = sh(&probe_script(), None, &env);
                assert_eq!(code, 0, "{out}");
                parse_probe(&out).unwrap()
            };
            mode(&open, 0o755);
            let p = probe();
            assert!(!p.tree_unsafe, "{link}: {p:?}");
            assert_eq!(p.has.iter().collect::<Vec<_>>(), [&id], "{link}");
            mode(&open, 0o777);
            let p = probe();
            assert!(p.tree_unsafe, "{link}: {p:?}");
            assert_eq!(
                p.unsafe_paths,
                [format!("{}: every user can write to it", open.display())],
                "{link}"
            );
            let (code, out) = sh(
                &upload_script(xdg.to_str().unwrap(), other, size, "9.9.9", "t5"),
                Some(&exe),
                &env,
            );
            assert_eq!(code, UPLOAD_MISMATCH, "{link}: {out}");
            assert!(out.contains("every user can write to it"), "{link}: {out}");
        }
        mode(&open, 0o755);

        // A path that exists but whose owner and mode cannot be read (an `ls` that fails):
        // refused, never taken for one that does not exist.
        let failing = dir.path().join("failing-ls");
        std::fs::create_dir_all(&failing).unwrap();
        std::fs::write(failing.join("ls"), "#!/bin/sh\nexit 2\n").unwrap();
        mode(&failing.join("ls"), 0o755);
        let path = format!(
            "{}:{}",
            failing.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let (code, out) = sh(
            &probe_script(),
            None,
            &[("HOME", home.to_str().unwrap()), ("PATH", &path)],
        );
        assert_eq!(code, 0, "{out}");
        let p = parse_probe(&out).unwrap();
        assert!(p.tree_unsafe, "{out}");
        assert!(p.unsafe_paths[0].ends_with(": cannot be read"), "{out}");
    }

    #[test]
    fn the_build_is_the_hubs_on_its_own_target_else_the_installed_one() {
        let probe = |os: &str, arch: &str| Probe {
            os: os.into(),
            arch: arch.into(),
            ..Default::default()
        };
        let hub = ("linux", "x86_64");
        let own = Some("0123456789abcdef");
        let set = Some("fedcba9876543210");
        assert_eq!(
            choose_build("gpu", &probe("Linux", "amd64"), hub, own, set),
            BuildChoice::Own("0123456789abcdef".into())
        );
        assert_eq!(
            choose_build("gpu", &probe("Linux", "aarch64"), hub, own, set),
            BuildChoice::Configured("fedcba9876543210".into())
        );
        assert_eq!(
            choose_build("gpu", &probe("Darwin", "arm64"), hub, own, None),
            BuildChoice::NeedsInstall(
                "box gpu is macos/aarch64 and the hub linux/x86_64: run `twr box install gpu`"
                    .into()
            )
        );
        assert_eq!(
            choose_build("gpu", &probe("FreeBSD", "amd64"), hub, own, None),
            BuildChoice::NeedsInstall(
                "box gpu is FreeBSD/x86_64 and the hub linux/x86_64: run `twr box install gpu`"
                    .into()
            )
        );
        assert!(matches!(
            choose_build("gpu", &probe("Linux", "x86_64"), hub, None, None),
            BuildChoice::NeedsInstall(m) if m.contains("cannot read its own executable")
        ));
    }

    /// The upload script, run here on what it receives: the right file is moved into place;
    /// one of another size, another digest or another version is removed and refused.
    #[test]
    fn uploads_are_checked_before_they_are_used() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let data_s = data.to_str().unwrap();
        let exe = dir.path().join("exe");
        std::fs::write(&exe, "#!/bin/sh\necho 'twr 9.9.9'\n").unwrap();
        let id = twr_core::build_id_of(&exe).unwrap();
        let size = std::fs::metadata(&exe).unwrap().len();
        let builds = data.join("twr/runner/builds").join(&id);
        let run = |size: u64, id: &str, version: &str| {
            sh(
                &upload_script(data_s, id, size, version, "tag1"),
                Some(&exe),
                &[],
            )
        };
        let leftovers = || -> Vec<String> {
            std::fs::read_dir(&builds)
                .map(|d| {
                    d.map(|e| e.unwrap().file_name().into_string().unwrap())
                        .filter(|n| n.starts_with(".twr.upload"))
                        .collect()
                })
                .unwrap_or_default()
        };
        let (code, out) = run(size + 1, &id, "9.9.9");
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(
            out.contains(&format!("arrived with {size} bytes, not {}", size + 1)),
            "{out}"
        );
        assert!(leftovers().is_empty());
        // (A build id is the head of the digest: another one does not match it.)
        let (code, out) = run(size, "0000000000000000", "9.9.9");
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(out.contains("SHA-256"), "{out}");
        let (code, out) = run(size, &id, "1.0.0");
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(
            out.contains("it says twr 9.9.9 when run, not twr 1.0.0"),
            "{out}"
        );
        assert!(leftovers().is_empty());
        assert!(!builds.join("twr").exists());
        let (code, out) = run(size, &id, "9.9.9");
        assert_eq!(
            (code, out.lines().last()),
            (0, Some("twr-upload: ok")),
            "{out}"
        );
        assert_eq!(
            std::fs::read(builds.join("twr")).unwrap(),
            std::fs::read(&exe).unwrap()
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(builds.join("twr"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(leftovers().is_empty());
    }

    /// A SHA-256 tool that is there but fails, or prints no digest, refuses the upload (the
    /// temporary file removed) in every upload script; only a box with neither `sha256sum` nor
    /// `shasum` falls back to size and version (the hub's upload) or refuses (a release).
    #[test]
    fn a_failing_sha256_tool_refuses_the_upload() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let data_s = data.to_str().unwrap();
        let builds = data.join("twr/runner/builds");
        let exe = dir.path().join("exe");
        std::fs::write(&exe, "#!/bin/sh\necho 'twr 9.9.9'\n").unwrap();
        let id = twr_core::build_id_of(&exe).unwrap();
        let size = std::fs::metadata(&exe).unwrap().len();
        let tool = |dir: &std::path::Path, name: &str, body: &str| {
            std::fs::create_dir_all(dir).unwrap();
            let f = dir.join(name);
            std::fs::write(&f, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        // Only what the scripts need, no SHA-256 tool, then fakes of one in front.
        let base = dir.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        for t in [
            "sh", "mkdir", "cat", "rm", "wc", "tr", "chmod", "head", "mv", "cut", "tar", "gzip",
            "ls", "id",
        ] {
            if let Some(p) = std::env::split_paths(&std::env::var_os("PATH").unwrap())
                .map(|d| d.join(t))
                .find(|p| p.is_file())
            {
                std::os::unix::fs::symlink(p, base.join(t)).unwrap();
            }
        }
        let path = |fakes: &std::path::Path| format!("{}:{}", fakes.display(), base.display());
        let leftovers = || -> Vec<String> {
            std::fs::read_dir(builds.join(&id))
                .map(|d| {
                    d.map(|e| e.unwrap().file_name().into_string().unwrap())
                        .collect()
                })
                .unwrap_or_default()
        };
        let cases = [
            ("failing", "sha256sum", "exit 1", "sha256sum failed on it"),
            ("empty", "sha256sum", "exit 0", "printed no digest of it"),
            (
                "junk",
                "sha256sum",
                "echo 'sha256sum: not today'",
                "printed no digest of it",
            ),
            (
                "short",
                "sha256sum",
                "echo 'abc123  -'",
                "printed no digest of it",
            ),
            ("shasum", "shasum", "exit 2", "shasum failed on it"),
        ];
        for (name, tool_name, body, says) in cases {
            let fakes = dir.path().join(name);
            tool(&fakes, tool_name, body);
            let env = [("PATH", path(&fakes))];
            let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let (code, out) = sh(
                &upload_script(data_s, &id, size, "9.9.9", "t1"),
                Some(&exe),
                &env,
            );
            assert_eq!(code, UPLOAD_MISMATCH, "{name}: {out}");
            assert!(out.contains(says), "{name}: {out}");
            assert!(leftovers().is_empty(), "{name}: {:?}", leftovers());
            let (code, out) = sh(&install_script(data_s, &id, size, "t2"), Some(&exe), &env);
            assert_eq!(code, UPLOAD_MISMATCH, "{name}: {out}");
            assert!(out.contains(says), "{name}: {out}");
            assert!(leftovers().is_empty(), "{name}: {:?}", leftovers());
            // The release script hashes the archive first: the same refusal, about it.
            let (code, out) = sh(
                &release_install_script(data_s, size, &"0".repeat(64), "9.9.9", "t3"),
                Some(&exe),
                &env,
            );
            assert_eq!(code, UPLOAD_MISMATCH, "{name}: {out}");
            assert!(
                out.contains(&says.replace(" it", " the archive")),
                "{name}: {out}"
            );
            let extract: Vec<_> = std::fs::read_dir(&builds)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| n.starts_with(".twr"))
                .collect();
            assert!(extract.is_empty(), "{name}: {extract:?}");
        }
        // Neither tool: the hub's upload goes by size and version, a release is refused.
        let none = dir.path().join("none");
        std::fs::create_dir_all(&none).unwrap();
        let env = [("PATH", path(&none))];
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let (code, out) = sh(
            &release_install_script(data_s, size, &"0".repeat(64), "9.9.9", "t4"),
            Some(&exe),
            &env,
        );
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(out.contains("neither sha256sum nor shasum"), "{out}");
        let (code, out) = sh(
            &upload_script(data_s, &id, size, "9.9.9", "t5"),
            Some(&exe),
            &env,
        );
        assert_eq!(
            (code, out.lines().last()),
            (0, Some("twr-upload: ok")),
            "{out}"
        );
        assert!(builds.join(&id).join("twr").is_file());
    }

    /// `twr box install --binary`'s script takes a build of any version that runs, says the
    /// version, and refuses one that does not run or arrived changed.
    #[test]
    fn a_named_binary_is_checked_and_its_version_reported() {
        let build_id_of = |p: &std::path::Path| twr_core::build_id_of(p).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let data_s = data.to_str().unwrap();
        let exe = dir.path().join("exe");
        std::fs::write(&exe, "#!/bin/sh\necho 'twr 0.1.0'\n").unwrap();
        let id = build_id_of(&exe);
        let size = std::fs::metadata(&exe).unwrap().len();
        let (code, out) = sh(&install_script(data_s, &id, size + 1, "t"), Some(&exe), &[]);
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        let (code, out) = sh(&install_script(data_s, &id, size, "t"), Some(&exe), &[]);
        assert_eq!(code, 0, "{out}");
        assert_eq!(
            upload_answer(&out),
            (Some("ok".to_string()), Some("twr 0.1.0".to_string()))
        );
        assert!(data
            .join("twr/runner/builds")
            .join(&id)
            .join("twr")
            .is_file());
        // Not a twr that runs on the box.
        let junk = dir.path().join("junk");
        std::fs::write(&junk, "#!/bin/sh\nexit 1\n").unwrap();
        let junk_id = build_id_of(&junk);
        let size = std::fs::metadata(&junk).unwrap().len();
        let (code, out) = sh(
            &install_script(data_s, &junk_id, size, "t"),
            Some(&junk),
            &[],
        );
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(
            upload_answer(&out)
                .0
                .unwrap()
                .starts_with("it does not run on the box"),
            "{out}"
        );
        assert!(!data
            .join("twr/runner/builds")
            .join(&junk_id)
            .join("twr")
            .exists());
    }

    /// The release archive's script checks the archive's size and SHA-256, unpacks `twr`,
    /// checks its version and puts it under its own build id; a changed archive or another
    /// version is refused and leaves nothing behind.
    #[test]
    fn a_release_archive_is_checked_and_unpacked_on_the_box() {
        use std::os::unix::fs::PermissionsExt;
        let build_id_of = |p: &std::path::Path| twr_core::build_id_of(p).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        let data_s = data.to_str().unwrap();
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let exe = stage.join("twr");
        std::fs::write(&exe, "#!/bin/sh\necho 'twr 9.9.9'\n").unwrap();
        std::fs::write(stage.join("README.md"), "readme\n").unwrap();
        let archive = dir.path().join("twr.tar.gz");
        let (code, _) = sh(
            &format!(
                "tar -C {} -czf {} twr README.md",
                sh_quote(stage.to_str().unwrap()),
                sh_quote(archive.to_str().unwrap())
            ),
            None,
            &[],
        );
        assert_eq!(code, 0);
        let size = std::fs::metadata(&archive).unwrap().len();
        let (code, sha) = sh(
            "if command -v sha256sum >/dev/null; then sha256sum; else shasum -a 256; fi",
            Some(&archive),
            &[],
        );
        assert_eq!(code, 0);
        let sha = sha.split_whitespace().next().unwrap().to_string();
        assert_eq!(sha.len(), 64);
        let builds = data.join("twr/runner/builds");
        let left = || -> Vec<String> {
            std::fs::read_dir(&builds)
                .map(|d| {
                    d.map(|e| e.unwrap().file_name().into_string().unwrap())
                        .filter(|n| n.starts_with(".twr"))
                        .collect()
                })
                .unwrap_or_default()
        };
        let run = |size: u64, sha: &str, version: &str| {
            sh(
                &release_install_script(data_s, size, sha, version, "t1"),
                Some(&archive),
                &[],
            )
        };
        let (code, out) = run(size + 1, &sha, "9.9.9");
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(left().is_empty());
        let (code, out) = run(size, &"0".repeat(64), "9.9.9");
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(out.contains("not the release's"), "{out}");
        let (code, out) = run(size, &sha, "1.0.0");
        assert_eq!(code, UPLOAD_MISMATCH, "{out}");
        assert!(
            out.contains("it says twr 9.9.9 when run, not twr 1.0.0"),
            "{out}"
        );
        assert!(left().is_empty());
        let (code, out) = run(size, &sha, "9.9.9");
        assert_eq!(code, 0, "{out}");
        let id = build_id_of(&exe);
        assert_eq!(upload_answer(&out).0, Some(format!("ok {id}")));
        let installed = builds.join(&id).join("twr");
        assert_eq!(
            std::fs::read(&installed).unwrap(),
            std::fs::read(&exe).unwrap()
        );
        let mode = std::fs::metadata(&installed).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(left().is_empty());
    }

    /// The prune script keeps the launched build and the daemon's (or the newest other one),
    /// and touches nothing not named like a build.
    #[test]
    fn old_builds_are_removed_but_two() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_str().unwrap();
        let builds = dir.path().join("twr/runner/builds");
        let ids = [
            "000000000000000a",
            "000000000000000b",
            "000000000000000c",
            "000000000000000d",
        ];
        let make = || {
            for (i, id) in ids.iter().enumerate() {
                std::fs::create_dir_all(builds.join(id)).unwrap();
                age(&builds.join(id), i as u64);
            }
            std::fs::create_dir_all(builds.join("keep-me")).unwrap();
            go_w_off(&dir.path().join("twr"));
        };
        let left = || {
            let mut v: Vec<String> = std::fs::read_dir(&builds)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| !n.starts_with('.'))
                .collect();
            v.sort();
            v
        };
        let prune = |keep: &[&str]| sh(&prune_script(data, "h1-gpu", keep), None, &[]).0;
        make();
        // Launched `a`: the newest other one (`d`) stays.
        assert_eq!(prune(&["000000000000000a"]), 0);
        assert_eq!(left(), ["000000000000000a", "000000000000000d", "keep-me"]);
        make();
        // Launched `a`, the daemon runs `b`: those two.
        assert_eq!(prune(&["000000000000000a", "000000000000000b"]), 0);
        assert_eq!(left(), ["000000000000000a", "000000000000000b", "keep-me"]);
        // No builds directory: nothing to do.
        assert_eq!(
            sh(
                &prune_script("/nonexistent", "h1-gpu", &["000000000000000a"]),
                None,
                &[]
            )
            .0,
            0
        );
    }

    /// Sets the modification time of `p` to long ago (plus `i` seconds).
    fn age(p: &std::path::Path, i: u64) {
        let t = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000 + i);
        std::fs::File::open(p).unwrap().set_modified(t).unwrap();
    }

    /// Two hub instances (two hub ids) share a box's builds directory and three builds: each
    /// hub's prune records its own builds and keeps the other's (its relay recorded them, or
    /// its own prune), and removes only builds no hub references, never one an upload is at
    /// or one made in the last minutes. A reference no hub rewrote for months no longer counts.
    #[test]
    fn hubs_sharing_a_box_keep_each_others_builds() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_str().unwrap();
        let builds = dir.path().join("twr/runner/builds");
        let refs = builds.join(REFS_DIR);
        let (a, b, c) = ("00000000000000a1", "00000000000000b2", "00000000000000c3");
        let (old, uploading, young) = ("00000000000000d4", "00000000000000e5", "00000000000000f6");
        for (i, id) in [a, b, c, old, uploading].iter().enumerate() {
            std::fs::create_dir_all(builds.join(id)).unwrap();
            if *id == uploading {
                std::fs::write(builds.join(id).join(".twr.upload.t1"), "half").unwrap();
            }
            age(&builds.join(id), i as u64);
        }
        std::fs::create_dir_all(builds.join(young)).unwrap();
        go_w_off(&dir.path().join("twr"));
        let left = || {
            let mut v: Vec<String> = std::fs::read_dir(&builds)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .filter(|n| !n.starts_with('.'))
                .collect();
            v.sort();
            v
        };
        let (h1, h2) = (
            ref_name("1111111111111111", "gpu"),
            ref_name("2222222222222222", "gpu"),
        );
        // Hub 1's relay (or install) recorded `a`; hub 2 runs `b` and its daemon still `c`.
        assert_eq!(sh(&install_ref_script(data, &h1, a), None, &[]).0, 0);
        assert_eq!(sh(&prune_script(data, &h2, &[b, c]), None, &[]).0, 0);
        assert_eq!(left(), [a, b, c, uploading, young]);
        // Hub 1 launched `a` and its daemon runs it (named twice: no newest other one kept):
        // `b` and `c` stay, hub 2's.
        assert_eq!(sh(&prune_script(data, &h1, &[a, a]), None, &[]).0, 0);
        assert_eq!(left(), [a, b, c, uploading, young]);
        let read = |n: &str| std::fs::read_to_string(refs.join(n)).unwrap();
        assert_eq!(read(&h1), format!("{a}\n"));
        assert_eq!(read(&h2), format!("{b}\n{c}\n"));
        // Hub 2 is on `b` alone now (named twice: no newest other one kept): `c` goes.
        assert_eq!(sh(&prune_script(data, &h2, &[b, b]), None, &[]).0, 0);
        assert_eq!(left(), [a, b, uploading, young]);
        // Hub 2 unused for months: its reference no longer counts, and `b` goes too.
        age(&refs.join(&h2), 0);
        assert_eq!(sh(&prune_script(data, &h1, &[a, a]), None, &[]).0, 0);
        assert_eq!(left(), [a, uploading, young]);
        assert!(!refs.join(&h2).exists());
    }

    /// `twr box install` records build `b` while the hub's prune works from a configuration
    /// read before (it still names `a` only): `b` stays referenced (its own install file) and
    /// is kept however old, until the hub runs it, whose prune then takes it into the hub's
    /// own reference and removes the install file.
    #[test]
    fn an_install_survives_a_stale_prune() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_str().unwrap();
        let builds = dir.path().join("twr/runner/builds");
        let refs = builds.join(REFS_DIR);
        let (a, b) = ("00000000000000a1", "00000000000000b2");
        for (i, id) in [a, b].iter().enumerate() {
            std::fs::create_dir_all(builds.join(id)).unwrap();
            age(&builds.join(id), i as u64);
        }
        go_w_off(&dir.path().join("twr"));
        let h = ref_name("1111111111111111", "gpu");
        let install = refs.join(format!("{h}.install.{b}"));
        // The install's record, then the stale prune (`a` named twice: no newest other one).
        assert_eq!(sh(&install_ref_script(data, &h, b), None, &[]).0, 0);
        assert_eq!(sh(&prune_script(data, &h, &[a, a]), None, &[]).0, 0);
        assert!(builds.join(b).exists(), "the installed build was pruned");
        assert_eq!(std::fs::read_to_string(&install).unwrap(), format!("{b}\n"));
        // The hub on `b`: its reference names it, the install file goes.
        assert_eq!(sh(&prune_script(data, &h, &[b, b]), None, &[]).0, 0);
        assert!(!install.exists());
        assert_eq!(
            std::fs::read_to_string(refs.join(&h)).unwrap(),
            format!("{b}\n")
        );
        assert!(builds.join(b).exists() && !builds.join(a).exists());
    }

    /// The references' lock carries its writer's token: a writer lets go only of its own
    /// lock (one whose stale lock another writer took over never removes the new one), and a
    /// stale lock is taken over.
    #[test]
    fn the_references_lock_is_its_writers_own() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join(twr_core::BUILD_REFS_LOCK);
        let run = |token: &str, body: &str| {
            sh(
                &format!(
                    "cd {}\n{}{body}",
                    sh_quote(dir.path().to_str().unwrap()),
                    refs_lock_fns(token)
                ),
                None,
                &[],
            )
            .0
        };
        assert_eq!(run("aaaa", "refs_lock"), 0);
        assert_eq!(
            std::fs::read_to_string(lock.join("owner")).unwrap(),
            "aaaa\n"
        );
        // Another writer's unlock leaves it.
        assert_eq!(run("bbbb", "refs_unlock"), 0);
        assert!(lock.exists());
        // Left long ago: taken over; the old writer's unlock leaves the new lock.
        age(&lock, 0);
        assert_eq!(run("bbbb", "refs_lock"), 0);
        assert_eq!(
            std::fs::read_to_string(lock.join("owner")).unwrap(),
            "bbbb\n"
        );
        assert_eq!(run("aaaa", "refs_unlock"), 0);
        assert!(lock.exists());
        assert_eq!(run("bbbb", "refs_unlock"), 0);
        assert!(!lock.exists());
        // Nothing renamed aside is left.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// The prune waits for the references' lock another writer holds, and removes a lock its
    /// writer left minutes ago.
    #[test]
    fn the_prune_waits_for_the_references_lock() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().to_str().unwrap();
        let builds = dir.path().join("twr/runner/builds");
        let old = "00000000000000d4";
        let make = || {
            std::fs::create_dir_all(builds.join(old)).unwrap();
            age(&builds.join(old), 0);
            go_w_off(&dir.path().join("twr"));
        };
        let lock = builds.join(twr_core::BUILD_REFS_LOCK);
        let keep = "00000000000000a1";
        make();
        std::fs::create_dir(&lock).unwrap();
        let script = prune_script(data, "h-gpu", &[keep, keep]);
        let start = std::time::Instant::now();
        let release = {
            let lock = lock.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(1500));
                std::fs::remove_dir(&lock).unwrap();
            })
        };
        assert_eq!(sh(&script, None, &[]).0, 0);
        release.join().unwrap();
        assert!(start.elapsed() >= Duration::from_millis(1400));
        assert!(!builds.join(old).exists());
        assert!(!lock.exists(), "let go");
        // A lock left behind long ago.
        make();
        std::fs::create_dir(&lock).unwrap();
        age(&lock, 0);
        let start = std::time::Instant::now();
        assert_eq!(sh(&script, None, &[]).0, 0);
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(!builds.join(old).exists());
        assert!(!lock.exists());
    }

    #[test]
    fn ssh_failures_say_what_to_do() {
        let lines = |l: &str| vec!["debug1: x".to_string(), l.to_string()];
        let why = ssh_failure("gpu", &lines("me@gpu: Permission denied (publickey).")).unwrap();
        assert!(
            why.contains("BatchMode") && why.contains("ssh-agent"),
            "{why}"
        );
        let why = ssh_failure("gpu", &lines("Host key verification failed.")).unwrap();
        assert!(why.contains("host key"), "{why}");
        let why = ssh_failure(
            "gpu",
            &lines("ssh: Could not resolve hostname gpu: Name or service not known"),
        )
        .unwrap();
        assert!(why.contains("[boxes.gpu] ssh"), "{why}");
        assert_eq!(ssh_failure("gpu", &lines("Connection reset")), None);
        assert!(error_like("Error: the box daemon did not start"));
        assert!(error_like("twr-box: warning: logind ends your processes"));
        assert!(!error_like(
            "twr-box: started the box daemon 0123456789abcdef"
        ));
    }

    /// A replace goes at once after the session that asked for it; from the second one in a
    /// row that is not welcomed (a daemon refused every time), the backoff applies.
    #[test]
    fn refused_replaces_back_off() {
        let mut n = 0;
        let mut at_once = Vec::new();
        for _ in 0..3 {
            n = next_refused_replaces(n, false, true);
            at_once.push(replace_at_once(n, true));
        }
        assert_eq!(at_once, [true, false, false]);
        // A welcomed session (a switch's) starts the count again.
        n = next_refused_replaces(n, true, true);
        assert_eq!((n, replace_at_once(n, true)), (0, true));
        assert!(!replace_at_once(0, false));
    }

    #[test]
    fn backoff_is_short_at_first_then_capped() {
        let s = Duration::from_secs;
        assert_eq!(backoff_delay(0, s(0), 0.5), s(1));
        assert_eq!(backoff_delay(1, s(2), 0.5), s(2));
        assert_eq!(backoff_delay(2, s(5), 0.5), s(4));
        assert_eq!(backoff_delay(5, s(30), 0.5), s(5));
        assert_eq!(backoff_delay(5, s(61), 0.5), s(30));
        assert_eq!(backoff_delay(40, s(3600), 0.99), s(30));
        assert!(backoff_delay(0, s(0), 0.0) >= Duration::from_millis(800));
    }

    /// What the hub reads from ssh is bounded while it is read: a line is cut (the rest
    /// skipped, line ends and a `\r` dropped, never inside a character), a stream longer than
    /// allowed is an error, and a relay's stderr gives a session at most
    /// [`SESSION_NOTES_MAX`] notes, each cut, and keeps the latest lines only.
    #[tokio::test]
    async fn what_ssh_prints_is_bounded_while_it_is_read() {
        let long = "x".repeat(10 * NOTE_MAX_BYTES);
        let input = format!("short\r\n{long}\nnext\né{long}\nlast");
        let mut r = tokio::io::BufReader::with_capacity(64, input.as_bytes());
        let mut got = Vec::new();
        while let Some(l) = read_line_capped(&mut r, NOTE_MAX_BYTES).await.unwrap() {
            got.push(l);
        }
        assert_eq!(
            got.len(),
            5,
            "{:?}",
            got.iter().map(String::len).collect::<Vec<_>>()
        );
        assert_eq!(got[0], "short");
        assert_eq!(got[1], "x".repeat(NOTE_MAX_BYTES));
        assert_eq!(got[2], "next");
        assert_eq!(got[3].len(), NOTE_MAX_BYTES);
        assert!(got[3].starts_with("éx"));
        assert_eq!(got[4], "last");
        // Cut inside a character: the replacement does not make it longer.
        let mut r = tokio::io::BufReader::new("aé\n".as_bytes());
        let l = read_line_capped(&mut r, 2).await.unwrap().unwrap();
        assert!(l.len() <= 2 && l.starts_with('a'), "{l:?}");
        assert_eq!(read_line_capped(&mut r, 2).await.unwrap(), None);

        let ok = vec![b'y'; SSH_OUTPUT_MAX];
        assert_eq!(
            read_capped(&ok[..], SSH_OUTPUT_MAX, "x")
                .await
                .unwrap()
                .len(),
            SSH_OUTPUT_MAX
        );
        let over = vec![b'y'; SSH_OUTPUT_MAX + 1];
        let e = read_capped(&over[..], SSH_OUTPUT_MAX, "ssh to box gpu (stdout)")
            .await
            .unwrap_err();
        assert!(
            e.contains("ssh to box gpu (stdout) printed over 64 KiB"),
            "{e}"
        );

        let b = SshBox::new("gpu", &BoxConfig::default());
        let mut stderr = String::new();
        for i in 0..SESSION_NOTES_MAX + 500 {
            stderr.push_str(&format!("twr-box: line {i} {long}\n"));
        }
        let lines = Mutex::default();
        read_notes(&b, stderr.as_bytes(), &lines).await;
        let lines = lines.into_inner().unwrap();
        assert_eq!(lines.len(), 2 * NOTES_KEPT);
        assert!(lines.iter().all(|l| l.len() <= NOTE_MAX_BYTES));
        assert!(
            lines
                .back()
                .unwrap()
                .starts_with(&format!("twr-box: line {} ", SESSION_NOTES_MAX - 1)),
            "the lines past the limit are dropped"
        );
        let st = b.status();
        assert!(st.notes.iter().all(|n| n.len() <= NOTE_MAX_BYTES));
        assert_eq!(
            st.notes.back().unwrap(),
            &format!(
                "the box printed over {SESSION_NOTES_MAX} lines on this connection: the rest \
                 is dropped"
            )
        );
    }

    /// What an ssh leaves in its process group (a `ProxyCommand`'s helper, a wrapper's) ends
    /// with it: after a script's ssh completed ([`run_ssh_command`]), after a session's ssh
    /// ended ([`Ssh::finish`]), and when one is dropped while it runs. The helper here is a
    /// `sleep` that the fake ssh leaves running in the background, its pid in a file.
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ssh_leaves_nothing_of_its_group_behind() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = dir.path().join("fake-ssh");
        std::fs::write(
            &fake,
            "#!/bin/sh\nsh -c 'echo $$ > \"$0\"; exec sleep 600' \"$HELPER_PID\" \
             > /dev/null 2>&1 < /dev/null &\n\
             while [ ! -s \"$HELPER_PID\" ]; do sleep 0.05; done\n\
             [ -n \"$STAY\" ] && sleep 600\n\
             echo said\nexit 3\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let alive = |pid: i32| {
            std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .and_then(|s| {
                    s.rsplit_once(')')
                        .map(|(_, r)| !r.trim_start().starts_with('Z'))
                })
                .unwrap_or(false)
        };
        let gone = |pid_file: &std::path::Path| {
            let pid: i32 = std::fs::read_to_string(pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while alive(pid) {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the helper {pid} is still running"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let cmd = |pid_file: &std::path::Path, stay: bool| {
            let mut c = Command::new(&fake);
            c.env("HELPER_PID", pid_file)
                .env("STAY", if stay { "1" } else { "" })
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .process_group(0)
                .kill_on_drop(true);
            c
        };
        // A script's ssh: its exit status kept, the helper gone.
        let pid_file = dir.path().join("script.pid");
        let (code, out, _) =
            run_ssh_command(cmd(&pid_file, false), "gpu", None, Duration::from_secs(20))
                .await
                .unwrap();
        assert_eq!((code, out.as_str()), (Some(3), "said\n"));
        gone(&pid_file);
        // A session's ssh that ended.
        let pid_file = dir.path().join("session.pid");
        let mut ssh = Ssh::spawn(cmd(&pid_file, false)).unwrap();
        let status = ssh.finish().await.unwrap();
        assert_eq!(status.code(), Some(3));
        drop(ssh);
        gone(&pid_file);
        // One dropped while it runs.
        let pid_file = dir.path().join("dropped.pid");
        let ssh = Ssh::spawn(cmd(&pid_file, true)).unwrap();
        while !std::fs::read_to_string(&pid_file).is_ok_and(|s| s.ends_with('\n')) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        drop(ssh);
        gone(&pid_file);
    }

    /// A supervisor that does not end by itself is aborted and joined: once `ended` returns,
    /// it no longer runs (what it held is dropped).
    #[tokio::test]
    async fn an_ended_supervisor_no_longer_runs() {
        let b = SshBox::new("gpu", &BoxConfig::default());
        let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
        struct Dropped(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let held = Dropped(Some(tx));
        *b.task.lock().unwrap() = Some(tokio::spawn(async move {
            let _held = held;
            std::future::pending::<()>().await;
        }));
        b.ended(Duration::from_millis(50)).await;
        assert!(
            rx.try_recv().is_ok(),
            "what the supervisor held was dropped by the time ended returned"
        );
    }

    #[test]
    fn notes_keep_the_latest_and_the_newest_error() {
        let b = SshBox::new("gpu", &BoxConfig::default());
        for i in 0..30 {
            b.note(&format!("twr-box: line {i}"));
        }
        b.remark("é".repeat(NOTE_MAX_BYTES));
        assert!(b.status().notes.back().unwrap().len() <= NOTE_MAX_BYTES);
        b.note("Error: something broke");
        b.note("twr-box: started the box daemon 0123456789abcdef");
        let st = b.status();
        assert_eq!(st.notes.len(), NOTES_KEPT);
        assert_eq!(
            st.notes.back().unwrap(),
            "twr-box: started the box daemon 0123456789abcdef"
        );
        assert_eq!(st.last_error.as_deref(), Some("Error: something broke"));
    }
}
