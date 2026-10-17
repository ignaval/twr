//! The runner protocol: what a runner on a remote box and the hub say to each other over one
//! WebSocket connection (JSON text frames, one message per frame, tagged by `type`).
//!
//! # Flow
//!
//! 1. The runner connects and sends [`RunnerMsg::Hello`]: its box, its protocol version and a
//!    fresh random nonce (nothing else: the hello is small, and carries no secret).
//! 2. The hub answers [`HubMsg::Challenge`]: a fresh nonce of its own and its proof that it
//!    knows the box's token ([`auth_proof`] by [`Prover::Hub`] over the box and both nonces).
//!    The runner checks that proof (constant time) before it sends anything else: a peer that
//!    does not know the token (another user's process on a loopback port the hub left free,
//!    say) gets nothing more from it. That holds only while such a peer can't reach the hub's
//!    listener too: the proofs bind the box and the nonces, not the transport, so a peer that
//!    can (a TLS listener open to the network) can relay the handshake between the two and
//!    then drive the box. `ssh -o ExitOnForwardFailure=yes` and a listener few machines can
//!    reach are what prevent that (docs/security-model.md). The runner then sends
//!    [`RunnerMsg::Auth`] with its own proof ([`Prover::Runner`]); the hub checks it (constant
//!    time) and answers [`HubMsg::Authenticated`]. The token itself never crosses the
//!    connection, and a proof is bound to both nonces and to its direction, so neither side
//!    can replay one. An unknown box gets a challenge with a random proof (the runner cannot
//!    tell it from a wrong token), and a bad proof `error{unauthorized}`.
//!
//!    Sealed connections (an SSH box's daemon, [`crate::sealed`]) have no token: the sealed
//!    channel the protocol runs inside already proved the hub (its key signed the channel's
//!    opening) and binds the connection to the box. The same messages go through, but the
//!    runner does not check the challenge's proof and answers [`SEALED_AUTH_PROOF`], and the
//!    hub ignores the runner's proof (the hub's ssh dialed the box: there is nothing more to
//!    check on its side).
//! 3. The runner sends [`RunnerMsg::Inventory`] (version, running sessions, in-flight command
//!    ids, highest spooled `rseq` per thread), which may be large: only an authenticated
//!    connection may send large messages. The hub answers [`HubMsg::Welcome`] with a fresh
//!    connection epoch and its ack watermark per thread of the box; it closes any older
//!    connection of the same box (fencing). On failure, at any step, it sends
//!    [`HubMsg::Error`] and closes.
//! 4. The runner drops spooled frames at or below each watermark, replays the rest in spool
//!    order (per thread: `rseq` order), sends [`RunnerMsg::Synced`], then streams live frames.
//! 5. The hub commits each [`RunnerMsg::Event`] / [`RunnerMsg::Exited`] and then sends
//!    [`HubMsg::Ack`]. Acks are cumulative per thread: `ack{thread, rseq}` acknowledges every
//!    frame of `thread` with `rseq` at or below it; the hub acks a batch of frames at once
//!    (after one sync to disk), so an ack may come a little later and cover several frames.
//!    Each frame carries the time the runner captured it (`at_ms`, optional: the hub records
//!    the event at that time, within the thread's order and never in the future).
//! 6. The hub drives the box with [`HubMsg::Cmd`]; each carries a durable `cmd_id`, and the
//!    runner answers [`RunnerMsg::CmdResult`] exactly once per id (a repeated id gets the
//!    stored result again without re-executing).
//!
//! `rseq` is per thread, starts at 1, never repeats and has no gaps on the wire: the runner
//! allocates it durably together with spooling the frame, before sending. A frame with
//! `rseq <= watermark` is a duplicate (ack it, do not append); `rseq > watermark + 1` is a gap
//! (close the connection; the runner replays); `rseq` 0 is never valid (the hub closes the
//! connection).
//!
//! Every message type is listed in [`RunnerMsg`] (runner to hub) and [`HubMsg`] (hub to
//! runner). [`PROTOCOL_VERSION`] changes on any incompatible change; the hub rejects a hello
//! with a different version (`error{code: "protocol"}`, before any authentication: the hello
//! of another version need not have the same shape).

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::adapter::SpawnConfig;
use crate::event::{Answer, EventBody};
use crate::git::{
    DeleteRefs, RemoveOutcome, RestoreRequest, TrashPurge, TrashRequest, WorktreeCreated,
    WorktreeSpec,
};

/// Whether `run` can be a run id ([`Op::Spawn`] `run`): 1 to 64 ASCII letters, digits, `-`
/// or `_` (it goes into the runtime's environment as its marker).
pub fn valid_run_id(run: &str) -> bool {
    !run.is_empty()
        && run.len() <= 64
        && run
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Version of this protocol, sent in [`Hello::protocol`].
///
/// 2: the hub names the run of a `spawn` / `resume` ([`Op::Spawn`] `run`), and a `send` names
/// the run it is for.
///
/// 3: a spawn carries the reasoning effort ([`WireSpawnConfig::effort`]); a runner before it
/// would drop it while the hub records the effort as applied. And the git ops of
/// `thread.delete` ([`Op::WorktreeTrash`], [`Op::BranchOnlyCommits`],
/// [`Op::WorktreeInventory`]) and of the trash ([`Op::TrashList`], [`Op::TrashPurge`],
/// [`Op::TrashRestore`], [`Op::TrashRestoreUndo`]), which a runner before them would leave
/// unanswered.
///
/// 4: session task events and runtime-initiated turns (`from: runtime`), which an older hub
/// cannot decode.
///
/// 5: the disk op ([`Op::DiskUsage`]), which a runner before it would leave unanswered.
///
/// 6: the mutual challenge-response handshake (a small [`Hello`], [`HubMsg::Challenge`],
/// [`RunnerMsg::Auth`], [`HubMsg::Authenticated`], then [`RunnerMsg::Inventory`]): the token
/// no longer crosses the connection, and the runner checks the hub before it trusts it.
///
/// 7: the agents op ([`Op::Agents`]: the box's agent CLIs, found or not, their versions and
/// whether they are signed in), which a runner before it would leave unanswered; the
/// inventory's [`Inventory::agents`] came with it (additive: an older runner's inventory has
/// none). No spooled frame changed: a protocol 6 spool's frames decode as they are.
///
/// A runner's spool keeps its frames across a restart, and so across an upgrade (it records
/// the protocol they were written under): a bump that changes how spooled frames encode must
/// migrate the spool, or the next runner refuses to open one that holds such frames.
pub const PROTOCOL_VERSION: u32 = 7;

/// The proof a runner sends in [`RunnerMsg::Auth`] on a sealed connection (see the module
/// docs): none. The hub does not check it there.
pub const SEALED_AUTH_PROOF: &str = "";

/// Largest WebSocket message either side accepts (16 MiB).
pub const MAX_FRAME_BYTES: usize = 16 << 20;

/// The most bytes the hub's machine reads from each of stdout and stderr of an ssh that runs a
/// script or a `twr runner` report command on an SSH box (`twr_hub::ssh_box::SSH_OUTPUT_MAX`):
/// what a box-side report prints must stay well below it
/// (`twr_runner::repos::REPORT_MAX_BYTES`), a login script's chatter aside.
pub const SSH_OUTPUT_MAX: usize = 64 * 1024;

/// How long a runner keeps the result of a durable command ([`is_durable_cmd_id`]) at least,
/// counted from when it ran: a repeat of the id within this time returns the stored result
/// instead of running again. Other results are kept for a shorter time (a day, at most the
/// newest thousand).
pub const RESULT_RETENTION: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);

/// How long after recording a durable command the hub may still send it (again): one day less
/// than [`RESULT_RETENTION`], a margin for delivery delays and clock skew between hub and box.
/// A recorded command older than this is dropped instead of resent (its effect is unknown: the
/// box may have run it long ago and forgotten the result), and the hub reports that.
pub const RESEND_HORIZON: std::time::Duration = std::time::Duration::from_secs(6 * 24 * 3600);

// The hub stops resending well before the runner forgets.
const _: () = assert!(RESEND_HORIZON.as_secs() + 3600 <= RESULT_RETENTION.as_secs());

/// Id prefixes of durable commands: the hub records them before sending and resends them
/// until answered (`send:<thread>:<turn>`, `respond:<id>`, `stop:<thread>:<run>`), so a
/// runner must remember their results for [`RESULT_RETENTION`].
pub const DURABLE_CMD_PREFIXES: &[&str] = &["send:", "respond:", "stop:"];

/// Whether `cmd_id` is a durable command's (see [`DURABLE_CMD_PREFIXES`]).
pub fn is_durable_cmd_id(cmd_id: &str) -> bool {
    DURABLE_CMD_PREFIXES.iter().any(|p| cmd_id.starts_with(p))
}

/// A secret string (the runner token). `Debug` never shows it; serde passes it through
/// unchanged.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    pub fn new(s: impl Into<String>) -> Self {
        Secret(s.into())
    }

    /// The secret itself. Never log it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Compares with `other` in time that depends only on the lengths, not on where the
    /// strings differ.
    pub fn ct_eq(&self, other: &str) -> bool {
        let (a, b) = (self.0.as_bytes(), other.as_bytes());
        let mut diff = a.len() ^ b.len();
        for i in 0..a.len().max(b.len()) {
            let x = a.get(i).copied().unwrap_or(0);
            let y = b.get(i).copied().unwrap_or(0);
            diff |= usize::from(x ^ y);
        }
        diff == 0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl From<String> for Secret {
    fn from(s: String) -> Self {
        Secret(s)
    }
}

/// Shortest box token the hub (`[boxes.<name>] token`) and the runner (`--token-file`,
/// `TWR_RUNNER_TOKEN`) accept, in characters.
pub const TOKEN_MIN_LEN: usize = 16;

/// Box tokens shorter than this, in characters, are accepted with a warning. A token from
/// [`new_token`] (`twr runner token`) has 64.
pub const TOKEN_GOOD_LEN: usize = 32;

/// 32 random bytes from the OS (`/dev/urandom`), hex-encoded: a box token (`twr runner
/// token`), the orchestrator's token.
pub fn new_token() -> anyhow::Result<String> {
    use std::io::Read;
    let mut buf = [0u8; 32];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| anyhow::anyhow!("reading /dev/urandom for a random token: {e}"))?;
    Ok(hex::encode(buf))
}

/// Checks the length of `token`, a box token that `what` names in the messages
/// (`boxes.gpu.token`, `the runner token`): an error below [`TOKEN_MIN_LEN`] characters, a
/// warning (`Ok(Some)`) below [`TOKEN_GOOD_LEN`]. Neither message holds the token.
pub fn check_token_len(token: &str, what: &str) -> Result<Option<String>, String> {
    let n = token.chars().count();
    if n < TOKEN_MIN_LEN {
        Err(format!(
            "{what} is too short ({n} characters; at least {TOKEN_MIN_LEN}, {TOKEN_GOOD_LEN} or \
             more recommended): `twr runner token` prints a new one"
        ))
    } else if n < TOKEN_GOOD_LEN {
        Ok(Some(format!(
            "{what} is short ({n} characters; {TOKEN_GOOD_LEN} or more recommended): `twr runner \
             token` prints a new one"
        )))
    } else {
        Ok(None)
    }
}

/// Checks a box name (`[boxes.<name>]`, `twr runner --box`): box names become file names
/// (`runner-<box>.db`) and parts of paths on the box, so `[A-Za-z0-9._-]`, not starting with
/// `.`, at most 64 bytes.
pub fn validate_box_name(name: &str) -> anyhow::Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "invalid box name {name:?}: use letters, digits, '.', '_' or '-' (not starting with \
             '.' or '-')"
        ))
    }
}

/// Length of a handshake nonce ([`new_nonce`]) in hex digits: 32 random bytes.
pub const NONCE_HEX_LEN: usize = 64;

/// A fresh handshake nonce ([`Hello::nonce`], [`HubMsg::Challenge`] `nonce`): 32 random bytes
/// from the OS, lowercase hex.
pub fn new_nonce() -> anyhow::Result<String> {
    use ring::rand::SecureRandom;
    let mut buf = [0u8; NONCE_HEX_LEN / 2];
    ring::rand::SystemRandom::new()
        .fill(&mut buf)
        .map_err(|_| anyhow::anyhow!("the OS gave no random bytes for a nonce"))?;
    Ok(hex::encode(buf))
}

/// Whether `nonce` has the shape of one from [`new_nonce`] (exactly [`NONCE_HEX_LEN`]
/// lowercase hex digits). Each side refuses the other's nonce otherwise, so neither can be
/// made to prove anything over a nonce the other did not draw the usual way (an empty one,
/// say).
pub fn valid_nonce(nonce: &str) -> bool {
    nonce.len() == NONCE_HEX_LEN
        && nonce
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Which side of the handshake proves it knows the box's token ([`auth_proof`]). The side is
/// part of what is signed, so a proof one side made is never a valid proof of the other's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prover {
    /// The hub, in [`HubMsg::Challenge`].
    Hub,
    /// The runner, in [`RunnerMsg::Auth`].
    Runner,
}

/// What a proof signs: the prover's label, then the box name and both nonces, each with its
/// length (4 bytes, big-endian) in front, so no two different inputs sign the same bytes.
fn proof_input(by: Prover, box_name: &str, runner_nonce: &str, hub_nonce: &str) -> Vec<u8> {
    let label: &[u8] = match by {
        Prover::Hub => b"twr hub",
        Prover::Runner => b"twr runner",
    };
    let mut m = Vec::with_capacity(
        label.len() + 12 + box_name.len() + runner_nonce.len() + hub_nonce.len(),
    );
    m.extend_from_slice(label);
    for part in [box_name, runner_nonce, hub_nonce] {
        let len = u32::try_from(part.len()).unwrap_or(u32::MAX);
        m.extend_from_slice(&len.to_be_bytes());
        m.extend_from_slice(part.as_bytes());
    }
    m
}

/// The handshake proof `by` sends for box `box_name` (see the module docs): HMAC-SHA256 keyed
/// with the box's token over the prover's label (`twr hub` / `twr runner`), the box name, the
/// runner's nonce and the hub's nonce; lowercase hex. It shows that its sender knows the token
/// without showing the token.
pub fn auth_proof(
    token: &Secret,
    by: Prover,
    box_name: &str,
    runner_nonce: &str,
    hub_nonce: &str,
) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.expose().as_bytes());
    let tag = ring::hmac::sign(&key, &proof_input(by, box_name, runner_nonce, hub_nonce));
    hex::encode(tag.as_ref())
}

/// Whether `proof` is [`auth_proof`] of `by` for these inputs, compared in constant time
/// (ring's HMAC verification). A proof that is not hex, or not of the right length, is not.
/// An empty token proves nothing.
pub fn check_auth_proof(
    token: &Secret,
    by: Prover,
    box_name: &str,
    runner_nonce: &str,
    hub_nonce: &str,
    proof: &str,
) -> bool {
    if token.is_empty() {
        return false;
    }
    let Ok(tag) = hex::decode(proof) else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, token.expose().as_bytes());
    ring::hmac::verify(
        &key,
        &proof_input(by, box_name, runner_nonce, hub_nonce),
        &tag,
    )
    .is_ok()
}

/// A runtime session running on the runner's box (reported in [`Inventory::running`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunningSession {
    /// Thread id.
    pub thread: String,
    /// Runtime (adapter) name.
    pub runtime: String,
    /// Session id, once the runtime reported one (`thread.started` / `thread.resumed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Process id (`ext.twr.pid` of the session's start event), when the adapter reported one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// The runner's id of this run of the session (fresh on every `spawn` / `resume`; its
    /// frames carry it, and a `stop` naming it stops only this run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

/// The first message of every connection (see the module docs): small, and without a secret.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Hello {
    /// Box name (`[boxes.<name>]` in the hub config).
    #[serde(rename = "box")]
    pub box_name: String,
    /// [`PROTOCOL_VERSION`] of the runner.
    pub protocol: u32,
    /// The runner's fresh nonce ([`new_nonce`]): the hub's proof covers it, so a proof is
    /// good for this connection only.
    pub nonce: String,
}

/// What the runner has, sent once the connection is authenticated
/// ([`RunnerMsg::Inventory`]); the hub answers it with the welcome.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Inventory {
    /// Runner version (`CARGO_PKG_VERSION`), for `runner.connected{version}`.
    pub version: String,
    /// Epoch of the runner's previous welcome in this process, if any. Informational only:
    /// the hub numbers connections with its own counter and never uses it (any value is
    /// accepted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub epoch_hint: Option<u64>,
    /// Sessions running on the box now.
    pub running: Vec<RunningSession>,
    /// Commands received and not finished: still executing, or cut short by a runner crash
    /// (those answer `error{code: "interrupted"}` when repeated).
    pub in_flight: Vec<String>,
    /// Highest spooled (unacknowledged) `rseq` per thread: what the replay will contain.
    pub acks_needed: BTreeMap<String, u64>,
    /// Runtimes (adapter names) available on the box.
    pub runtimes: Vec<String>,
    /// Directory under which the box keeps thread worktrees
    /// (`<worktrees>/<project>/<thread-name>`).
    pub worktrees: PathBuf,
    /// The runner's build id ([`crate::build_id`]: the first 16 hex digits of the SHA-256 of
    /// its executable), when it could read its own executable. It tells two builds of one
    /// version apart (two dirty development builds share their version string), so the hub
    /// knows whether an SSH box runs the build it launched. Absent from an older runner's
    /// inventory, and ignored by an older hub.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// The box's agent CLIs as its runner last probed them (protocol 7: [`Op::Agents`] asks
    /// for them, fresh or not). Empty when the runner has not probed them yet (an inventory
    /// sent right after it started), probes none (a box set up without), or is older.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<AgentInfo>,
    /// How an SSH box's daemon was started, as it tells from its own environment
    /// ([`valid_launch`]): `launchd <label>` (a launchd job in the user's GUI session, macOS),
    /// `systemd <unit>` (a transient user service, Linux) or `detached` (a session of its own,
    /// started from ssh). Absent from a runner that is no box daemon, or an older one; ignored
    /// by an older hub.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
}

/// Whether `launch` is what a box daemon reports of how it was started
/// ([`Inventory::launch`]): `detached`, or `launchd ` or `systemd ` and a name of 1 to 128
/// letters, digits, `.`, `-`, `_` and `@`. Nothing else is kept or shown.
pub fn valid_launch(launch: &str) -> bool {
    if launch == "detached" {
        return true;
    }
    let name = launch
        .strip_prefix("launchd ")
        .or_else(|| launch.strip_prefix("systemd "));
    name.is_some_and(|n| {
        (1..=128).contains(&n.len())
            && n.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'@'))
    })
}

/// Whether an agent CLI on a box is signed in ([`AgentInfo::login`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLogin {
    /// Its status command said it is signed in.
    LoggedIn,
    /// Its status command said it is not.
    LoggedOut,
    /// Not known: the CLI has no status command (Grok Build), it is not installed, or its
    /// status could not be read ([`AgentInfo::note`] says why). Also any value a newer box
    /// sends that this build does not know.
    #[default]
    #[serde(other)]
    Unknown,
}

/// One built-in agent runtime's CLI on a box, as the box's agent probe found it
/// (`twr_adapters::agents`): resolved as the box would start the runtime (its `TWR_*_BIN`,
/// else on the `PATH` its runtimes get), its version, and whether it is signed in. Never more
/// than this: no account detail (email, organization, plan) is kept, and every string is
/// short and printable.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentInfo {
    /// The runtime (`claude-code`, `codex`, `grok-build`).
    pub runtime: String,
    /// Whether its program was found: an executable file on the box.
    pub found: bool,
    /// The executable file found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// The first line its `--version` printed, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Whether it is signed in.
    pub login: AgentLogin,
    /// How it is signed in, when it says and twr knows the word (Claude Code's `authMethod`:
    /// `claude.ai`, `console`, `api-key`, `bedrock`, `vertex`; anything else is `other`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Why something is not known (a fixed phrase: "not found on PATH", "`claude auth
    /// status` took longer than 10s").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// What `twr runner --agents` prints (one JSON object): the box's agents as its runner would
/// probe them, and how the login shell's `PATH` they are resolved with went.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentsReport {
    /// The login shell's `PATH` ([`LoginPathReport`]).
    pub login_path: LoginPathReport,
    /// The agents, one per built-in agent runtime.
    pub agents: Vec<AgentInfo>,
}

/// How asking the box's login shell for its `PATH` went ([`AgentsReport::login_path`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoginPathReport {
    /// The shell asked (`$SHELL`), when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// How many entries its `PATH` gave (put before the runner's own), when it gave one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entries: Option<usize>,
    /// Why it gave none (the runtimes then get the runner's own `PATH`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Runner to hub.
// `Event` carries a whole event body; boxing it would add an allocation per event.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunnerMsg {
    /// First message of a connection.
    Hello(Hello),
    /// The runner's proof that it knows the box's token ([`auth_proof`] by
    /// [`Prover::Runner`]), sent only once the hub's [`HubMsg::Challenge`] proved the same.
    Auth {
        /// The proof (lowercase hex).
        proof: String,
    },
    /// What the runner has, sent once the hub said [`HubMsg::Authenticated`].
    Inventory(Inventory),
    /// A normalized adapter event ([`crate::adapter::AdapterEvent::Emit`]).
    Event {
        /// Thread id.
        thread: String,
        /// Per-thread runner sequence number.
        rseq: u64,
        /// Turn the event belongs to.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<u64>,
        /// `{kind, data}`.
        body: EventBody,
        /// Raw runtime line, if kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<String>,
        /// The run of the session that emitted it ([`RunningSession::run`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
        /// When the runner captured the event (Unix milliseconds): the hub records it at this
        /// time (kept in order within the thread, never in the future), so a frame replayed
        /// from the spool keeps its original time. Absent from older runners (the hub's time).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_ms: Option<u64>,
    },
    /// The runtime process exited ([`crate::adapter::AdapterEvent::Exited`]); the last frame of
    /// a session.
    Exited {
        /// Thread id.
        thread: String,
        /// Per-thread runner sequence number.
        rseq: u64,
        /// Exit code, None if killed by a signal (or stopped by the runner itself).
        #[serde(default)]
        code: Option<i32>,
        /// Last bytes of stderr.
        #[serde(default)]
        stderr_tail: String,
        /// The run of the session that exited ([`RunningSession::run`]); none for the exit a
        /// restarted runner records for a runtime its predecessor left behind.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
        /// When the runner recorded the exit (Unix milliseconds; see [`RunnerMsg::Event`]).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at_ms: Option<u64>,
    },
    /// Sent once per connection, after the replay of the spool: every frame spooled before
    /// the welcome has been sent. The hub can reconcile sessions from here on.
    Synced,
    /// The outcome of [`HubMsg::Cmd`] `cmd_id`.
    CmdResult(CmdResult),
    /// Keepalive; answered with `pong`.
    Ping,
    /// Answer to the hub's `ping`.
    Pong,
}

/// Hub to runner.
// `Cmd` carries a spawn configuration; boxing it would add an allocation per command.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HubMsg {
    /// The answer to the hello: the hub's nonce and its proof that it knows the box's token
    /// ([`auth_proof`] by [`Prover::Hub`]; random for a box the hub does not know).
    Challenge {
        /// The hub's fresh nonce ([`new_nonce`]).
        nonce: String,
        /// The proof (lowercase hex).
        proof: String,
    },
    /// The runner's [`RunnerMsg::Auth`] proof is good: the connection is authenticated, and
    /// the runner sends its [`RunnerMsg::Inventory`] next.
    Authenticated,
    /// Accepts the connection (the answer to the inventory).
    Welcome {
        /// Connection epoch (increases per accepted connection of the box).
        epoch: u64,
        /// Highest committed `rseq` per thread of this box. Threads not listed: 0.
        #[serde(default)]
        watermarks: BTreeMap<String, u64>,
    },
    /// Every frame of `thread` with `rseq` at or below this one is committed.
    Ack {
        /// Thread id.
        thread: String,
        /// Committed up to here.
        rseq: u64,
    },
    /// Runs an operation on the box.
    Cmd {
        /// Durable command id (unique per hub).
        cmd_id: String,
        /// What to do.
        op: Op,
    },
    /// The hub refuses the connection (sent right before closing it).
    Error {
        /// See [`hub_errors`].
        code: String,
        /// Human-readable detail (never contains the token).
        #[serde(default)]
        message: String,
    },
    /// Keepalive; answered with `pong`.
    Ping,
    /// Answer to the runner's `ping`.
    Pong,
}

/// Codes of [`HubMsg::Error`].
pub mod hub_errors {
    /// A bad proof (wrong token), or an unknown box.
    pub const UNAUTHORIZED: &str = "unauthorized";
    /// [`super::Hello::protocol`] is not the hub's version.
    pub const PROTOCOL: &str = "protocol";
    /// A newer connection of the same box replaced this one.
    pub const FENCED: &str = "fenced";
    /// The hub cannot number another connection of the box (its epoch counter is exhausted;
    /// a hub restart starts it over).
    pub const EXHAUSTED: &str = "exhausted";
}

/// An operation the hub runs on the runner's box. Every `ThreadHost` / `GitOps` call of a
/// remote box maps to one of these.
///
/// Results (`cmd_result.result` when `ok`):
///
/// | op | result |
/// |----|--------|
/// | `spawn`, `resume`, `send`, `respond`, `stop` | `null` (events follow on the thread) |
/// | `attach_info` | [`AttachInfo`] or `null` (the runtime has no interactive CLI) |
/// | `repo_root` | path string or `null` |
/// | `git_common_dir` | path string |
/// | `worktree_create` | [`WireWorktreeCreated`] |
/// | `worktree_remove` | [`WireRemoveOutcome`] |
/// | `worktree_prune` | `null` |
/// | `worktree_exists` | bool |
/// | `git_snapshot` | [`crate::event::GitSnapshot`] |
/// | `worktree_trash` | [`crate::git::TrashOutcome`] |
/// | `branch_only_commits` | `[`[`crate::git::OnlyCommits`]`]`, one per target in order |
/// | `worktree_inventory` | [`crate::git::WorktreeInventory`] |
/// | `trash_list` | `[`[`crate::git::TrashItem`]`]` |
/// | `trash_purge` | [`crate::git::PurgeOutcome`] |
/// | `trash_restore` | [`crate::git::RestoreOutcome`] |
/// | `trash_restore_undo` | bool |
/// | `disk_usage` | [`crate::git::DiskReading`] |
/// | `agents` | `[`[`AgentInfo`]`]` |
///
/// `git_common_dir`, `worktree_trash`, `branch_only_commits`, `worktree_inventory` and the
/// `trash_*` ops came with protocol 3 ([`PROTOCOL_VERSION`]): a runner from before them is refused at its hello, so it is never
/// asked.
///
/// Session ops (`spawn` .. `stop`) run in the order received; the others run concurrently.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Op {
    /// Starts a new session (`Adapter::spawn`). Error `already_running` when the thread has a
    /// live session, `unknown_runtime` when the box lacks the runtime.
    Spawn {
        /// Thread id (overrides `cfg.thread`).
        thread: String,
        /// Runtime name.
        runtime: String,
        /// Spawn configuration.
        cfg: WireSpawnConfig,
        /// First prompt (turn `cfg.prompt_turn`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
        /// The id of the new run ([`RunningSession::run`]; [`valid_run_id`]): the hub names
        /// it, so it knows its session's run before any frame of it arrives. `None`: the
        /// runner makes one up.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
    /// Resumes a session (`Adapter::resume`). Same errors as `spawn`.
    Resume {
        /// Thread id.
        thread: String,
        /// Runtime name.
        runtime: String,
        /// Spawn configuration.
        cfg: WireSpawnConfig,
        /// Session to resume.
        session_id: String,
        /// The id of the new run, as for `spawn`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
    /// Sends a prompt as turn `turn`. Error `not_running` without a live session, or when
    /// the live session is not run `run` (given): the prompt was meant for a run that has
    /// ended, and never reaches another one.
    Send {
        /// Thread id.
        thread: String,
        /// Turn number.
        turn: u64,
        /// Prompt text.
        text: String,
        /// Only this run ([`RunningSession::run`], the hub's session when it was sent);
        /// `None`: whatever runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
    /// Answers an attention item. Error `not_running` without a live session, or when the
    /// live session is not run `run` (given): the run that raised the item has ended, and a
    /// later run numbers its requests anew, so the id could name another question there.
    Respond {
        /// Thread id.
        thread: String,
        /// Attention id (the runtime's own, naming exactly one question of `run`).
        attn_id: String,
        /// The answer.
        answer: Answer,
        /// Only this run ([`RunningSession::run`], the run whose frame raised the item);
        /// `None`: whatever runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
    /// Stops the thread's runtime; its `exited` frame follows. A thread without a live
    /// session is fine (nothing to do), and so is one whose session is not run `run`, when
    /// given: a stop meant for a run that has ended never reaches its replacement.
    Stop {
        /// Thread id.
        thread: String,
        /// Only this run ([`RunningSession::run`]); `None`: whatever runs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        run: Option<String>,
    },
    /// The interactive command for `twr attach` (`Adapter::attach_command`).
    AttachInfo {
        /// Thread id.
        thread: String,
        /// Runtime name.
        runtime: String,
        /// Spawn configuration (its `env` is ignored, see `Adapter::attach_command`).
        cfg: WireSpawnConfig,
        /// Session to open.
        session_id: String,
    },
    /// `GitOps::repo_root`.
    RepoRoot {
        /// Directory to look up.
        dir: PathBuf,
    },
    /// `GitOps::git_common_dir`.
    GitCommonDir {
        /// A directory of the repository.
        dir: PathBuf,
    },
    /// `GitOps::worktree_create`.
    WorktreeCreate {
        /// Repository.
        repo: PathBuf,
        /// Worktree path.
        path: PathBuf,
        /// Branch.
        branch: String,
        /// Base of a new branch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    /// `GitOps::worktree_remove`.
    WorktreeRemove {
        /// Repository.
        repo: PathBuf,
        /// Worktree path.
        path: PathBuf,
        /// The thread's branch: a worktree with anything else checked out is kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// The ignored paths the removal may delete with the worktree (the hub's `[threads]
        /// park_disposable`); absent: the runner's default list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disposable: Option<Vec<String>>,
    },
    /// `GitOps::worktree_prune`.
    WorktreePrune {
        /// Repository.
        repo: PathBuf,
    },
    /// `GitOps::worktree_exists`.
    WorktreeExists {
        /// Path.
        path: PathBuf,
    },
    /// `GitOps::snapshot`.
    GitSnapshot {
        /// Work tree.
        path: PathBuf,
        /// The ref its merge readiness is read against (a runner from before it ignores it).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base: Option<String>,
    },
    /// `GitOps::worktree_trash`.
    WorktreeTrash {
        /// What to move to the trash.
        request: TrashRequest,
        /// The ignored paths the deletion deletes instead of moving them to the trash (the
        /// hub's `[threads] park_disposable`); absent (an older hub): none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disposable: Option<Vec<String>>,
    },
    /// `GitOps::branch_only_commits`.
    BranchOnlyCommits {
        /// The threads deleted together, each with its repository: those sharing a git common
        /// directory are one deletion set, everything of it counting as gone.
        targets: Vec<DeleteRefs>,
    },
    /// `GitOps::worktree_inventory`.
    WorktreeInventory {
        /// Repository.
        repo: PathBuf,
        /// Worktree path.
        path: PathBuf,
        /// As for [`Op::WorktreeTrash`]: what the inventory counts as build output the deletion
        /// deletes; absent: none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disposable: Option<Vec<String>>,
    },
    /// `GitOps::trash_list`.
    TrashList {},
    /// `GitOps::trash_purge`.
    TrashPurge {
        /// Which items.
        purge: TrashPurge,
    },
    /// `GitOps::trash_restore`.
    TrashRestore {
        /// What to put back.
        request: RestoreRequest,
    },
    /// `GitOps::trash_restore_undo`.
    TrashRestoreUndo {
        /// The item a restore made a worktree from.
        item: String,
    },
    /// `GitOps::disk_usage` (protocol 5).
    DiskUsage {
        /// The worktree to measure; absent: only the free space where the box's worktrees are.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<PathBuf>,
        /// What counts as build output (the hub's `[threads] park_disposable`); absent: the
        /// runner's default list.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        disposable: Option<Vec<String>>,
    },
    /// The box's agent CLIs ([`AgentInfo`], protocol 7): as the runner last probed them, or
    /// probed again first when `refresh` (a probe started after the request, at most 10 s a
    /// command). A box that probes none answers an empty list.
    Agents {
        /// Probe again before answering (a just installed or signed in CLI).
        #[serde(default)]
        refresh: bool,
    },
}

impl Op {
    /// The thread a session op acts on.
    pub fn thread(&self) -> Option<&str> {
        match self {
            Op::Spawn { thread, .. }
            | Op::Resume { thread, .. }
            | Op::Send { thread, .. }
            | Op::Respond { thread, .. }
            | Op::Stop { thread, .. }
            | Op::AttachInfo { thread, .. } => Some(thread),
            _ => None,
        }
    }

    /// Whether this op changes a session (spawn, resume, send, respond, stop): such ops run
    /// in the order they arrive.
    pub fn is_session_op(&self) -> bool {
        matches!(
            self,
            Op::Spawn { .. }
                | Op::Resume { .. }
                | Op::Send { .. }
                | Op::Respond { .. }
                | Op::Stop { .. }
        )
    }

    /// Whether this op runs git commands that change a repository or a work tree on the box
    /// (each killed past [`crate::git::GIT_CHANGE_TIMEOUT`] and reaped before the runner
    /// answers): the hub waits up to [`crate::git::CHANGE_REQUEST_TIMEOUT`] for its answer,
    /// and while one may still run on the box (unanswered), it starts no other git op there.
    /// A runner finishes every such op it received before it connects again.
    pub fn changes_repository(&self) -> bool {
        matches!(
            self,
            Op::WorktreeCreate { .. }
                | Op::WorktreeRemove { .. }
                | Op::WorktreePrune { .. }
                | Op::WorktreeTrash { .. }
                | Op::TrashPurge { .. }
                | Op::TrashRestore { .. }
                | Op::TrashRestoreUndo { .. }
        )
    }

    /// The op name (its `type` tag).
    pub fn name(&self) -> &'static str {
        match self {
            Op::Spawn { .. } => "spawn",
            Op::Resume { .. } => "resume",
            Op::Send { .. } => "send",
            Op::Respond { .. } => "respond",
            Op::Stop { .. } => "stop",
            Op::AttachInfo { .. } => "attach_info",
            Op::RepoRoot { .. } => "repo_root",
            Op::GitCommonDir { .. } => "git_common_dir",
            Op::WorktreeCreate { .. } => "worktree_create",
            Op::WorktreeRemove { .. } => "worktree_remove",
            Op::WorktreePrune { .. } => "worktree_prune",
            Op::WorktreeExists { .. } => "worktree_exists",
            Op::GitSnapshot { .. } => "git_snapshot",
            Op::WorktreeTrash { .. } => "worktree_trash",
            Op::BranchOnlyCommits { .. } => "branch_only_commits",
            Op::WorktreeInventory { .. } => "worktree_inventory",
            Op::TrashList {} => "trash_list",
            Op::TrashPurge { .. } => "trash_purge",
            Op::TrashRestore { .. } => "trash_restore",
            Op::TrashRestoreUndo { .. } => "trash_restore_undo",
            Op::DiskUsage { .. } => "disk_usage",
            Op::Agents { .. } => "agents",
        }
    }

    /// `worktree_create` for `spec`.
    pub fn worktree_create(spec: WorktreeSpec) -> Op {
        Op::WorktreeCreate {
            repo: spec.repo,
            path: spec.path,
            branch: spec.branch,
            base: spec.base,
        }
    }
}

/// Codes of [`CmdError`].
pub mod cmd_errors {
    /// The box has no adapter by that name.
    pub const UNKNOWN_RUNTIME: &str = "unknown_runtime";
    /// The thread has no live session.
    pub const NOT_RUNNING: &str = "not_running";
    /// The thread already has a live session.
    pub const ALREADY_RUNNING: &str = "already_running";
    /// The runner crashed while running the command; its effect is unknown.
    pub const INTERRUPTED: &str = "interrupted";
    /// The operation failed (message has the detail).
    pub const FAILED: &str = "failed";
}

/// Why a command failed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CmdError {
    /// See [`cmd_errors`].
    pub code: String,
    /// Detail.
    pub message: String,
}

impl CmdError {
    /// An error with `code` and `message`.
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        CmdError {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for CmdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for CmdError {}

/// `cmd_result` payload.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CmdResult {
    /// The command.
    pub cmd_id: String,
    /// Whether it succeeded.
    pub ok: bool,
    /// The op's result when `ok` (see [`Op`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Why it failed when not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<CmdError>,
}

impl CmdResult {
    /// A result for `cmd_id` from an outcome.
    pub fn new(cmd_id: impl Into<String>, outcome: Result<Value, CmdError>) -> Self {
        let cmd_id = cmd_id.into();
        match outcome {
            Ok(v) => CmdResult {
                cmd_id,
                ok: true,
                result: Some(v),
                error: None,
            },
            Err(e) => CmdResult {
                cmd_id,
                ok: false,
                result: None,
                error: Some(e),
            },
        }
    }

    /// The outcome.
    pub fn into_outcome(self) -> Result<Value, CmdError> {
        if self.ok {
            Ok(self.result.unwrap_or(Value::Null))
        } else {
            Err(self
                .error
                .unwrap_or_else(|| CmdError::new(cmd_errors::FAILED, "no error given")))
        }
    }
}

/// [`SpawnConfig`] on the wire.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WireSpawnConfig {
    /// See [`SpawnConfig::thread`].
    pub thread: String,
    /// See [`SpawnConfig::cwd`] (a path on the runner's box).
    pub cwd: PathBuf,
    /// See [`SpawnConfig::mode`].
    pub mode: String,
    /// See [`SpawnConfig::model`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// See [`SpawnConfig::effort`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// See [`SpawnConfig::system_append`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_append: Option<String>,
    /// See [`SpawnConfig::mcp_config`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_config: Option<Value>,
    /// See [`SpawnConfig::mcp_config_file`] (a path on the runner's box).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mcp_config_file: Option<PathBuf>,
    /// See [`SpawnConfig::allowed_tools`].
    pub allowed_tools: Vec<String>,
    /// See [`SpawnConfig::builtin_tools`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub builtin_tools: Option<Vec<String>>,
    /// See [`SpawnConfig::settings`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings: Option<Value>,
    /// See [`SpawnConfig::thinking`].
    pub thinking: bool,
    /// See [`SpawnConfig::env`].
    pub env: Vec<(String, String)>,
    /// See [`SpawnConfig::resume_session`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume_session: Option<String>,
    /// See [`SpawnConfig::prompt_turn`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_turn: Option<u64>,
    /// See [`SpawnConfig::repo`] (a path on the runner's box). A runner from before it ignores
    /// it (and anchors nothing); a hub from before it sends none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<PathBuf>,
}

impl From<SpawnConfig> for WireSpawnConfig {
    fn from(c: SpawnConfig) -> Self {
        WireSpawnConfig {
            thread: c.thread,
            cwd: c.cwd,
            mode: c.mode,
            model: c.model,
            effort: c.effort,
            system_append: c.system_append,
            mcp_config: c.mcp_config,
            mcp_config_file: c.mcp_config_file,
            allowed_tools: c.allowed_tools,
            builtin_tools: c.builtin_tools,
            settings: c.settings,
            thinking: c.thinking,
            env: c.env,
            resume_session: c.resume_session,
            prompt_turn: c.prompt_turn,
            repo: c.repo,
        }
    }
}

impl From<WireSpawnConfig> for SpawnConfig {
    fn from(c: WireSpawnConfig) -> Self {
        SpawnConfig {
            thread: c.thread,
            cwd: c.cwd,
            mode: c.mode,
            model: c.model,
            effort: c.effort,
            system_append: c.system_append,
            refresh_system_prompt: false,
            mcp_config: c.mcp_config,
            mcp_config_file: c.mcp_config_file,
            allowed_tools: c.allowed_tools,
            builtin_tools: c.builtin_tools,
            settings: c.settings,
            thinking: c.thinking,
            env: c.env,
            resume_session: c.resume_session,
            prompt_turn: c.prompt_turn,
            wrap: Vec::new(),
            repo: c.repo,
        }
    }
}

/// [`WorktreeCreated`] on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WireWorktreeCreated {
    /// See [`WorktreeCreated::new_branch`].
    pub new_branch: bool,
    /// See [`WorktreeCreated::head`].
    pub head: String,
    /// See [`WorktreeCreated::base_ref`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_ref: Option<String>,
}

impl From<WorktreeCreated> for WireWorktreeCreated {
    fn from(w: WorktreeCreated) -> Self {
        WireWorktreeCreated {
            new_branch: w.new_branch,
            head: w.head,
            base_ref: w.base_ref,
        }
    }
}

impl From<WireWorktreeCreated> for WorktreeCreated {
    fn from(w: WireWorktreeCreated) -> Self {
        WorktreeCreated {
            new_branch: w.new_branch,
            head: w.head,
            base_ref: w.base_ref,
        }
    }
}

/// [`RemoveOutcome`] on the wire: `{"outcome": "removed" | "kept_dirty" | "missing" |
/// "kept_checkout" | "kept", "dirty_files"?: n, "head"?: s, "reason"?: s}`. A hub older than
/// `kept` fails to decode it, which it reports as an error (the worktree stays).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WireRemoveOutcome {
    /// See [`RemoveOutcome::Removed`].
    Removed,
    /// See [`RemoveOutcome::KeptDirty`].
    KeptDirty {
        /// Dirty files.
        dirty_files: u64,
    },
    /// See [`RemoveOutcome::Missing`].
    Missing,
    /// See [`RemoveOutcome::KeptCheckout`].
    KeptCheckout {
        /// What is checked out.
        head: String,
    },
    /// See [`RemoveOutcome::Kept`].
    Kept {
        /// Why.
        reason: String,
    },
}

impl From<RemoveOutcome> for WireRemoveOutcome {
    fn from(o: RemoveOutcome) -> Self {
        match o {
            RemoveOutcome::Removed => WireRemoveOutcome::Removed,
            RemoveOutcome::KeptDirty { dirty_files } => {
                WireRemoveOutcome::KeptDirty { dirty_files }
            }
            RemoveOutcome::Missing => WireRemoveOutcome::Missing,
            RemoveOutcome::KeptCheckout { head } => WireRemoveOutcome::KeptCheckout { head },
            RemoveOutcome::Kept { reason } => WireRemoveOutcome::Kept { reason },
        }
    }
}

impl From<WireRemoveOutcome> for RemoveOutcome {
    fn from(o: WireRemoveOutcome) -> Self {
        match o {
            WireRemoveOutcome::Removed => RemoveOutcome::Removed,
            WireRemoveOutcome::KeptDirty { dirty_files } => {
                RemoveOutcome::KeptDirty { dirty_files }
            }
            WireRemoveOutcome::Missing => RemoveOutcome::Missing,
            WireRemoveOutcome::KeptCheckout { head } => RemoveOutcome::KeptCheckout { head },
            WireRemoveOutcome::Kept { reason } => RemoveOutcome::Kept { reason },
        }
    }
}

/// The interactive command of `twr attach` on a remote box (result of `attach_info`): what
/// `Adapter::attach_command` returned, as data. The CLI runs it over ssh.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AttachInfo {
    /// Program.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Working directory on the runner's box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Environment changes: `(key, Some(value))` sets, `(key, None)` removes.
    pub env: Vec<(String, Option<String>)>,
}

impl AttachInfo {
    /// Captures a command (non-UTF-8 parts are converted lossily).
    pub fn from_command(cmd: &Command) -> Self {
        AttachInfo {
            program: cmd.get_program().to_string_lossy().into_owned(),
            args: cmd
                .get_args()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            cwd: cmd.get_current_dir().map(PathBuf::from),
            env: cmd
                .get_envs()
                .map(|(k, v)| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.map(|v| v.to_string_lossy().into_owned()),
                    )
                })
                .collect(),
        }
    }

    /// Rebuilds the command.
    pub fn to_command(&self) -> Command {
        let mut c = Command::new(&self.program);
        c.args(&self.args);
        if let Some(d) = &self.cwd {
            c.current_dir(d);
        }
        for (k, v) in &self.env {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c
    }
}

/// The result value of a successful op, typed by the caller (e.g.
/// [`crate::event::GitSnapshot`] for
/// `git_snapshot`).
pub fn decode_result<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, CmdError> {
    serde_json::from_value(v).map_err(|e| {
        CmdError::new(
            cmd_errors::FAILED,
            format!("malformed result from the runner: {e}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box daemon's launch is one of three shapes, its name short and plain.
    #[test]
    fn launches() {
        for ok in [
            "detached",
            "launchd tools.twr.box.0123456789abcdef.0a1b2c3d",
            "systemd twr-box-0123456789abcdef-0a1b2c3d.service",
            "systemd a@b",
        ] {
            assert!(valid_launch(ok), "{ok}");
        }
        let long = format!("launchd {}", "a".repeat(129));
        for bad in [
            "",
            "setsid",
            "launchd",
            "launchd ",
            "systemd  x",
            "launchd a b",
            "launchd a/b",
            "launchd \u{1b}[0m",
            "Detached",
            long.as_str(),
        ] {
            assert!(!valid_launch(bad), "{bad:?}");
        }
    }
    use crate::event::{AgentText, GitSnapshot, SessionPayload};
    use serde_json::json;

    fn round_trip_runner(m: RunnerMsg) {
        let s = serde_json::to_string(&m).unwrap();
        let back: RunnerMsg = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m, "{s}");
    }

    fn round_trip_hub(m: HubMsg) {
        let s = serde_json::to_string(&m).unwrap();
        let back: HubMsg = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m, "{s}");
    }

    fn cfg() -> WireSpawnConfig {
        WireSpawnConfig {
            thread: "t1".into(),
            cwd: "/w/t1".into(),
            mode: "default".into(),
            model: Some("m".into()),
            effort: Some("high".into()),
            mcp_config: Some(json!({"mcpServers": {}})),
            allowed_tools: vec!["Read".into()],
            builtin_tools: Some(vec![]),
            thinking: true,
            env: vec![("TWR_THREAD".into(), "t1".into())],
            prompt_turn: Some(2),
            ..Default::default()
        }
    }

    #[test]
    fn runner_messages_round_trip() {
        let mut acks = BTreeMap::new();
        acks.insert("t1".to_string(), 7);
        round_trip_runner(RunnerMsg::Hello(Hello {
            box_name: "gpu".into(),
            protocol: PROTOCOL_VERSION,
            nonce: new_nonce().unwrap(),
        }));
        round_trip_runner(RunnerMsg::Auth {
            proof: "ab".repeat(32),
        });
        round_trip_runner(RunnerMsg::Inventory(Inventory {
            version: "0.1.0".into(),
            epoch_hint: Some(3),
            running: vec![RunningSession {
                thread: "t1".into(),
                runtime: "fake".into(),
                session_id: Some("s".into()),
                pid: Some(42),
                run: Some("r1".into()),
            }],
            in_flight: vec!["c1".into()],
            acks_needed: acks,
            runtimes: vec!["fake".into()],
            worktrees: "/data/worktrees".into(),
            build: Some("0123456789abcdef".into()),
            agents: vec![AgentInfo {
                runtime: "claude-code".into(),
                found: true,
                path: Some("/home/u/.local/bin/claude".into()),
                version: Some("2.1.302 (Claude Code)".into()),
                login: AgentLogin::LoggedIn,
                method: Some("claude.ai".into()),
                note: None,
            }],
            launch: Some("launchd tools.twr.box.0123456789abcdef.0a1b2c3d".into()),
        }));
        round_trip_runner(RunnerMsg::Event {
            thread: "t1".into(),
            rseq: 1,
            turn: None,
            body: EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s".into()),
                ..Default::default()
            }),
            raw: Some("{}".into()),
            run: Some("r1".into()),
            at_ms: None,
        });
        round_trip_runner(RunnerMsg::Event {
            thread: "t1".into(),
            rseq: 2,
            turn: Some(1),
            body: EventBody::AgentText(AgentText::default()),
            raw: None,
            run: None,
            at_ms: None,
        });
        round_trip_runner(RunnerMsg::Event {
            thread: "t1".into(),
            rseq: 3,
            turn: None,
            body: EventBody::Unknown {
                kind: "future.kind".into(),
                data: json!({"x": 1}),
            },
            raw: None,
            run: None,
            at_ms: None,
        });
        round_trip_runner(RunnerMsg::Exited {
            thread: "t1".into(),
            rseq: 4,
            code: None,
            stderr_tail: "boom".into(),
            run: Some("r1".into()),
            at_ms: None,
        });
        round_trip_runner(RunnerMsg::Synced);
        round_trip_runner(RunnerMsg::CmdResult(CmdResult::new("c1", Ok(json!(true)))));
        round_trip_runner(RunnerMsg::CmdResult(CmdResult::new(
            "c2",
            Err(CmdError::new(cmd_errors::NOT_RUNNING, "t9")),
        )));
        round_trip_runner(RunnerMsg::Ping);
        round_trip_runner(RunnerMsg::Pong);
    }

    #[test]
    fn hub_messages_round_trip() {
        let mut wm = BTreeMap::new();
        wm.insert("t1".to_string(), 5);
        round_trip_hub(HubMsg::Challenge {
            nonce: new_nonce().unwrap(),
            proof: "cd".repeat(32),
        });
        round_trip_hub(HubMsg::Authenticated);
        round_trip_hub(HubMsg::Welcome {
            epoch: 9,
            watermarks: wm,
        });
        round_trip_hub(HubMsg::Ack {
            thread: "t1".into(),
            rseq: 5,
        });
        round_trip_hub(HubMsg::Error {
            code: hub_errors::UNAUTHORIZED.into(),
            message: "no".into(),
        });
        round_trip_hub(HubMsg::Ping);
        round_trip_hub(HubMsg::Pong);
        let ops = vec![
            Op::Spawn {
                thread: "t1".into(),
                runtime: "fake".into(),
                cfg: cfg(),
                prompt: Some("hi".into()),
                run: None,
            },
            Op::Resume {
                thread: "t1".into(),
                runtime: "fake".into(),
                cfg: cfg(),
                session_id: "s".into(),
                run: None,
            },
            Op::Send {
                thread: "t1".into(),
                turn: 2,
                text: "more".into(),
                run: Some("r1".into()),
            },
            Op::Respond {
                thread: "t1".into(),
                attn_id: "a".into(),
                answer: Answer::allow(),
                run: None,
            },
            Op::Respond {
                thread: "t1".into(),
                attn_id: "a".into(),
                answer: Answer::allow(),
                run: Some("r1".into()),
            },
            Op::Stop {
                thread: "t1".into(),
                run: None,
            },
            Op::Stop {
                thread: "t1".into(),
                run: Some("r1".into()),
            },
            Op::AttachInfo {
                thread: "t1".into(),
                runtime: "fake".into(),
                cfg: cfg(),
                session_id: "s".into(),
            },
            Op::RepoRoot { dir: "/r".into() },
            Op::GitCommonDir { dir: "/r".into() },
            Op::worktree_create(WorktreeSpec {
                repo: "/r".into(),
                path: "/w".into(),
                branch: "twr/x".into(),
                base: Some("main".into()),
            }),
            Op::WorktreeRemove {
                repo: "/r".into(),
                path: "/w".into(),
                branch: Some("twr/x".into()),
                disposable: Some(vec!["target/".into()]),
            },
            Op::WorktreePrune { repo: "/r".into() },
            Op::WorktreeExists { path: "/w".into() },
            Op::GitSnapshot {
                path: "/w".into(),
                base: None,
            },
            Op::WorktreeTrash {
                request: TrashRequest {
                    item: "01J00000000000000000000000".into(),
                    repo: "/r".into(),
                    worktree: "/w".into(),
                    branch: Some("twr/x".into()),
                    other_branches: Some(vec![crate::git::OtherBranch {
                        repo: "/r2".into(),
                        branch: "twr/y".into(),
                    }]),
                    info: Default::default(),
                },
                disposable: Some(vec!["target/".into()]),
            },
            Op::BranchOnlyCommits {
                targets: vec![
                    DeleteRefs {
                        repo: "/r".into(),
                        branch: Some("twr/x".into()),
                        worktree: Some("/w".into()),
                        other_branches: Some(vec![]),
                    },
                    DeleteRefs::default(),
                ],
            },
            Op::WorktreeInventory {
                repo: "/r".into(),
                path: "/w".into(),
                disposable: None,
            },
            Op::TrashList {},
            Op::TrashPurge {
                purge: TrashPurge {
                    items: vec!["01J00000000000000000000000".into()],
                    before_ms: Some(1),
                },
            },
            Op::TrashRestore {
                request: RestoreRequest {
                    item: "01J00000000000000000000000".into(),
                    branch: "twr/x".into(),
                    paths: vec!["/w".into()],
                    other_branches: Some(Vec::new()),
                },
            },
            Op::TrashRestoreUndo {
                item: "01J00000000000000000000000".into(),
            },
            Op::DiskUsage {
                worktree: Some("/w".into()),
                disposable: Some(vec!["target/".into()]),
            },
            Op::Agents { refresh: true },
            Op::Agents { refresh: false },
        ];
        for op in ops {
            let name = op.name();
            let v = serde_json::to_value(HubMsg::Cmd {
                cmd_id: "c".into(),
                op: op.clone(),
            })
            .unwrap();
            assert_eq!(v["type"], "cmd");
            assert_eq!(v["op"]["type"], name);
            round_trip_hub(HubMsg::Cmd {
                cmd_id: "c".into(),
                op,
            });
        }
    }

    #[test]
    fn golden_frames() {
        let ev = RunnerMsg::Event {
            thread: "t1".into(),
            rseq: 2,
            turn: Some(1),
            body: EventBody::AgentText(AgentText {
                text: "hi".into(),
                ..Default::default()
            }),
            raw: None,
            run: None,
            at_ms: None,
        };
        let v = serde_json::to_value(&ev).unwrap();
        assert_eq!(v["type"], "event");
        assert_eq!(v["rseq"], 2);
        assert_eq!(v["body"]["kind"], "agent.text");
        assert_eq!(v["body"]["data"]["text"], "hi");
        assert!(v.get("raw").is_none());

        // The hello of an older runner (with its token and inventory) still decodes, so the
        // hub can tell it which version to update.
        let hello: RunnerMsg = serde_json::from_value(json!({
            "type": "hello", "box": "b", "token": "t", "version": "1", "protocol": 5,
            "running": [], "acks_needed": {"t1": 3}
        }))
        .unwrap();
        let RunnerMsg::Hello(h) = hello else {
            panic!("not a hello")
        };
        assert_eq!(h.box_name, "b");
        assert_eq!(h.protocol, 5);
        assert!(h.nonce.is_empty());
        let auth = serde_json::to_value(RunnerMsg::Auth { proof: "p".into() }).unwrap();
        assert_eq!(auth, json!({"type": "auth", "proof": "p"}));

        // An older runner's inventory has no build, and one without a build sends none (an
        // older hub ignores it either way: the inventory takes unknown fields).
        let inv: RunnerMsg =
            serde_json::from_value(json!({"type": "inventory", "version": "1"})).unwrap();
        let RunnerMsg::Inventory(inv) = inv else {
            panic!("not an inventory")
        };
        assert_eq!(inv.build, None);
        let v = serde_json::to_value(RunnerMsg::Inventory(inv)).unwrap();
        assert!(v.get("build").is_none(), "{v}");
        let inv: RunnerMsg = serde_json::from_value(
            json!({"type": "inventory", "version": "1", "build": "0123456789abcdef", "new": 1}),
        )
        .unwrap();
        let RunnerMsg::Inventory(inv) = inv else {
            panic!("not an inventory")
        };
        assert_eq!(inv.build.as_deref(), Some("0123456789abcdef"));
        // An inventory without agents (an older runner, or one that has not probed yet) has
        // none, and sends none.
        assert!(inv.agents.is_empty());
        let v = serde_json::to_value(RunnerMsg::Inventory(inv)).unwrap();
        assert!(v.get("agents").is_none(), "{v}");
        let ok = serde_json::to_value(HubMsg::Authenticated).unwrap();
        assert_eq!(ok, json!({"type": "authenticated"}));

        let ack: HubMsg = serde_json::from_str(r#"{"type":"ack","thread":"t","rseq":3}"#).unwrap();
        assert_eq!(
            ack,
            HubMsg::Ack {
                thread: "t".into(),
                rseq: 3
            }
        );
        let r = serde_json::to_value(RunnerMsg::CmdResult(CmdResult::new("c", Ok(Value::Null))))
            .unwrap();
        assert_eq!(
            r,
            json!({"type": "cmd_result", "cmd_id": "c", "ok": true, "result": null})
        );
    }

    /// The agents op and what it answers, on the wire: `refresh` defaults to false, a login
    /// state this build does not know reads as unknown, and what is not known is not sent.
    #[test]
    fn agents_on_the_wire() {
        let op: Op = serde_json::from_value(json!({"type": "agents"})).unwrap();
        assert_eq!(op, Op::Agents { refresh: false });
        assert_eq!(op.name(), "agents");
        assert!(!op.is_session_op() && !op.changes_repository() && op.thread().is_none());
        let info = AgentInfo {
            runtime: "codex".into(),
            found: true,
            path: Some("/usr/bin/codex".into()),
            login: AgentLogin::LoggedOut,
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({"runtime": "codex", "found": true, "path": "/usr/bin/codex", "login": "logged_out"})
        );
        let newer: AgentInfo = serde_json::from_value(
            json!({"runtime": "grok-build", "found": false, "login": "expired", "new": 1}),
        )
        .unwrap();
        assert_eq!(newer.login, AgentLogin::Unknown);
        assert!(!newer.found);
        let report: AgentsReport = serde_json::from_value(json!({
            "login_path": {"shell": "/bin/csh", "error": "csh is not asked"},
            "agents": [{"runtime": "claude-code", "login": "logged_in"}]
        }))
        .unwrap();
        assert_eq!(report.login_path.entries, None);
        assert_eq!(report.agents[0].login, AgentLogin::LoggedIn);
    }

    #[test]
    fn secrets_stay_out_of_debug_output() {
        let s = Secret::new("hunter2");
        assert_eq!(format!("{s:?}"), "Secret(<redacted>)");
        let o = SecretHolder { token: s.clone() };
        assert!(!format!("{o:?}").contains("hunter2"));
        assert_eq!(serde_json::to_value(&s).unwrap(), "hunter2");
        assert!(s.ct_eq("hunter2"));
        assert!(!s.ct_eq("hunter3"));
        assert!(!s.ct_eq("hunter"));
        assert!(!s.ct_eq("hunter22"));
        assert!(Secret::default().ct_eq(""));
    }

    #[derive(Debug)]
    struct SecretHolder {
        #[allow(dead_code)]
        token: Secret,
    }

    /// The handshake proofs: each side's proof checks only as that side's, for that box and
    /// those nonces, with that token; nothing else (another token, a swapped nonce, the other
    /// side's label, a cut or non-hex proof, an empty token) passes. Pinned to an HMAC-SHA256
    /// computed independently, so both sides of any build agree on the bytes.
    #[test]
    fn handshake_proofs_bind_side_box_nonces_and_token() {
        let token = Secret::new("0123456789abcdef0123456789abcdef");
        let (rn, hn) = (new_nonce().unwrap(), new_nonce().unwrap());
        assert!(valid_nonce(&rn) && valid_nonce(&hn));
        assert_ne!(rn, hn);
        let hub = auth_proof(&token, Prover::Hub, "gpu", &rn, &hn);
        let runner = auth_proof(&token, Prover::Runner, "gpu", &rn, &hn);
        assert_eq!(hub.len(), 64);
        assert_ne!(hub, runner);
        assert!(!hub.contains(token.expose()));
        let ok = |by, b: &str, r: &str, h: &str, p: &str| check_auth_proof(&token, by, b, r, h, p);
        assert!(ok(Prover::Hub, "gpu", &rn, &hn, &hub));
        assert!(ok(Prover::Runner, "gpu", &rn, &hn, &runner));
        assert!(!ok(Prover::Runner, "gpu", &rn, &hn, &hub));
        assert!(!ok(Prover::Hub, "gpu", &rn, &hn, &runner));
        assert!(!ok(Prover::Hub, "gpu2", &rn, &hn, &hub));
        assert!(!ok(Prover::Hub, "gpu", &hn, &rn, &hub));
        assert!(!ok(Prover::Hub, "gpu", &rn, &rn, &hub));
        assert!(!ok(Prover::Hub, "gpu", &rn, &hn, &hub[..62]));
        assert!(!ok(
            Prover::Hub,
            "gpu",
            &rn,
            &hn,
            // Uppercase hex in place of the first digit: a case-only variant when that digit
            // is `a`, another proof otherwise; refused either way, whatever the random proof.
            &format!("A{}", &hub[1..])
        ));
        assert!(!ok(Prover::Hub, "gpu", &rn, &hn, ""));
        let other = Secret::new("0123456789abcdef0123456789abcdeX");
        assert!(!check_auth_proof(
            &other,
            Prover::Hub,
            "gpu",
            &rn,
            &hn,
            &hub
        ));
        let empty = Secret::default();
        let p = auth_proof(&empty, Prover::Hub, "gpu", &rn, &hn);
        assert!(!check_auth_proof(&empty, Prover::Hub, "gpu", &rn, &hn, &p));
        // Lengths are part of what is signed: moving a byte between fields changes it.
        assert_ne!(
            auth_proof(&token, Prover::Hub, "ab", "c", "d"),
            auth_proof(&token, Prover::Hub, "a", "bc", "d")
        );
        // HMAC-SHA256(key, "twr hub" | len "b" | len "r" | len "h"), from Python's hmac module.
        assert_eq!(
            auth_proof(&Secret::new("k"), Prover::Hub, "b", "r", "h"),
            "3c5da0337b61ca60ddf4feb6bb294479b16c2308094cf65466ddf9d5fc0fdbe0"
        );
        for bad in [
            "",
            "ab",
            &"A".repeat(64),
            &"g".repeat(64),
            &"a".repeat(63),
            &"a".repeat(65),
        ] {
            assert!(!valid_nonce(bad), "{bad}");
        }
    }

    #[test]
    fn box_names_are_checked() {
        for ok in ["gpu", "gpu-2", "a.b_c", &"x".repeat(64)] {
            validate_box_name(ok).unwrap();
        }
        for bad in [
            "",
            ".hidden",
            "-gpu",
            "--help",
            "a/b",
            "a b",
            "ü",
            &"x".repeat(65),
            "a\0b",
        ] {
            let e = validate_box_name(bad).unwrap_err().to_string();
            assert!(e.starts_with("invalid box name"), "{bad:?}: {e}");
        }
    }

    #[test]
    fn tokens_are_random_hex() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_eq!(check_token_len(&a, "t"), Ok(None));
    }

    #[test]
    fn token_lengths_are_checked() {
        let e = check_token_len("short-tok-12345", "boxes.gpu.token").unwrap_err();
        assert!(
            e.starts_with("boxes.gpu.token is too short (15 characters"),
            "{e}"
        );
        assert!(
            e.contains("twr runner token") && !e.contains("short-tok"),
            "{e}"
        );
        assert!(check_token_len("", "t").is_err());
        let w = check_token_len(&"x".repeat(16), "the runner token")
            .unwrap()
            .unwrap();
        assert!(
            w.starts_with("the runner token is short (16 characters"),
            "{w}"
        );
        assert!(check_token_len(&"x".repeat(31), "t").unwrap().is_some());
        assert_eq!(check_token_len(&"x".repeat(32), "t"), Ok(None));
        // Characters, not bytes.
        assert!(check_token_len(&"é".repeat(15), "t").is_err());
    }

    #[test]
    fn wire_conversions_are_lossless() {
        let sc: SpawnConfig = cfg().into();
        assert_eq!(WireSpawnConfig::from(sc.clone()), cfg());
        assert_eq!(SpawnConfig::from(WireSpawnConfig::from(sc.clone())), sc);
        for o in [
            RemoveOutcome::Removed,
            RemoveOutcome::KeptDirty { dirty_files: 3 },
            RemoveOutcome::Missing,
            RemoveOutcome::KeptCheckout {
                head: "detached at abc1234".into(),
            },
            RemoveOutcome::Kept {
                reason: "ignored files not known to be disposable: .env".into(),
            },
        ] {
            let w = WireRemoveOutcome::from(o.clone());
            let v = serde_json::to_value(w).unwrap();
            let back: WireRemoveOutcome = serde_json::from_value(v).unwrap();
            assert_eq!(RemoveOutcome::from(back), o);
        }
        assert_eq!(
            serde_json::to_value(WireRemoveOutcome::KeptDirty { dirty_files: 2 }).unwrap(),
            json!({"outcome": "kept_dirty", "dirty_files": 2})
        );
        assert_eq!(
            serde_json::to_value(WireRemoveOutcome::Kept { reason: "r".into() }).unwrap(),
            json!({"outcome": "kept", "reason": "r"})
        );
        let t = crate::git::TrashOutcome {
            item: Some("01J00000000000000000000000".into()),
            worktree_moved: true,
            refs: vec!["refs/twr-trash/01J00000000000000000000000/branch".into()],
            bytes: 7,
            ..Default::default()
        };
        let back: crate::git::TrashOutcome = decode_result(json!(t)).unwrap();
        assert_eq!(back, t);
        let o = vec![
            crate::git::OnlyCommits {
                branch: Some(2),
                head: Some(1),
                commits: crate::git::Listed::new(["c1".to_string(), "c2".into(), "c3".into()]),
                shared: vec![crate::git::SharedCommits {
                    with: 1,
                    commits: 1,
                }],
                checkout: Some("twr/x-2".into()),
                checkout_only: Some(4),
                unchecked: None,
                common: Some("/r/.git".into()),
            },
            crate::git::OnlyCommits {
                unchecked: Some("the reflog x: line 1 is not a reflog entry".into()),
                ..Default::default()
            },
        ];
        let v = serde_json::to_value(&o).unwrap();
        assert_eq!(
            v,
            json!([{"branch": 2, "head": 1, "commits": {"items": ["c1", "c2", "c3"], "total": 3},
                    "shared": [{"with": 1, "commits": 1}], "checkout": "twr/x-2",
                    "checkout_only": 4, "common": "/r/.git"},
                   {"branch": null, "head": null, "commits": {"items": [], "total": 0},
                    "shared": [], "unchecked": "the reflog x: line 1 is not a reflog entry"}])
        );
        assert_eq!(decode_result::<Vec<crate::git::OnlyCommits>>(v).unwrap(), o);
        let op: Op = serde_json::from_value(
            json!({"type": "branch_only_commits", "targets": [{"repo": "/r", "branch": "twr/x"}]}),
        )
        .unwrap();
        assert_eq!(
            op,
            Op::BranchOnlyCommits {
                targets: vec![DeleteRefs {
                    repo: "/r".into(),
                    branch: Some("twr/x".into()),
                    worktree: None,
                    // An older hub sends none.
                    other_branches: None,
                }],
            }
        );
        let inv: crate::git::WorktreeInventory = decode_result(json!({
            "dirty": {"items": ["a"], "total": 1},
            "ignored_groups": [{"path": "target/", "files": 3, "more": true}],
        }))
        .unwrap();
        assert_eq!(inv.dirty.total, 1);
        assert!(inv.ignored_groups[0].more && inv.hidden.is_empty() && !inv.gone);
        let mut c = Command::new("claude");
        c.args(["--resume", "s"])
            .current_dir("/w")
            .env("A", "1")
            .env_remove("B");
        let info = AttachInfo::from_command(&c);
        assert_eq!(info.program, "claude");
        assert_eq!(info.args, vec!["--resume", "s"]);
        assert_eq!(info.cwd.as_deref(), Some(std::path::Path::new("/w")));
        let back = AttachInfo::from_command(&info.to_command());
        assert_eq!(back, info);
        let r: GitSnapshot = decode_result(json!({"branch": "x", "dirty_files": 1})).unwrap();
        assert_eq!(r.dirty_files, 1);
        assert!(decode_result::<bool>(json!("nope")).is_err());
    }

    #[test]
    fn durable_command_ids_and_their_retention() {
        for id in ["send:t1:3", "respond:01J", "stop:t1:run"] {
            assert!(is_durable_cmd_id(id), "{id}");
        }
        for id in ["01JABC", "sender:x", "git:snap", ""] {
            assert!(!is_durable_cmd_id(id), "{id}");
        }
    }
}
