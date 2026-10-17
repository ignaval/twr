//! Runner connections (the hub side): authentication, fencing, the ingest path
//! and the reconciliation of sessions. The wire protocol is [`twr_core::runner`]; the boxes
//! are [`RemoteBox`]es; the listener that accepts connections is [`crate::listener`].
//!
//! **Handshake** ([`twr_core::runner`]). The first message must be `hello`, with
//! [`PROTOCOL_VERSION`] (else `error{protocol}`, before any authentication) and a well-formed
//! nonce. The hub answers `challenge{nonce, proof}` with a fresh nonce and its proof over
//! the box's token ([`RemoteBox::hub_proof`]; a random one for a box it does not know or that
//! has no token, so a peer cannot tell which boxes exist), then expects `auth{proof}`: the
//! runner's proof ([`RemoteBox::checks_runner_proof`], constant time), else
//! `error{unauthorized}`. All of that within one deadline from the connection's accept
//! ([`HELLO_TIMEOUT`], see [`crate::listener`]). Once it is good the connection gets the full
//! message size, the hub says `authenticated` and waits (as long again) for the runner's
//! `inventory`, which may be large. The token never crosses the connection and never appears
//! in logs or events. An accepted connection fences the box's
//! previous one ([`RemoteBox::connect`]: told `fenced`, closed, its frames no longer
//! committed) and gets a new epoch. The `welcome{epoch, watermarks}` carries only durable
//! watermarks: under the lock that fences the previous connection, the store is synced
//! ([`crate::store::Store::sync`], covering that connection's open ack batch) before they are
//! read; a sync that fails refuses the connection. Before the welcome goes out, the hub
//! takes over the sessions the inventory reports that its log shows as live but that it has no
//! runtime for (after a hub restart), unless that runtime's exit is being recorded: their
//! frames are then recorded like any other. An exit closes the runtime of the log its session
//! was started or taken over for, once: an exit of a session whose runtime the log has closed
//! meanwhile records nothing but its frame (`Core::exit_plan`).
//!
//! **Sealed connections** ([`Hub::serve_sealed_runner`]): an SSH box's runner is reached by
//! the hub's own ssh ([`crate::ssh_box`]), inside a channel the hub's key opened
//! ([`twr_core::sealed`]); such a connection never comes from the listener and is bound to
//! its box. Its hello must name that box (else `error{unauthorized}`), the challenge carries a
//! random proof, and the runner's `auth.proof` must be [`SEALED_AUTH_PROOF`] (else
//! `error{unauthorized}`): the channel proved the hub, and the hub's ssh reached the box. It
//! is accepted with the listener's small pre-authentication limits and raised to the full
//! message size once authenticated, as a listener's is; the listener's limits on connections
//! still authenticating do not apply (the hub opened it). From the inventory on it is served as
//! any authenticated connection. On the listener an SSH box (which has no token) is a box the
//! hub does not know: a random proof, then `unauthorized`.
//!
//! **Ingest.** Frames are handled one at a time, in arrival order. A frame with `rseq` 0 closes
//! the connection (`rseq` starts at 1). A frame that
//! names a thread of another box (the local box included) is refused, whatever its `rseq`
//! (a duplicate of one too): only its watermark is committed when it is new (so the runner's
//! spool drains), it is counted ([`RemoteBox::rejected_frames`]) and the runner is told to
//! stop that session; the thread is never touched. A runner's
//! attention ids are recorded in their thread's namespace, `<thread>:<id>`, like a local
//! adapter's ([`twr_core::adapter::hub_attn_id`]; the thread is the box's). Every commit
//! happens under the core lock while the frame's connection is still the box's current one
//! ([`Fence`]), checked again on each retry: a connection fenced meanwhile commits nothing
//! more (its frames are replayed on the new one). The same holds for everything else a
//! connection changes: a recorded command's result (the command retired, a failed one's turn
//! ended; a newer connection may have sent it again under the same id), the reconciliation
//! (commands retired or sent again), and the stop of a stray (sent only on the connection that
//! saw it). For each `event` / `exited` frame of a
//! thread, with `w` the committed watermark of (box, thread):
//! `rseq <= w` is a duplicate (acked with `w`, nothing appended; a duplicate `turn.ended` of
//! a turn whose end the log has from the box's runtime gets the reactions it may be missing,
//! `Core::missed_turn_end`, never one the hub ended itself); `rseq == w + 1` is appended
//! (through the same checks as a local adapter's events: `Core::record_adapter_event`) and
//! `w` raised to `rseq` in the SAME transaction, then synced and acked; `rseq > w + 1` is a gap:
//! logged as an error and the connection is closed (the runner replays from its spool). An
//! `exited` frame runs the closing sequence (open turn interrupted, adapter items resolved,
//! `thread.stopped` / `thread.crashed` / `thread.attached` / `thread.parked`) in the frame's
//! transaction. The hub names the run of every session it starts (`spawn` / `resume` `run`); a
//! session it takes over learns it from the inventory (or, taken over for frames still to replay,
//! from its first frame). Once the run is known, frames and exits of another run, and exits
//! naming none, are not that session's (a restarted runner reporting an earlier run's end
//! again, or a stray's exit arriving after a new session started, closes nothing); until the
//! runner has confirmed a run the hub named (a frame of it), an exit naming none may still be
//! the session's (a restarted runner's, for the run its predecessor left behind). A frame for a
//! thread the hub runs no session for (a stray) is committed as a watermark only; after the
//! replay the stray session is stopped. Events reach the log only
//! here: the session's `AdapterHandle` carries commands out and never events in
//! ([`crate::remote`]), so nothing is appended twice. A frame whose commit keeps failing
//! closes the connection unacked (the runner sends it again), and so does any frame once the
//! hub is shutting down (it has let go of the box's sessions, which keep running: the next hub
//! takes them over and gets the frames then).
//!
//! **Ack batches.** Consecutive frames of a connection are committed one by one (each frame's
//! events with its watermark, fence checked at each commit) without a sync each; the batch
//! ends after [`ACK_BATCH_FRAMES`] frames, [`ACK_BATCH_DELAY`] after its first frame, before
//! any other message and before a turn end's reactions: one sync to disk
//! ([`crate::store::Store::sync`]) covers the batch, then one cumulative ack per thread goes
//! out. A crash or power loss before the sync can only lose unacked frames, which the runner
//! still has (the log keeps a prefix of its commits, each frame's events with its watermark).
//! A frame's `at_ms` (the runner's capture time) is its events' time (`Batch::at`).
//!
//! **Reconciliation** runs once per connection at `synced` (the spool replayed), against the
//! sessions the inventory reported as running, minus the runs whose `exited` frame the replay
//! brought (the inventory as of the end of the replay):
//! - the log shows a live runtime the runner does not run: closing sequence, `thread.crashed`
//!   ("the runner lost the process", e.g. it restarted; `thread.stopped` when a `thread.stop`
//!   asked for it) — unless the hub is still waiting for that session's `spawn` / `resume`
//!   (sent again once the box is ready). A crash gets its reactions (block_kind, error item,
//!   the automatic retry when due) but nothing from the queue;
//! - the runner runs a session the hub does not (the log says stopped, parked, attached or
//!   crashed, or an unknown thread): the runner is told to stop that run (the stop names it,
//!   so it never stops a later run of the thread);
//! - the runner runs another run of the thread than the hub's session (both runs known): the
//!   hub's session is lost (closed as above) and the runner's run is a stray (stopped by its
//!   run). A `send` names the run of the hub's session too, so no prompt reaches another run.
//!
//! Then the box is *online*: first the stops of those strays go out, then the session commands
//! that wait for the box (same ids), including the recorded commands (`remote_outbox`) a
//! previous hub may not have delivered — a `send` of a live session whose turn is still the one
//! it was issued in, a `respond` of the live runtime generation it answers (whatever its turn:
//! a thread takes no new turn while an answer to it is unacknowledged, [`Core::thread_free`]);
//! the others no longer apply and are forgotten. Then `runner.connected
//! {box, version}` is appended on the `hub` thread, a switch of the box to another build that
//! was under way is over (`hub::box_switch`), and the box's threads get their queued
//! prompts. Until then (and while it is offline) their prompts wait and box commands answer
//! `box_offline`. When the current connection ends the box is offline again and
//! `runner.disconnected{box, version}` is appended (the connection's end is checked and
//! recorded in one step under the core lock, so the end of a replaced connection never takes
//! its replacement offline); its threads keep their state and their runtimes keep running on
//! the box.
//!
//! **Failed and expired commands.** A recorded command (`remote_outbox`) the runner answers
//! with an error, or that expires unsent ([`store::OUTBOX_RETENTION`]), is never just dropped
//! ([`Core::failed_remote_cmd`], [`Core::expired_remote_cmd`]): a `send` or `respond` is
//! retired in one batch with the `turn.ended{error}` of its open turn (which raises the
//! turn's `error` item) and, when its runtime may be left waiting (an answer it never got, a
//! prompt it may be working on), a durable stop of that runtime (`stop_requests` + its `stop`
//! command); the thread's queue is held until the user acts, and nothing is dispatched to a
//! runtime being stopped. A `stop` stays recorded and goes again while its runtime lives.
//!
//! **Epochs** are in memory only (see [`crate::remote`]).
//!
//! Keepalive: the runner pings every 20 s (a protocol `ping` message); a connection that sends
//! no protocol message for [`IDLE_TIMEOUT`] is closed. WebSocket ping frames do not count: they
//! cannot keep a connection open. The hub answers `ping` with `pong`.
//!
//! Backpressure: what the hub sends goes through a bounded queue
//! ([`crate::remote::SEND_QUEUE`]); a full queue, or a message the runner does not take
//! within [`WRITE_TIMEOUT`], closes the connection, and so does the writer ending for any
//! reason. A runner that sends without reading (a flood of protocol pings or of WebSocket
//! pings) cannot grow the hub's memory: the WebSocket write buffer is bounded too
//! ([`crate::listener::runner_ws_config`]).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use twr_core::adapter::AdapterCmd;
use twr_core::config::BoxTransport;
use twr_core::event::*;
use twr_core::runner::{
    cmd_errors, hub_errors, new_nonce, valid_nonce, CmdError, Hello, HubMsg, Inventory, Op,
    RunnerMsg, PROTOCOL_VERSION, SEALED_AUTH_PROOF,
};

use super::supervisor::{close_exited, give_up_runtime, persist, supervise};
use super::{box_switch, git, Batch, Core, Exit, Hub, HubState, Inner, Live, PendingExit};
use crate::remote::{self, ConnTx, RemoteBox, SEND_QUEUE};
use crate::store::{self, RunnerAck};

/// How long a new connection has to authenticate, counted from its accept (its TLS and
/// WebSocket handshakes, hello and proof together), and then to send its inventory.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(15);

/// A connection that sends no protocol message for this long is closed (the runner pings
/// every 20 s; WebSocket pings do not count).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// How long the writer gets to flush after the connection ends.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(2);

/// How long one message may take to be written to the runner before the connection closes.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Most frames of one connection committed before their batch is synced and acked.
pub const ACK_BATCH_FRAMES: usize = 256;

/// Longest a committed frame waits for its batch to be synced and acked.
pub const ACK_BATCH_DELAY: Duration = Duration::from_millis(50);

/// A runner connection's right to commit: it holds while the connection is the box's current
/// one. Checked under the core lock (where a newer connection replaces it, see [`serve`]),
/// right before each commit.
#[derive(Clone)]
pub(super) struct Fence {
    rbox: RemoteBox,
    epoch: u64,
}

impl Fence {
    /// Whether the connection is still the box's current one.
    pub(super) fn holds(&self) -> bool {
        self.rbox.is_current(self.epoch)
    }
}

/// What the ingest path did with one `event` frame.
enum Ingested {
    /// The hub is shutting down (it has let go of the box's sessions): nothing recorded, the
    /// connection closes, the runner keeps the frame for the next hub.
    Stopping,
    /// A newer connection of the box replaced this one: nothing recorded.
    Fenced,
    /// The thread is on another box: only the watermark moved (when the frame was new); acked
    /// with this.
    Foreign(u64),
    /// At or below the watermark: nothing appended. With the turn a duplicate `turn.ended`
    /// ended when the hub's reactions to it are missing ([`Core::missed_turn_end`]).
    Duplicate(u64, Option<super::EndedTurn>),
    /// Above the watermark + 1: the connection closes.
    Gap(u64),
    /// No session of the hub: only the watermark moved.
    Stray,
    /// Appended (runtime `generation`), with the turn it ended, if any.
    Recorded {
        generation: u64,
        ended: Option<super::EndedTurn>,
    },
}

/// What the ingest path did with one `exited` frame (before its closing sequence runs).
enum Exited {
    Stopping,
    Fenced,
    /// The thread is on another box: only the watermark moved (when the frame was new); acked
    /// with this.
    Foreign(u64),
    Duplicate(u64),
    Gap(u64),
    /// No session of the hub: only the watermark moved.
    Stray,
    /// The hub's session ended: its closing sequence (with the frame's ack) is to be recorded.
    Close(Box<PendingExit>),
}

/// One runner connection.
struct Conn {
    rbox: RemoteBox,
    epoch: u64,
    tx: ConnTx,
    version: String,
    /// The build the inventory reported ([`Inventory::build`]).
    build: Option<String>,
    /// Sessions the inventory reported as running (thread -> run), minus those whose
    /// `exited` frame the replay brought.
    running: HashMap<String, Option<String>>,
    /// `synced` arrived (and the reconciliation ran).
    synced: bool,
    /// `runner.connected` was appended for this connection.
    announced: bool,
    /// Strays (thread, run) already told to stop on this connection.
    stopped: HashSet<(String, Option<String>)>,
    /// The open ack batch: the highest rseq per thread to ack once the batch is synced
    /// ([`flush_acks`]).
    unacked: BTreeMap<String, u64>,
    /// Frames in the open batch.
    batched: usize,
    /// When the open batch began (its first frame was handled).
    batch_since: Option<tokio::time::Instant>,
    /// Longest a batch stays open ([`HubOptions::ack_batch_delay`]).
    ///
    /// [`HubOptions::ack_batch_delay`]: crate::HubOptions::ack_batch_delay
    batch_delay: Duration,
}

impl Conn {
    fn box_name(&self) -> &str {
        self.rbox.name()
    }

    fn fence(&self) -> Fence {
        Fence {
            rbox: self.rbox.clone(),
            epoch: self.epoch,
        }
    }

    fn send(&self, m: HubMsg) {
        self.tx.send(m);
    }

    /// Acks every frame of `thread` up to `rseq` once the open batch is synced
    /// ([`flush_acks`]; acks are cumulative).
    fn ack(&mut self, thread: &str, rseq: u64) {
        let w = self.unacked.entry(thread.to_string()).or_insert(rseq);
        *w = (*w).max(rseq);
    }

    /// A frame joined the open batch.
    fn batched(&mut self) {
        self.batched += 1;
        self.batch_since
            .get_or_insert_with(tokio::time::Instant::now);
    }

    /// When the open batch must be flushed (none open: `None`).
    fn flush_at(&self) -> Option<tokio::time::Instant> {
        self.batch_since.map(|t| t + self.batch_delay)
    }

    /// Stops run `run` of a stray session once per connection (only once the box is ready:
    /// before that, the reconciliation stops what is left).
    fn stop_stray(&mut self, thread: &str, run: Option<String>) {
        if self.synced && self.stopped.insert((thread.to_string(), run.clone())) {
            self.rbox.stop_stray(self.epoch, thread, run);
        }
    }

    /// The replay brought the `exited` frame of run `run` of `thread`: the runner no longer
    /// runs it (the reconciliation must not take it for a stray).
    fn exited(&mut self, thread: &str, run: Option<&str>) {
        if self.synced {
            return;
        }
        if let (Some(run), Some(Some(r))) = (run, self.running.get(thread)) {
            if r == run {
                self.running.remove(thread);
            }
        }
    }
}

impl Hub {
    /// Serves one runner connection (an accepted WebSocket) until it ends (see the module
    /// docs). `peer` is for logs.
    pub async fn serve_runner<S>(&self, ws: WebSocketStream<S>, peer: String)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        serve(
            Arc::clone(&self.inner),
            ws,
            peer,
            Admission::Listener(None),
            None,
        )
        .await;
    }

    /// [`Hub::serve_runner`] for a connection accepted with the small pre-authentication
    /// limits ([`crate::listener`]): `permit` (its place among the connections still
    /// authenticating, and its deadline) is released once the connection has authenticated,
    /// and the connection then gets the full message size
    /// ([`crate::listener::runner_ws_config`]).
    pub async fn serve_runner_gated<S>(
        &self,
        ws: WebSocketStream<S>,
        peer: String,
        permit: Option<crate::listener::PreAuth>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        serve(
            Arc::clone(&self.inner),
            ws,
            peer,
            Admission::Listener(permit),
            None,
        )
        .await;
    }

    /// Serves a connection to SSH box `box_name` that the hub's own ssh opened
    /// ([`crate::ssh_box`]): `ws` runs inside the sealed channel to the box's daemon
    /// ([`twr_core::sealed::hub_side`]) and was accepted with the small pre-authentication
    /// limits ([`crate::listener::pre_auth_ws_config`]), raised once it authenticates. Never
    /// reachable from the listener. Its handshake is bound to the box: a hello naming another
    /// box, or an `auth` with a proof, is `unauthorized`; the challenge carries a random
    /// proof. Past that it is served as an authenticated token connection is (inventory,
    /// welcome, fencing, reconciliation). Returns how it went.
    pub async fn serve_sealed_runner<S>(&self, ws: WebSocketStream<S>, box_name: &str) -> SealedEnd
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        serve(
            Arc::clone(&self.inner),
            ws,
            format!("ssh:{box_name}"),
            Admission::Sealed(box_name.to_string()),
            None,
        )
        .await
    }

    /// How long a new runner connection has to authenticate ([`HubOptions::hello_timeout`]).
    ///
    /// [`HubOptions::hello_timeout`]: crate::HubOptions::hello_timeout
    pub(crate) fn runner_hello_timeout(&self) -> Duration {
        self.inner.hello_timeout
    }

    /// Frames box `name`'s runners sent for threads of other boxes (refused, see
    /// [`crate::hub`] runners).
    pub fn box_rejected_frames(&self, name: &str) -> u64 {
        self.inner.remote(name).map_or(0, |b| b.rejected_frames())
    }

    /// The latest refused connection naming box `name` since one of its connections last
    /// authenticated ([`RemoteBox::note_refusal`]).
    pub fn box_refusal(&self, name: &str) -> Option<crate::remote::Refusal> {
        self.inner.remote(name).and_then(|b| b.refusal())
    }

    /// The remote boxes: `(name, online, runner version)`.
    pub fn boxes(&self) -> Vec<(String, bool, Option<String>)> {
        let remotes = self.inner.remotes_list();
        let core = self.inner.core();
        let mut v: Vec<_> = remotes
            .iter()
            .map(|b| {
                (
                    b.name().to_string(),
                    core.online_boxes.contains(b.name()),
                    b.info().map(|i| i.version),
                )
            })
            .collect();
        v.sort();
        v
    }
}

/// Ends a connection that has not authenticated: its pre-authentication slot is released
/// first, then `refusal` (if any) and the close get [`FLUSH_TIMEOUT`] in all before the
/// connection is dropped. A peer that does not read (its buffers full of pongs, say) can hold
/// neither the connection nor a slot.
async fn refuse_unauthenticated<S>(
    mut ws: WebSocketStream<S>,
    permit: Option<crate::listener::PreAuth>,
    refusal: Option<HubMsg>,
) where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    drop(permit);
    let _ = tokio::time::timeout(FLUSH_TIMEOUT, async {
        if let Some(m) = refusal {
            if flushed_send(&mut ws, text(&m)).await.is_err() {
                return;
            }
        }
        let _ = ws.close(None).await;
    })
    .await;
}

fn text(m: &HubMsg) -> Message {
    Message::Text(serde_json::to_string(m).unwrap_or_default().into())
}

/// What the next message of a connection that is not welcomed yet is.
// Short-lived (a few per connection), and matched by value: not boxed.
#[allow(clippy::large_enum_variant)]
enum Next {
    Msg(RunnerMsg),
    Malformed(serde_json::Error),
    /// Closed, or a read error.
    Closed,
    TimedOut,
}

/// The next text message of `ws` (other frames skipped) by `deadline`.
async fn next_message<S>(ws: &mut WebSocketStream<S>, deadline: tokio::time::Instant) -> Next
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let next = tokio::time::timeout_at(deadline, async {
        loop {
            match ws.next().await {
                Some(Ok(Message::Text(t))) => {
                    return match serde_json::from_str::<RunnerMsg>(t.as_str()) {
                        Ok(m) => Next::Msg(m),
                        Err(e) => Next::Malformed(e),
                    }
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return Next::Closed,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await;
    next.unwrap_or(Next::TimedOut)
}

/// Sends `m` on a connection that is not welcomed yet, by `deadline`: whether it went out.
async fn send_by<S>(ws: &mut WebSocketStream<S>, m: &HubMsg, deadline: tokio::time::Instant) -> bool
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    matches!(
        tokio::time::timeout_at(deadline, flushed_send(ws, text(m))).await,
        Ok(Ok(()))
    )
}

/// Sends `m` once what is buffered for the peer (pongs to its pings, say) has gone out: the
/// write buffer is bounded ([`crate::listener::runner_ws_config`]), and tungstenite refuses a
/// message that does not fit next to what it holds instead of waiting for room.
async fn flushed_send<W>(w: &mut W, m: Message) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    W: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    w.flush().await?;
    w.send(m).await
}

/// How a runner connection reached the hub.
pub(super) enum Admission {
    /// Through the listener ([`crate::listener`]), with its place among the connections still
    /// authenticating (`None`: a caller that applies no such limits, tests).
    Listener(Option<crate::listener::PreAuth>),
    /// Through the hub's own ssh to SSH box `.0` ([`crate::ssh_box`]): bound to that box,
    /// sealed (`twr_core::sealed`), accepted with the small pre-authentication limits
    /// ([`crate::listener::pre_auth_ws_config`]) and raised once authenticated.
    Sealed(String),
}

/// Why a handshake ended without an authenticated connection.
pub(super) enum Refused {
    /// The runner speaks runner protocol `.0`, not the hub's.
    Protocol(u32),
    /// Anything else (refused, closed, timed out).
    Other,
}

/// How a sealed connection ([`Hub::serve_sealed_runner`]) went, for the ssh supervisor that
/// opened it ([`crate::ssh_box`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SealedEnd {
    /// The hub welcomed it (it authenticated and got an epoch): the box was reachable.
    pub welcomed: bool,
    /// Refused before authenticating because its runner speaks another protocol (this one).
    pub protocol_refused: Option<u32>,
    /// The build its inventory reported ([`Inventory::build`]), once welcomed.
    pub build: Option<String>,
}

/// The handshake of one connection (see the module docs): the box and the runner's inventory
/// once the connection is authenticated, with the connection (raised to the full message size
/// when it came with the small pre-authentication limits); why not once it was refused or
/// ended.
///
/// A listener's connection names any box: a token box proves its token both ways, while an
/// SSH box (which has none) is treated as one the hub does not know. A sealed one
/// ([`Admission::Sealed`]) is bound to its box: a hello for another box is `unauthorized`, the
/// challenge's proof is random (the channel proved the hub), and the runner's proof must be
/// [`SEALED_AUTH_PROOF`].
async fn handshake<S>(
    hub: &Arc<Inner>,
    mut ws: WebSocketStream<S>,
    peer: &str,
    admission: Admission,
) -> Result<(RemoteBox, Inventory, WebSocketStream<S>), Refused>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let refuse = |code: &str, message: String| HubMsg::Error {
        code: code.to_string(),
        message,
    };
    let (permit, bound) = match admission {
        Admission::Listener(p) => (p, None),
        Admission::Sealed(b) => (None, Some(b)),
    };
    // Small until authenticated: raised then (the listener's gated connections and the
    // sealed ones; a plain `serve_runner` caller chose its own limits).
    let small = permit.is_some() || bound.is_some();
    let deadline = permit.as_ref().map_or_else(
        || tokio::time::Instant::now() + hub.hello_timeout,
        crate::listener::PreAuth::deadline,
    );
    let hello: Hello = match next_message(&mut ws, deadline).await {
        Next::Msg(RunnerMsg::Hello(h)) => h,
        Next::Msg(_) => {
            let m = refuse(
                hub_errors::PROTOCOL,
                "the first message must be hello".into(),
            );
            refuse_unauthenticated(ws, permit, Some(m)).await;
            return Err(Refused::Other);
        }
        Next::Malformed(e) => {
            let m = refuse(hub_errors::PROTOCOL, format!("malformed hello: {e}"));
            refuse_unauthenticated(ws, permit, Some(m)).await;
            return Err(Refused::Other);
        }
        Next::Closed => return Err(Refused::Other),
        Next::TimedOut => {
            tracing::warn!(peer, "runner sent no hello in time");
            refuse_unauthenticated(ws, permit, None).await;
            return Err(Refused::Other);
        }
    };
    // Before anything else: a runner of another version need not speak this handshake.
    if hello.protocol != PROTOCOL_VERSION {
        tracing::warn!(peer, box_name = %hello.box_name, protocol = hello.protocol, "runner refused: protocol mismatch");
        let update = if hello.protocol < PROTOCOL_VERSION {
            format!("update twr on box `{}`", hello.box_name)
        } else {
            "update twr on the hub".to_string()
        };
        let named = hub
            .remote(&hello.box_name)
            .filter(|b| bound.as_deref().is_none_or(|n| n == b.name()));
        if let Some(b) = named {
            b.note_refusal(format!(
                "a runner speaking protocol {} was refused (the hub's is {PROTOCOL_VERSION}): \
                 {update}",
                hello.protocol
            ));
        }
        let m = refuse(
            hub_errors::PROTOCOL,
            format!(
                "the hub speaks runner protocol {PROTOCOL_VERSION}, this runner {}; {update} so \
                 both run the same twr version",
                hello.protocol
            ),
        );
        refuse_unauthenticated(ws, permit, Some(m)).await;
        return Err(Refused::Protocol(hello.protocol));
    }
    if !valid_nonce(&hello.nonce) {
        let m = refuse(
            hub_errors::PROTOCOL,
            "malformed hello: its nonce is not 64 lowercase hex digits".into(),
        );
        refuse_unauthenticated(ws, permit, Some(m)).await;
        return Err(Refused::Other);
    }
    if let Some(b) = bound.as_deref().filter(|b| *b != hello.box_name) {
        tracing::warn!(peer, box_name = %hello.box_name, bound = %b, "runner refused: a sealed connection named another box");
        let m = refuse(
            hub_errors::UNAUTHORIZED,
            format!("this connection is box {b}'s"),
        );
        refuse_unauthenticated(ws, permit, Some(m)).await;
        return Err(Refused::Other);
    }
    // A listener's connection naming an SSH box is one naming a box the hub does not know;
    // a sealed one is its box's.
    let rbox = hub.remote(&hello.box_name).filter(|b| match &bound {
        None => b.transport() == BoxTransport::Token,
        Some(_) => b.transport() == BoxTransport::Ssh,
    });
    let hub_nonce = new_nonce();
    let proof = match (&hub_nonce, &rbox, &bound) {
        (Ok(n), Some(b), None) => b.hub_proof(&hello.nonce, n).map(Ok),
        _ => None,
    }
    // A box the hub does not know (or without a token) gets a random proof: the runner
    // cannot tell it from a wrong token. A sealed connection's too: the channel proved the
    // hub, and the runner does not check it.
    .unwrap_or_else(new_nonce);
    let (hub_nonce, proof) = match (hub_nonce, proof) {
        (Ok(n), Ok(p)) => (n, p),
        (Err(e), _) | (_, Err(e)) => {
            tracing::error!(peer, error = %format!("{e:#}"), "runner refused: no random nonce for its challenge");
            refuse_unauthenticated(ws, permit, None).await;
            return Err(Refused::Other);
        }
    };
    let challenge = HubMsg::Challenge {
        nonce: hub_nonce.clone(),
        proof,
    };
    if !send_by(&mut ws, &challenge, deadline).await {
        tracing::debug!(peer, "a runner connection took no challenge");
        refuse_unauthenticated(ws, permit, None).await;
        return Err(Refused::Other);
    }
    let proof = match next_message(&mut ws, deadline).await {
        Next::Msg(RunnerMsg::Auth { proof }) => proof,
        Next::Msg(_) => {
            let m = refuse(
                hub_errors::PROTOCOL,
                "the answer to the challenge must be auth".into(),
            );
            refuse_unauthenticated(ws, permit, Some(m)).await;
            return Err(Refused::Other);
        }
        Next::Malformed(e) => {
            let m = refuse(hub_errors::PROTOCOL, format!("malformed auth: {e}"));
            refuse_unauthenticated(ws, permit, Some(m)).await;
            return Err(Refused::Other);
        }
        Next::Closed => {
            // A runner that did not accept the hub's proof leaves without a word.
            tracing::warn!(peer, box_name = %hello.box_name, "runner left before it authenticated (did it refuse the hub's proof? its token may differ from the hub's)");
            if let (Some(b), None) = (&rbox, &bound) {
                b.note_refusal(
                    "a runner left after the hub's challenge: it did not accept the hub's \
                     proof, so its token likely differs from this box's token",
                );
            }
            return Err(Refused::Other);
        }
        Next::TimedOut => {
            tracing::warn!(peer, "runner did not authenticate in time");
            refuse_unauthenticated(ws, permit, None).await;
            return Err(Refused::Other);
        }
    };
    let rbox = rbox.filter(|b| {
        let ok = match &bound {
            None => b.checks_runner_proof(&hello.nonce, &hub_nonce, &proof),
            Some(_) => proof == SEALED_AUTH_PROOF,
        };
        if !ok {
            b.note_refusal(match &bound {
                None => "a runner was refused: its proof does not match this box's token",
                Some(_) => "the box's runner was refused: it sent a proof on a sealed connection",
            });
        }
        ok
    });
    let Some(rbox) = rbox else {
        tracing::warn!(peer, box_name = %hello.box_name, "runner refused: unknown box or bad token");
        let m = refuse(hub_errors::UNAUTHORIZED, "unknown box or bad token".into());
        refuse_unauthenticated(ws, permit, Some(m)).await;
        return Err(Refused::Other);
    };
    if *hub.state.borrow() != HubState::Running {
        refuse_unauthenticated(ws, permit, None).await;
        return Err(Refused::Other);
    }
    // Authenticated: the connection no longer counts against the pre-authentication limits
    // and gets the full message size (the runner sends nothing more before `authenticated`,
    // so no message is buffered in the old wrapper).
    drop(permit);
    let mut ws = if small {
        WebSocketStream::from_raw_socket(
            ws.into_inner(),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            Some(crate::listener::runner_ws_config()),
        )
        .await
    } else {
        ws
    };
    let deadline = tokio::time::Instant::now() + hub.hello_timeout;
    if !send_by(&mut ws, &HubMsg::Authenticated, deadline).await {
        tracing::debug!(peer, box_name = %rbox.name(), "runner connection lost after it authenticated");
        return Err(Refused::Other);
    }
    let refused = match next_message(&mut ws, deadline).await {
        Next::Msg(RunnerMsg::Inventory(inv)) => {
            let (inv, odd) = checked_inventory(inv);
            for why in odd {
                tracing::warn!(peer, box_name = %rbox.name(), "{why}");
                if let Some(s) = hub.ssh_box(rbox.name()) {
                    s.remark(why);
                }
            }
            return Ok((rbox, inv, ws));
        }
        Next::Msg(_) => Some(refuse(
            hub_errors::PROTOCOL,
            "the message after authenticated must be inventory".into(),
        )),
        Next::Malformed(e) => Some(refuse(
            hub_errors::PROTOCOL,
            format!("malformed inventory: {e}"),
        )),
        Next::Closed => return Err(Refused::Other),
        Next::TimedOut => {
            tracing::warn!(peer, box_name = %rbox.name(), "runner sent no inventory in time");
            None
        }
    };
    refuse_unauthenticated(ws, None, refused).await;
    Err(Refused::Other)
}

/// Longest runner version an inventory may report ([`checked_inventory`]).
pub(super) const VERSION_MAX: usize = 64;

/// `inv` (a runner's word: shown in `hub.info`, matched against builds) with its `version`
/// printable ASCII of at most [`VERSION_MAX`] bytes (else `unreadable`) and its `build` a build
/// id (else none), and a sentence for each field it changed.
pub(super) fn checked_inventory(mut inv: Inventory) -> (Inventory, Vec<String>) {
    let mut odd = Vec::new();
    let printable = |s: &str| s.bytes().all(|b| (0x20..0x7f).contains(&b));
    if inv.version.is_empty() || inv.version.len() > VERSION_MAX || !printable(&inv.version) {
        odd.push(format!(
            "the runner reported a version twr does not show ({} bytes, not printable text of \
             at most {VERSION_MAX}): taken as unreadable",
            inv.version.len()
        ));
        inv.version = "unreadable".into();
    }
    if let Some(b) = inv.build.take() {
        if twr_core::valid_build_id(&b) {
            inv.build = Some(b);
        } else {
            odd.push(format!(
                "the runner reported a build that is no build id ({} bytes): taken as unknown",
                b.len()
            ));
        }
    }
    (inv, odd)
}

/// Serves one runner connection from its handshake to its end (see the module docs).
/// `welcomed` is told the inventory's build once the welcome went out.
pub(super) async fn serve<S>(
    hub: Arc<Inner>,
    ws: WebSocketStream<S>,
    peer: String,
    admission: Admission,
    welcomed: Option<oneshot::Sender<Option<String>>>,
) -> SealedEnd
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let refuse = |code: &str, message: String| HubMsg::Error {
        code: code.to_string(),
        message,
    };
    let (rbox, inv, ws) = match handshake(&hub, ws, &peer, admission).await {
        Ok(h) => h,
        Err(Refused::Protocol(p)) => {
            return SealedEnd {
                protocol_refused: Some(p),
                ..Default::default()
            }
        }
        Err(Refused::Other) => return SealedEnd::default(),
    };
    let (mut sink, mut stream) = ws.split();

    let (tx, mut rx) = mpsc::channel::<HubMsg>(SEND_QUEUE);
    let (close_tx, mut close_rx) = watch::channel::<Option<String>>(None);
    let tx = ConnTx::new(tx, close_tx);
    let box_name = rbox.name().to_string();
    let connected = {
        let mut core = hub.core();
        rbox.connect(&inv, tx.clone()).map(|epoch| {
            // Not ready until synced and reconciled.
            core.online_boxes.remove(&box_name);
            core.adopt_remote_sessions(&hub, &rbox, &inv);
            // The runner deletes every frame at or below a watermark it is told about, so only
            // durable commits may be: the fenced previous connection's open ack batch (frames
            // committed without a sync, see the module docs) is synced first. Under the same
            // lock as the fence, so nothing commits for the box in between.
            let watermarks = core
                .store
                .sync()
                .context("syncing the store before the welcome")
                .and_then(|()| core.store.runner_watermarks(&box_name));
            if watermarks.is_err() {
                // Under the same lock: nothing else saw this connection.
                rbox.disconnect(epoch);
            }
            (epoch, watermarks)
        })
    };
    let Some((epoch, watermarks)) = connected else {
        // Nothing changed: the current connection (if any) stays.
        tracing::error!(box_name = %box_name, "runner refused: no epoch left for another connection of the box (restart the hub)");
        let m = refuse(
            hub_errors::EXHAUSTED,
            "the hub cannot number another connection of this box; restart the hub".into(),
        );
        let _ = tokio::time::timeout(FLUSH_TIMEOUT, async {
            if flushed_send(&mut sink, text(&m)).await.is_ok() {
                let _ = sink.close().await;
            }
        })
        .await;
        return SealedEnd::default();
    };
    let watermarks = match watermarks {
        Ok(w) => w,
        Err(e) => {
            tracing::error!(box_name = %box_name, error = %format!("{e:#}"), "runner refused: cannot give it durable watermarks");
            let _ = tokio::time::timeout(FLUSH_TIMEOUT, sink.close()).await;
            return SealedEnd::default();
        }
    };
    tracing::info!(
        box_name = %box_name,
        epoch,
        peer,
        version = %inv.version,
        running = inv.running.len(),
        "runner connected"
    );
    tx.send(HubMsg::Welcome { epoch, watermarks });
    // The box's agent CLIs, asked on this connection while it lasts (`hub::box_agents`).
    let _agents = super::box_agents::AbortOnDrop(tokio::spawn(super::box_agents::poll(
        rbox.clone(),
        epoch,
        hub.agents_poll,
    )));
    let end = SealedEnd {
        welcomed: true,
        protocol_refused: None,
        build: inv.build.clone(),
    };
    if let Some(w) = welcomed {
        let _ = w.send(inv.build.clone());
    }
    // Its end (a write failed or timed out, or every sender is gone) ends the connection.
    let (writer_done, mut writer_ended) = oneshot::channel::<()>();
    let writer_box = box_name.clone();
    let writer = tokio::spawn(async move {
        let _done = writer_done;
        while let Some(m) = rx.recv().await {
            match tokio::time::timeout(WRITE_TIMEOUT, flushed_send(&mut sink, text(&m))).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::debug!(box_name = %writer_box, error = %e, "writing to a runner failed");
                    return;
                }
                Err(_) => {
                    tracing::warn!(box_name = %writer_box, "a runner did not take a message within {}s", WRITE_TIMEOUT.as_secs());
                    return;
                }
            }
        }
        let _ = tokio::time::timeout(FLUSH_TIMEOUT, sink.close()).await;
    });

    let mut conn = Conn {
        rbox: rbox.clone(),
        epoch,
        tx,
        version: inv.version.clone(),
        build: inv.build.clone(),
        running: inv
            .running
            .iter()
            .map(|r| (r.thread.clone(), r.run.clone()))
            .collect(),
        synced: false,
        announced: false,
        stopped: HashSet::new(),
        unacked: BTreeMap::new(),
        batched: 0,
        batch_since: None,
        batch_delay: hub.ack_batch_delay,
    };
    let mut state = hub.state.subscribe();
    // Only protocol messages keep the connection open: WebSocket pings (answered by the
    // WebSocket layer) do not move it.
    let idle = hub.runner_idle_timeout;
    let mut idle_at = tokio::time::Instant::now() + idle;
    let why = loop {
        let flush_at = conn.flush_at();
        let next = tokio::select! {
            biased;
            r = close_rx.wait_for(Option::is_some) => {
                break r.map_or_else(|_| "closed".to_string(), |c| c.clone().unwrap_or_default())
            }
            _ = &mut writer_ended => break "its writer stopped (a write failed or timed out)".to_string(),
            _ = state.wait_for(|s| *s == HubState::Stopped) => break "the hub stopped".to_string(),
            // Before reading more: a batch whose time is up is synced and acked even while
            // frames keep coming.
            () = tokio::time::sleep_until(flush_at.unwrap_or_else(tokio::time::Instant::now)), if flush_at.is_some() => None,
            m = tokio::time::timeout_at(idle_at, stream.next()) => Some(m),
        };
        let Some(next) = next else {
            if let Err(why) = flush_acks(&hub, &mut conn).await {
                break why;
            }
            continue;
        };
        let msg = match next {
            Err(_) => break format!("no message for {}s", idle.as_secs()),
            Ok(None) => break "closed".to_string(),
            Ok(Some(Err(e))) => break format!("read error: {e}"),
            Ok(Some(Ok(Message::Close(_)))) => break "closed by the runner".to_string(),
            Ok(Some(Ok(Message::Text(t)))) => {
                idle_at = tokio::time::Instant::now() + idle;
                t
            }
            Ok(Some(Ok(_))) => continue,
        };
        let msg = match serde_json::from_str::<RunnerMsg>(msg.as_str()) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(box_name = %box_name, error = %e, "ignoring a malformed runner message");
                continue;
            }
        };
        if !rbox.is_current(epoch) {
            break "fenced by a newer connection".to_string();
        }
        // Anything but a frame first settles the frames before it (acks go out in order).
        let frame = matches!(msg, RunnerMsg::Event { .. } | RunnerMsg::Exited { .. });
        if !frame {
            if let Err(why) = flush_acks(&hub, &mut conn).await {
                break why;
            }
        }
        if let Err(why) = handle(&hub, &mut conn, msg).await {
            break why;
        }
        if frame {
            conn.batched();
            if conn.batched >= ACK_BATCH_FRAMES {
                if let Err(why) = flush_acks(&hub, &mut conn).await {
                    break why;
                }
            }
        }
    };
    tracing::info!(box_name = %box_name, epoch, reason = %why, "runner connection ended");
    connection_ended(&hub, &rbox, epoch, conn.announced, &conn.version);
    drop(conn.tx);
    // The writer ends once every sender is gone (the box dropped its copy above).
    let mut writer = writer;
    if tokio::time::timeout(FLUSH_TIMEOUT, &mut writer)
        .await
        .is_err()
    {
        writer.abort();
    }
    end
}

/// Connection `epoch` of `rbox` ended: when it was still the box's current one, the box is
/// offline (and `runner.disconnected` is recorded once `runner.connected` was). The whole of
/// it happens under the core lock, where a newer connection replaces the current one
/// ([`serve`]): the epoch is checked and the box taken offline in one step, so the end of a
/// connection a replacement already fenced never takes that replacement offline.
fn connection_ended(
    hub: &Arc<Inner>,
    rbox: &RemoteBox,
    epoch: u64,
    announced: bool,
    version: &str,
) {
    let mut core = hub.core();
    if !rbox.disconnect(epoch) {
        return;
    }
    #[cfg(test)]
    tests::after_disconnect();
    let box_name = rbox.name().to_string();
    core.online_boxes.remove(&box_name);
    if announced && !core.shutting_down {
        let body = EventBody::RunnerDisconnected(RunnerPayload {
            box_name: box_name.clone(),
            version: Some(version.to_string()),
            ..Default::default()
        });
        if let Err(e) = core.append(hub, HUB_THREAD, None, Source::Hub, body) {
            tracing::error!(box_name = %box_name, error = %format!("{e:#}"), "cannot record runner.disconnected");
        }
    }
}

/// Handles one message; `Err(reason)` closes the connection.
async fn handle(hub: &Arc<Inner>, conn: &mut Conn, msg: RunnerMsg) -> Result<(), String> {
    match msg {
        RunnerMsg::Event { thread, rseq, .. } | RunnerMsg::Exited { thread, rseq, .. }
            if rseq == 0 =>
        {
            tracing::warn!(box_name = %conn.box_name(), thread, "a runner frame with rseq 0 (rseq starts at 1); closing the connection");
            return Err(format!("a frame of thread {thread} with rseq 0"));
        }
        RunnerMsg::Event {
            thread,
            rseq,
            turn,
            body,
            raw,
            run,
            at_ms,
        } => {
            let at = capture_time(at_ms);
            let ack = RunnerAck {
                box_name: conn.box_name().to_string(),
                thread: thread.clone(),
                rseq,
            };
            let kind = body.kind().to_string();
            let ev = (body, raw, turn);
            let fence = conn.fence();
            let r = persist(hub, &thread, "recording a runner event", |core| {
                core.deferring_sync(|core| {
                    core.ingest_event(hub, &ack, &fence, ev.clone(), run.as_deref(), at)
                })
            })
            .await;
            match r {
                Ok(Ingested::Stopping) => return Err("the hub is stopping".into()),
                Ok(Ingested::Fenced) => return Err("fenced by a newer connection".into()),
                Ok(Ingested::Duplicate(w, missed)) => {
                    conn.ack(&thread, w);
                    // Reactions an earlier connection recorded the turn's end without.
                    if let Some(t) = missed {
                        let reacted =
                            persist(hub, &thread, "reacting to the end of a turn", |core| {
                                core.react_to_adapter_turn_end(hub, &thread, &t)
                            })
                            .await;
                        if let Err(e) = reacted {
                            tracing::error!(box_name = %conn.box_name(), thread, turn = t.turn, error = %format!("{e:#}"), "cannot react to the end of a turn; the next replay or hub start does");
                        }
                    }
                }
                Ok(Ingested::Gap(w)) => return Err(gap(conn, &thread, rseq, w)),
                Ok(Ingested::Stray) => {
                    conn.ack(&thread, rseq);
                    tracing::debug!(box_name = %conn.box_name(), thread, kind, "frame of a session the hub does not run");
                    conn.stop_stray(&thread, run);
                }
                Ok(Ingested::Foreign(w)) => {
                    conn.ack(&thread, w);
                    conn.stop_stray(&thread, run);
                }
                Ok(Ingested::Recorded { generation, ended }) => {
                    conn.ack(&thread, rseq);
                    if let Some(t) = ended {
                        // The turn's end is acked before its reactions, which may take a
                        // while (retried, or given up on). The reactions run even when the
                        // flush fails (the connection then closes): the replayed frame is a
                        // duplicate, and nothing else would react before a hub restart.
                        let flushed = flush_acks(hub, conn).await;
                        let reacted =
                            persist(hub, &thread, "reacting to the end of a turn", |core| {
                                core.react_to_adapter_turn_end(hub, &thread, &t)
                            })
                            .await;
                        if let Err(e) = reacted {
                            give_up_runtime(hub, &thread, generation, e).await;
                        }
                        git::snapshot_soon(hub, &thread);
                        flushed?;
                    }
                }
                Err(e) => {
                    tracing::error!(box_name = %conn.box_name(), thread, rseq, error = %format!("{e:#}"), "cannot record a runner frame; closing the connection (the runner sends it again)");
                    return Err("store failure".into());
                }
            }
        }
        RunnerMsg::Exited {
            thread,
            rseq,
            code,
            stderr_tail,
            run,
            at_ms,
        } => {
            let at = capture_time(at_ms);
            let ack = RunnerAck {
                box_name: conn.box_name().to_string(),
                thread: thread.clone(),
                rseq,
            };
            let fence = conn.fence();
            let r = persist(hub, &thread, "recording a runner frame", |core| {
                core.deferring_sync(|core| {
                    core.ingest_exit(hub, &ack, &fence, (code, stderr_tail.clone()), run.as_deref(), at)
                })
            })
            .await
            .map_err(|e| {
                tracing::error!(thread, error = %format!("{e:#}"), "cannot record a runner frame; closing the connection (the runner sends it again)");
                "store failure".to_string()
            })?;
            match r {
                Exited::Stopping => return Err("the hub is stopping".into()),
                Exited::Fenced => return Err("fenced by a newer connection".into()),
                Exited::Duplicate(w) => conn.ack(&thread, w),
                Exited::Gap(w) => return Err(gap(conn, &thread, rseq, w)),
                Exited::Foreign(w) => conn.ack(&thread, w),
                Exited::Stray => {
                    conn.ack(&thread, rseq);
                    conn.exited(&thread, run.as_deref());
                }
                Exited::Close(mut exit) => {
                    // Records the closing sequence with the frame's watermark (or fails the
                    // hub), then dispatches what follows a requested stop.
                    if !close_exited(hub, &thread, &mut exit).await {
                        return Err("store failure".into());
                    }
                    // Fenced meanwhile: the closing sequence went in without the watermark
                    // (fencing is for good, so a fence that holds now held at the commit).
                    if !fence.holds() {
                        return Err("fenced by a newer connection".into());
                    }
                    conn.ack(&thread, rseq);
                    conn.exited(&thread, run.as_deref());
                }
            }
        }
        RunnerMsg::Synced => {
            if hub.core().shutting_down {
                return Err("the hub is stopping".into());
            }
            if !conn.synced {
                reconcile(hub, conn).await;
            }
        }
        RunnerMsg::CmdResult(r) => {
            if remote::is_durable_cmd_id(&r.cmd_id) {
                // Like a frame, a recorded command's result changes the hub's state only while
                // its connection is the box's current one, checked under the core lock at
                // every attempt: a newer connection may already have sent the command again
                // (same id), and its own result is the one that counts.
                let fence = conn.fence();
                let failed = (!r.ok).then(|| {
                    r.error.clone().unwrap_or_else(|| {
                        CmdError::new(cmd_errors::FAILED, "the runner gave no reason")
                    })
                });
                match failed {
                    // The runner keeps the result for good (it answers a repeat with it): a
                    // recorded command is delivered.
                    None => {
                        let mut core = hub.core();
                        if !fence.holds() {
                            return Err("fenced by a newer connection".into());
                        }
                        let respond = match core.store.remote_cmd(conn.box_name(), &r.cmd_id) {
                            Ok(c) => c.filter(|c| c.cmd_id.starts_with("respond:")),
                            Err(_) => None,
                        };
                        let done = core.store.remote_cmd_done(conn.box_name(), &r.cmd_id);
                        match done {
                            Err(e) => {
                                tracing::warn!(cmd_id = %r.cmd_id, error = %format!("{e:#}"), "cannot forget a delivered command (it is sent again later; the runner answers it again)");
                            }
                            // The answer is with its runtime: the thread's next turn, which
                            // waited for it (`Core::thread_free`), may go now.
                            Ok(_) => {
                                if let Some(c) = respond {
                                    if let Err(e) = core.try_dispatch(hub, &c.thread) {
                                        tracing::error!(thread = %c.thread, error = %format!("{e:#}"), "dispatching after an answer was delivered failed");
                                    }
                                }
                            }
                        }
                    }
                    // Never only logged: see `Core::failed_remote_cmd`.
                    Some(e) => {
                        let box_name = conn.box_name().to_string();
                        // A retired `respond` no longer holds its thread's next turn
                        // (`Core::thread_free`): the queue may go (unless the failure holds it).
                        let respond_of = r
                            .cmd_id
                            .starts_with("respond:")
                            .then(|| hub.core().store.remote_cmd(&box_name, &r.cmd_id))
                            .and_then(Result::ok)
                            .flatten()
                            .map(|c| c.thread);
                        let r = persist(hub, &r.cmd_id, "recording a failed command", |core| {
                            if !fence.holds() {
                                return Ok(false);
                            }
                            core.failed_remote_cmd(hub, &box_name, &r.cmd_id, &e)
                                .map(|()| true)
                        })
                        .await;
                        match r {
                            Ok(true) => {
                                if let Some(thread) = respond_of {
                                    let mut core = hub.core();
                                    if let Err(e) = core.try_dispatch(hub, &thread) {
                                        tracing::error!(thread, error = %format!("{e:#}"), "dispatching after an answer was retired failed");
                                    }
                                }
                            }
                            Ok(false) => return Err("fenced by a newer connection".into()),
                            Err(e) => {
                                tracing::error!(box_name, error = %format!("{e:#}"), "cannot record what a failed command means; it stays recorded and goes again at the next connection (the runner answers it again)");
                            }
                        }
                    }
                }
            }
            conn.rbox.on_result(conn.epoch, r)
        }
        RunnerMsg::Ping => conn.send(HubMsg::Pong),
        RunnerMsg::Pong => {}
        RunnerMsg::Hello(_) | RunnerMsg::Auth { .. } | RunnerMsg::Inventory(_) => {
            return Err("a handshake message after the welcome".into())
        }
    }
    Ok(())
}

/// Ends the open ack batch of `conn`: syncs what its frames committed ([`store::Store::sync`])
/// and then acks them (the highest rseq per thread). A sync that keeps failing closes the
/// connection unacked (the runner sends the frames again; they are duplicates by then).
async fn flush_acks(hub: &Arc<Inner>, conn: &mut Conn) -> Result<(), String> {
    conn.batched = 0;
    conn.batch_since = None;
    if conn.unacked.is_empty() {
        return Ok(());
    }
    let synced = persist(hub, conn.box_name(), "syncing runner frames", |core| {
        core.store.sync()
    })
    .await;
    if let Err(e) = synced {
        tracing::error!(box_name = %conn.box_name(), error = %format!("{e:#}"), "cannot sync runner frames; closing the connection (the runner sends them again)");
        return Err("store failure".into());
    }
    for (thread, rseq) in std::mem::take(&mut conn.unacked) {
        conn.send(HubMsg::Ack { thread, rseq });
    }
    Ok(())
}

/// A frame's capture time (`at_ms`, the runner's clock), when it is a representable time; the
/// hub keeps it within the thread's order and never later than now (`Core::event_time`).
fn capture_time(at_ms: Option<u64>) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(i64::try_from(at_ms?).ok()?)
}

fn gap(conn: &Conn, thread: &str, rseq: u64, watermark: u64) -> String {
    tracing::error!(
        box_name = %conn.box_name(),
        thread,
        rseq,
        watermark,
        "runner frame gap: expected rseq {}; closing the connection so the runner replays its spool",
        watermark.saturating_add(1)
    );
    format!("gap in thread {thread}: rseq {rseq} after {watermark}")
}

/// The reconciliation at `synced` (see the module docs), then the box goes online.
async fn reconcile(hub: &Arc<Inner>, conn: &mut Conn) {
    let box_name = conn.box_name().to_string();
    let fence = conn.fence();
    let lost = format!(
        "twr: the runner of box {box_name} does not run this runtime any more (it restarted or \
         lost the process)"
    );
    // Threads whose runtime the runner lost and that crashed: they get their crash reactions
    // (and the automatic retry when it is due) but nothing from the queue.
    let mut crashed: HashSet<String> = HashSet::new();
    let (exits, strays, threads) = {
        let mut core = hub.core();
        if !fence.holds() {
            return;
        }
        let threads: Vec<twr_core::reduce::ThreadView> = core
            .registry
            .list()
            .into_iter()
            .filter(|v| v.box_name == box_name)
            .cloned()
            .collect();
        let mut exits = Vec::new();
        let mut strays = Vec::new();
        for v in &threads {
            let running = conn.running.get(&v.id);
            let rt = core.rt.get(&v.id);
            let generation = rt.and_then(|r| r.live.as_ref()).map(|l| l.generation);
            let process_live = rt.is_some_and(|r| r.process_live);
            let stop_requested = rt.is_some_and(|r| r.stop_requested);
            // The run of the hub's session, once the runner confirmed it.
            let session_run = rt
                .and_then(|r| r.live.as_ref())
                .filter(|l| l.run_confirmed)
                .and_then(|l| l.run.clone());
            let named_run = rt.and_then(|r| r.live.as_ref()).and_then(|l| l.run.clone());
            match (running, generation) {
                (None, Some(g)) if process_live => {
                    tracing::warn!(thread = %v.id, box_name = %box_name, "the runner lost a live runtime");
                    if let Some(exit) = core.begin_exit(&v.id, g, None, lost.clone()) {
                        if exit.crash {
                            crashed.insert(v.id.clone());
                        }
                        exits.push((v.id.clone(), exit));
                    }
                }
                (None, None)
                    if (process_live || v.has_work() || stop_requested)
                        && !v.parked
                        && v.attached.is_none() =>
                {
                    // A runtime the user asked to stop is closed as stopped.
                    let exit = if stop_requested {
                        Exit::Stopped
                    } else {
                        tracing::warn!(thread = %v.id, box_name = %box_name, "the runner lost a live runtime");
                        Exit::Crashed {
                            exit_code: None,
                            stderr_tail: lost.clone(),
                        }
                    };
                    let crash = matches!(exit, Exit::Crashed { .. });
                    match core.close_process(hub, &v.id, exit) {
                        Ok(()) if crash => {
                            crashed.insert(v.id.clone());
                            // Classified like any crash; its retry (if due) waits for the box.
                            if let Err(e) = core.react_to_failure(hub, &v.id) {
                                tracing::error!(thread = %v.id, error = %format!("{e:#}"), "cannot react to a lost runtime");
                            }
                        }
                        Ok(()) => {}
                        Err(e) => {
                            tracing::error!(thread = %v.id, error = %format!("{e:#}"), "cannot close a lost runtime")
                        }
                    }
                }
                (Some(run), None) => strays.push((v.id.clone(), run.clone())),
                // The runner runs another run of the thread than the hub's session (whose own
                // end the replay did not bring): that session is lost, and the run the runner
                // has is a stray.
                (Some(Some(run)), Some(g))
                    if session_run.as_ref().is_some_and(|known| known != run) =>
                {
                    tracing::warn!(thread = %v.id, box_name = %box_name, run, "the runner runs another run than the hub's session");
                    if let Some(exit) = core.begin_exit(&v.id, g, None, lost.clone()) {
                        if exit.crash {
                            crashed.insert(v.id.clone());
                        }
                        exits.push((v.id.clone(), exit));
                    }
                    strays.push((v.id.clone(), Some(run.clone())));
                }
                // The hub's session has not started as far as the runner has said (its
                // `spawn` / `resume` may still be on its way) and the runner runs another run:
                // that one is a stray; the session's own command decides its fate.
                (Some(Some(run)), Some(_))
                    if named_run.as_ref().is_some_and(|named| named != run) =>
                {
                    strays.push((v.id.clone(), Some(run.clone())));
                }
                _ => {}
            }
        }
        let known: HashSet<&str> = threads.iter().map(|v| v.id.as_str()).collect();
        strays.extend(
            conn.running
                .iter()
                .filter(|(t, _)| !known.contains(t.as_str()))
                .map(|(t, r)| (t.clone(), r.clone())),
        );
        let ids: Vec<String> = threads.into_iter().map(|v| v.id).collect();
        (exits, strays, ids)
    };
    for (thread, mut exit) in exits {
        close_exited(hub, &thread, &mut exit).await;
    }
    conn.synced = true;
    let mut core = hub.core();
    // Fenced meanwhile: the recorded commands are the newer connection's to retire or send.
    if !fence.holds() {
        return;
    }
    // Strays are stopped before anything else goes out (their stops name their runs, so they
    // never stop a replacement either), then the commands that wait for the box.
    let resend = core.remote_resends(hub, &box_name);
    if !conn.rbox.set_ready(conn.epoch, &strays, resend) {
        return;
    }
    conn.stopped.extend(strays);
    core.online_boxes.insert(box_name.clone());
    // A switch to another build is over once a connection after it is reconciled
    // (`hub::box_switch`): its threads are dispatched below.
    let switched = core.switching_boxes.remove(&box_name);
    let body = EventBody::RunnerConnected(RunnerPayload {
        box_name: box_name.clone(),
        version: Some(conn.version.clone()),
        ..Default::default()
    });
    match core.append(hub, HUB_THREAD, None, Source::Hub, body) {
        Ok(_) => conn.announced = true,
        Err(e) => {
            tracing::error!(box_name = %box_name, error = %format!("{e:#}"), "cannot record runner.connected")
        }
    }
    // Prompts that waited for the box (and due retries). A thread whose runtime the runner
    // lost gets only its due retry: nothing is dispatched after a crash.
    for thread in threads {
        let r = if crashed.contains(&thread) {
            if core.thread_free(&thread) {
                core.start_retry(hub, &thread).map(|_| ())
            } else {
                Ok(())
            }
        } else {
            core.try_dispatch(hub, &thread).map(|_| ())
        };
        if let Err(e) = r {
            tracing::error!(thread, error = %format!("{e:#}"), "dispatching after a runner reconnected failed");
        }
    }
    drop(core);
    tracing::info!(box_name = %box_name, epoch = conn.epoch, "runner synced; box online");
    if switched {
        box_switch::ended(hub, &box_name, conn.build.as_deref());
    }
    // What a crash left the log and this box's trash disagreeing on.
    let hub = super::Hub {
        inner: Arc::clone(hub),
    };
    tokio::spawn(async move {
        hub.reconcile_boxes(Some(&box_name)).await;
    });
}

/// Settles the recorded commands the sessions' forwarders of `rbox` give up on unfinished
/// ([`RemoteBox::take_orphans`]) while the box is connected: each is forgotten when it no
/// longer applies, else sent again (the rules of [`Core::remote_resends`]); while the box is
/// not ready they are left to its next reconciliation, which settles every record. Ends with
/// the hub.
pub(super) async fn settle_orphans(
    hub: std::sync::Weak<Inner>,
    rbox: RemoteBox,
    mut state: watch::Receiver<HubState>,
) {
    loop {
        tokio::select! {
            _ = rbox.orphans_changed() => {}
            _ = state.wait_for(|s| *s == HubState::Stopped) => return,
        }
        let Some(inner) = hub.upgrade() else {
            return;
        };
        let mut core = inner.core();
        if core.shutting_down {
            return;
        }
        let ids: HashSet<String> = rbox.take_orphans().into_iter().collect();
        if ids.is_empty() || !rbox.is_ready() {
            continue;
        }
        let resend = core.remote_resends_of(&inner, rbox.name(), Some(&ids));
        rbox.resend(resend);
    }
}

/// Recorded command `c` was just committed: the session's forwarder issues it as recorded (a
/// `respond` under its id and bound to its run, a `send` bound to its run).
pub(super) fn expect_recorded(hub: &Inner, c: &store::RemoteCmd) {
    let Some(rbox) = hub.remote(&c.box_name) else {
        return;
    };
    match serde_json::from_str(&c.op) {
        Ok(Op::Respond {
            thread,
            attn_id,
            run,
            ..
        }) => rbox.expect_respond(&thread, &attn_id, &c.cmd_id, run),
        Ok(Op::Send { run, .. }) => rbox.expect_send(&c.cmd_id, run),
        _ => {}
    }
}

/// Recorded command `c`, which [`expect_recorded`] announced, will not be issued by a session's
/// forwarder after all (its runtime was gone): its announcement is withdrawn.
pub(super) fn forget_recorded(hub: &Inner, c: &store::RemoteCmd) {
    let Some(rbox) = hub.remote(&c.box_name) else {
        return;
    };
    if let Ok(Op::Respond {
        thread, attn_id, ..
    }) = serde_json::from_str(&c.op)
    {
        rbox.forget_respond(&thread, &attn_id, &c.cmd_id);
    }
}

impl Core {
    /// Takes over the sessions `inv` reports (running, or with frames still to replay) whose
    /// thread on this box the log shows as live but that the hub runs no runtime for: after a
    /// hub restart, the runner still runs them.
    fn adopt_remote_sessions(&mut self, hub: &Arc<Inner>, rbox: &RemoteBox, inv: &Inventory) {
        let mut candidates: BTreeSet<&str> =
            inv.running.iter().map(|r| r.thread.as_str()).collect();
        candidates.extend(inv.acks_needed.keys().map(String::as_str));
        for thread in candidates {
            let Some(view) = self.registry.get(thread).cloned() else {
                continue;
            };
            if view.box_name != rbox.name() || view.parked || view.attached.is_some() {
                continue;
            }
            let rt = self.rt(thread);
            // Not while an exit of the thread's runtime is being recorded: the log still shows
            // that runtime live, but it is gone (its closing sequence is on its way in), and
            // the runner's replay may well bring that same exit again.
            if rt.live.is_some() || rt.exiting || !(rt.process_live || view.has_work()) {
                continue;
            }
            let stop_requested = rt.stop_requested;
            let run = inv
                .running
                .iter()
                .find(|r| r.thread == thread)
                .and_then(|r| r.run.clone());
            let mut handle = rbox.start_session(thread, None);
            let Some(events) = handle.take_events() else {
                continue;
            };
            // A `thread.stop` the previous hub asked for (`crate::store` `stop_requests`): the
            // runtime is asked again, and its exit closes the thread as stopped.
            if stop_requested {
                tracing::info!(thread, box_name = %rbox.name(), "asking a taken-over session to stop again");
                let _ = handle.stop();
            }
            self.generation += 1;
            let generation = self.generation;
            let log_runtime = self.runtime_gen(thread);
            self.rt(thread).live = Some(Live {
                handle,
                generation,
                stop_requested,
                attach: false,
                park: false,
                _mcp_file: None,
                // (Its runner holds the working directory.)
                _workdir: None,
                run_confirmed: run.is_some(),
                run,
                given_up: None,
                log_runtime,
            });
            hub.live_count.send_modify(|n| *n += 1);
            tokio::spawn(supervise(
                Arc::clone(hub),
                thread.to_string(),
                generation,
                Source::Adapter(view.runtime.clone()),
                events,
            ));
            tracing::info!(thread, box_name = %rbox.name(), "took over a session the runner still runs");
        }
    }

    /// Runs `f` with runner frames committed without a sync each (their connection syncs the
    /// batch before acking it, [`flush_acks`]).
    fn deferring_sync<T>(&mut self, f: impl FnOnce(&mut Core) -> T) -> T {
        self.defer_ack_sync = true;
        let r = f(self);
        self.defer_ack_sync = false;
        r
    }

    /// Where `rseq` of `ack`'s thread falls against the committed watermark.
    fn classify(&self, ack: &RunnerAck) -> anyhow::Result<Result<(), (bool, u64)>> {
        let w = self.store.runner_ack(&ack.box_name, &ack.thread)?;
        // Checked: `rseq` is the runner's, and a watermark at `u64::MAX` has no next frame
        // (everything is then a duplicate).
        Ok(if ack.rseq <= w {
            Err((false, w))
        } else if w.checked_add(1) != Some(ack.rseq) {
            Err((true, w))
        } else {
            Ok(())
        })
    }

    /// Whether `ack`'s thread is known and on another box than the frame's: then the frame is
    /// counted and logged (see the module docs).
    fn foreign(&self, ack: &RunnerAck, fence: &Fence) -> bool {
        let Some(owner) = self.registry.get(&ack.thread).map(|v| v.box_name.as_str()) else {
            return false;
        };
        if owner == ack.box_name {
            return false;
        }
        fence.rbox.note_rejected();
        tracing::warn!(
            box_name = %ack.box_name,
            thread = %ack.thread,
            owner = %owner,
            rseq = ack.rseq,
            "refusing a runner frame for a thread of another box"
        );
        true
    }

    /// [`Core::foreign`] for a new frame (`rseq` is the watermark's next): then only its
    /// watermark is committed.
    fn refuse_foreign(
        &mut self,
        hub: &Inner,
        ack: &RunnerAck,
        fence: &Fence,
    ) -> anyhow::Result<bool> {
        if !self.foreign(ack, fence) {
            return Ok(false);
        }
        self.commit_ack(hub, Some(ack))?;
        Ok(true)
    }

    /// The ingest of one `event` frame (see the module docs). Safe to repeat after a failure:
    /// nothing is recorded unless everything is.
    fn ingest_event(
        &mut self,
        hub: &Inner,
        ack: &RunnerAck,
        fence: &Fence,
        ev: (EventBody, Option<String>, Option<u64>),
        run: Option<&str>,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Ingested> {
        if self.shutting_down {
            return Ok(Ingested::Stopping);
        }
        if !fence.holds() {
            return Ok(Ingested::Fenced);
        }
        match self.classify(ack)? {
            // A duplicate of another box's thread is refused like a new frame: nothing of the
            // thread (its turn's reactions included) is ever touched for it.
            Err((false, w)) if self.foreign(ack, fence) => return Ok(Ingested::Foreign(w)),
            Err((false, w)) => {
                let missed = self.missed_turn_end(&ack.thread, &ev.0, ev.2)?;
                return Ok(Ingested::Duplicate(w, missed));
            }
            Err((true, w)) => return Ok(Ingested::Gap(w)),
            Ok(()) => {}
        }
        if self.refuse_foreign(hub, ack, fence)? {
            return Ok(Ingested::Foreign(ack.rseq));
        }
        // Stamped with the runtime the session was started as (a thread moved to another
        // runtime keeps the previous one running until its turn ends: what it sends is its
        // own), else the thread's.
        let session = self.registry.get(&ack.thread).and_then(|v| {
            let rt = self.rt.get(&ack.thread)?;
            let live = rt.live.as_ref()?;
            let runtime = rt
                .runtime_settings
                .as_ref()
                .map(|s| s.runtime.clone())
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| v.runtime.clone());
            Some((live.generation, runtime, live.run.clone()))
        });
        // A frame of another run than the session's (once one is known) is not the session's.
        let session = session.filter(|(_, _, known)| match (known, run) {
            (Some(k), Some(r)) => k == r,
            _ => true,
        });
        let Some((generation, runtime, known)) = session else {
            self.commit_ack(hub, Some(ack))?;
            return Ok(Ingested::Stray);
        };
        if let (Some(r), Some(live)) = (
            run,
            self.rt.get_mut(&ack.thread).and_then(|x| x.live.as_mut()),
        ) {
            if known.is_none() {
                live.run = Some(r.to_string());
            }
            live.run_confirmed = true;
        }
        let ended = self.record_adapter_event(
            hub,
            &ack.thread,
            generation,
            &Source::Adapter(runtime),
            ev,
            Some(ack),
            at,
        )?;
        Ok(Ingested::Recorded { generation, ended })
    }

    /// The turn a duplicate frame (`body`, turn `turn`) ended, when it is a `turn.ended` the
    /// log has and the hub's reactions to it may be missing: a completed or failed turn
    /// without its report (or due review), or the thread's latest turn while no other is
    /// open (its adapter items and queue, which an interrupted turn's reactions are about).
    /// A turn's end is recorded before its reactions run, so a connection that ended between
    /// the two (the reactions failed, or were never reached) leaves them to the replay.
    /// Every reaction step skips what the log already has. Only an end the box's runtime
    /// reported counts (the log's `turn.ended` comes from an adapter): one the hub recorded
    /// itself (the turn of a runtime that crashed or was lost, which holds the queue) is never
    /// reacted to again for a frame.
    fn missed_turn_end(
        &self,
        thread: &str,
        body: &EventBody,
        turn: Option<u64>,
    ) -> anyhow::Result<Option<super::EndedTurn>> {
        let (EventBody::TurnEnded(_), Some(turn)) = (body, turn) else {
            return Ok(None);
        };
        let Some(view) = self.registry.get(thread) else {
            return Ok(None);
        };
        // What the log recorded (a refused end was recorded bare, or not at all).
        let result = match store::last_of_kind(self.store.conn(), thread, Some(turn), "turn.ended")?
        {
            Some(Event {
                body: EventBody::TurnEnded(p),
                source: Source::Adapter(_),
                ..
            }) => p.result,
            _ => return Ok(None),
        };
        let latest =
            view.current_turn.is_none() && view.last_turn.as_ref().is_some_and(|t| t.turn == turn);
        let missing = result != TurnResult::Interrupted
            && !self.turn_reactions_complete(thread, turn, result, !view.is_orchestrator())?;
        if !(missing || latest) {
            return Ok(None);
        }
        Ok(Some(super::EndedTurn {
            turn,
            runtime_gen: self.runtime_gen(thread),
            result,
            final_text: self.logged_final_text(thread, turn)?,
        }))
    }

    /// The first half of the ingest of one `exited` frame: takes the hub's session of the
    /// thread and plans its closing sequence, carrying the frame's ack and fence (the caller
    /// records it with [`close_exited`]). A stray's frame is committed here (its watermark
    /// alone). Safe to repeat after a failure.
    fn ingest_exit(
        &mut self,
        hub: &Inner,
        ack: &RunnerAck,
        fence: &Fence,
        (code, stderr_tail): (Option<i32>, String),
        run: Option<&str>,
        at: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Exited> {
        if self.shutting_down {
            return Ok(Exited::Stopping);
        }
        if !fence.holds() {
            return Ok(Exited::Fenced);
        }
        match self.classify(ack)? {
            Err((false, w)) if self.foreign(ack, fence) => return Ok(Exited::Foreign(w)),
            Err((false, w)) => return Ok(Exited::Duplicate(w)),
            Err((true, w)) => return Ok(Exited::Gap(w)),
            Ok(()) => {}
        }
        if self.refuse_foreign(hub, ack, fence)? {
            return Ok(Exited::Foreign(ack.rseq));
        }
        // The exit of the session's run: when the session's run is known, an exit that names
        // another run, or none (a runner recovering a run it lost track of, possibly after it
        // already reported that run's end), is not this session's and closes nothing.
        let generation = self
            .rt
            .get(&ack.thread)
            .and_then(|r| r.live.as_ref())
            .filter(|l| match (&l.run, run) {
                (Some(known), Some(r)) => known == r,
                (Some(_), None) => !l.run_confirmed,
                (None, _) => true,
            })
            .map(|l| l.generation);
        let exit = generation.and_then(|g| self.begin_exit(&ack.thread, g, code, stderr_tail));
        match exit {
            Some(mut exit) => {
                exit.runner_ack = Some(ack.clone());
                exit.fence = Some(fence.clone());
                exit.at = at;
                Ok(Exited::Close(Box::new(exit)))
            }
            None => {
                self.commit_ack(hub, Some(ack))?;
                Ok(Exited::Stray)
            }
        }
    }

    /// The command for the remote box that delivering turn `turn` (`text`) to `thread`
    /// implies, to be recorded with the turn's start (`remote_outbox`): a `send` when the
    /// prompt goes to a running or resumed session (a new session gets it with its `spawn`,
    /// which is not recorded). `None` for a local thread.
    pub(super) fn remote_send(
        &self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        text: &str,
    ) -> Option<store::RemoteCmd> {
        let live = self.rt.get(thread).is_some_and(|r| r.live.is_some());
        let resumes = self
            .registry
            .get(thread)
            .is_some_and(|v| v.session_id.is_some());
        if !live && !resumes {
            return None;
        }
        let cmd = AdapterCmd::Send {
            text: text.to_string(),
            turn,
        };
        self.remote_cmd(hub, thread, Some(turn), cmd)
    }

    /// The command for the remote box that forwarding `answer` to the runtime's item `attn_id`
    /// (its own id, the thread namespace stripped) implies, to be recorded with its
    /// `attention.resolved`. `None` for a local thread.
    pub(super) fn remote_respond(
        &self,
        hub: &Inner,
        thread: &str,
        turn: Option<u64>,
        attn_id: &str,
        answer: &Answer,
    ) -> Option<store::RemoteCmd> {
        let cmd = AdapterCmd::Respond {
            attn_id: attn_id.to_string(),
            answer: answer.clone(),
        };
        self.remote_cmd(hub, thread, turn, cmd)
    }

    /// The recorded `stop` a `thread.stop` of `thread`'s live runtime on a remote box implies
    /// (`stop:<thread>:<generation>`), to be recorded with the stop request (then the
    /// session's forwarder is told its id, [`RemoteBox::expect_stop`]). `None` for a local
    /// thread.
    pub(super) fn remote_stop(&self, hub: &Inner, thread: &str) -> Option<store::RemoteCmd> {
        let view = self.registry.get(thread)?;
        hub.remote(&view.box_name)?;
        let op = Op::Stop {
            thread: thread.to_string(),
            run: None,
        };
        let op = serde_json::to_string(&op).ok()?;
        Some(store::RemoteCmd {
            box_name: view.box_name.clone(),
            cmd_id: remote::stop_cmd_id(thread, self.runtime_gen(thread)),
            thread: thread.to_string(),
            turn: view.current_turn,
            op,
        })
    }

    fn remote_cmd(
        &self,
        hub: &Inner,
        thread: &str,
        turn: Option<u64>,
        cmd: AdapterCmd,
    ) -> Option<store::RemoteCmd> {
        let box_name = &self.registry.get(thread)?.box_name;
        let mut op = hub.remote(box_name)?.session_op(thread, cmd)?;
        // An answer goes only to the run that raised its item, a prompt only to the run of the
        // hub's session (the live session's run, known from its frames): the runner refuses
        // them for any other run.
        if let Op::Respond { run, .. } | Op::Send { run, .. } = &mut op {
            *run = self
                .rt
                .get(thread)
                .and_then(|r| r.live.as_ref())
                .and_then(|l| l.run.clone());
        }
        let cmd_id = match &op {
            Op::Respond { .. } => remote::respond_cmd_id(self.runtime_gen(thread)),
            op => remote::durable_cmd_id(op)?,
        };
        let op = match serde_json::to_string(&op) {
            Ok(op) => op,
            Err(e) => {
                tracing::error!(thread, error = %e, "cannot record a command for a remote box");
                return None;
            }
        };
        Some(store::RemoteCmd {
            box_name: box_name.clone(),
            cmd_id,
            thread: thread.to_string(),
            turn,
            op,
        })
    }

    /// The recorded commands (`remote_outbox`) of box `box_name` to send again once it is
    /// ready: a `send` of a live session whose open turn is still the one it was issued in; a
    /// `respond` of the live runtime generation it answers (whatever its turn; one recorded
    /// before `respond` ids named their generation: of its turn, like a send); a stop of the
    /// live runtime it was issued for ([`Core::stop_applies`]). The others no longer apply
    /// (their turn ended, the runtime is gone or was replaced) and are forgotten.
    /// Those older than [`store::OUTBOX_RETENTION`] are never sent again (the runner may have
    /// forgotten their result, and would run them twice): the turn they were issued in, when
    /// it is still open, ends `error` saying so, in one batch with the command's retirement,
    /// and a runtime that may be left waiting is stopped ([`Core::expired_remote_cmd`]). When
    /// that cannot be recorded the command stays (unsent) and the next reconnection, after a
    /// hub restart included, tries again. A stop of a live runtime is sent whatever its age.
    fn remote_resends(&mut self, hub: &Inner, box_name: &str) -> Vec<(String, Op)> {
        self.remote_resends_of(hub, box_name, None)
    }

    /// [`Core::remote_resends`] for the recorded commands `only` names (all, when `None`).
    fn remote_resends_of(
        &mut self,
        hub: &Inner,
        box_name: &str,
        only: Option<&HashSet<String>>,
    ) -> Vec<(String, Op)> {
        let wanted = |id: &str| only.is_none_or(|o| o.contains(id));
        let retention = chrono::Duration::from_std(store::OUTBOX_RETENTION)
            .unwrap_or_else(|_| chrono::Duration::days(1));
        let expired = match self
            .store
            .expired_remote_cmds(box_name, chrono::Utc::now() - retention)
        {
            Ok(expired) => expired,
            Err(e) => {
                // Which commands expired is unknown: none is sent again.
                tracing::error!(box_name, error = %format!("{e:#}"), "cannot read the expired commands recorded for a box; sending none again");
                return Vec::new();
            }
        };
        let mut unsendable: HashSet<String> = HashSet::new();
        for c in expired.into_iter().filter(|c| wanted(&c.cmd_id)) {
            // A stop is never dropped while its runtime lives: it goes again (running a stop
            // twice is harmless), whatever its age.
            if self.stop_applies(&c) {
                continue;
            }
            if let Err(e) = self.expired_remote_cmd(hub, &c) {
                tracing::error!(box_name, cmd_id = %c.cmd_id, thread = %c.thread, error = %format!("{e:#}"), "cannot retire an expired command and end its turn (the command is never sent again; unless its turn ended, it stays for the next reconnection)");
            }
            unsendable.insert(c.cmd_id);
        }
        let rows = match self.store.remote_cmds(box_name) {
            Ok(rows) => rows,
            Err(e) => {
                tracing::error!(box_name, error = %format!("{e:#}"), "cannot read the commands recorded for a box");
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        for row in rows.into_iter().filter(|r| wanted(&r.cmd_id)) {
            // Expired: retired above, or kept for the next try, never sent.
            if unsendable.contains(&row.cmd_id) {
                continue;
            }
            let live = self.rt.get(&row.thread).is_some_and(|r| r.live.is_some());
            let turn = self.registry.get(&row.thread).and_then(|v| v.current_turn);
            let stop_applies = self.stop_applies(&row);
            // A `send` belongs to its turn. A `respond` belongs to the runtime it answers,
            // whatever turn that runtime is in now (a question between turns has none; new
            // turns wait for the answer, [`Core::thread_free`]), and never to a replacement.
            let applies = live
                && match self.respond_gen_matches(&row) {
                    Some(same_runtime) => same_runtime,
                    None => turn == row.turn,
                };
            match serde_json::from_str::<Op>(&row.op) {
                // A stop applies to the live runtime it was issued for, whatever turn it is in
                // (never to a later runtime of the thread: it names none).
                Ok(op @ Op::Stop { .. }) if stop_applies => out.push((row.cmd_id, op)),
                Ok(Op::Stop { .. }) => {
                    tracing::info!(box_name, cmd_id = %row.cmd_id, "forgetting a recorded stop of a runtime that is gone");
                    if let Err(e) = self.store.remote_cmd_done(box_name, &row.cmd_id) {
                        tracing::warn!(cmd_id = %row.cmd_id, error = %format!("{e:#}"), "cannot forget a recorded command");
                    }
                }
                Ok(op) if applies => out.push((row.cmd_id, op)),
                _ => {
                    tracing::info!(box_name, cmd_id = %row.cmd_id, "forgetting a recorded command that no longer applies");
                    if let Err(e) = self.store.remote_cmd_done(box_name, &row.cmd_id) {
                        tracing::warn!(cmd_id = %row.cmd_id, error = %format!("{e:#}"), "cannot forget a recorded command");
                    }
                }
            }
        }
        out
    }

    /// For a recorded `respond` whose id names its runtime generation
    /// ([`remote::respond_cmd_id`]): whether that is the thread's current generation. `None`
    /// for another command, or a `respond` recorded before ids named it.
    fn respond_gen_matches(&self, c: &store::RemoteCmd) -> Option<bool> {
        remote::respond_cmd_gen(&c.cmd_id).map(|g| g == self.runtime_gen(&c.thread))
    }

    /// Whether recorded command `c` is a `stop` of the thread's live runtime
    /// ([`remote::stop_cmd_id`] names its generation).
    fn stop_applies(&self, c: &store::RemoteCmd) -> bool {
        c.cmd_id.starts_with("stop:")
            && self.rt.get(&c.thread).is_some_and(|r| r.live.is_some())
            && c.cmd_id == remote::stop_cmd_id(&c.thread, self.runtime_gen(&c.thread))
    }

    /// Recorded command `c` expired unsent (see [`Core::remote_resends`]; a `stop` of a live
    /// runtime never gets here). A `stop` of a runtime that is gone is retired; a `send` or
    /// `respond` is abandoned ([`Core::abandon_remote_cmd`]): the turn it was issued in, when
    /// still open, ends `error` in the same batch as the retirement (so the turn is never left
    /// open without the record that ends it). The runtime is stopped too when it may be left
    /// waiting: always after a `respond` (it may still await the answer, and its item is
    /// resolved), after a `send` when the runtime recorded anything in that turn (it got the
    /// prompt and never finished the turn; the runner's spool is replayed by now, so a runtime
    /// with nothing recorded never got it).
    fn expired_remote_cmd(&mut self, hub: &Inner, c: &store::RemoteCmd) -> anyhow::Result<()> {
        let kind = c.cmd_id.split(':').next().unwrap_or("command");
        tracing::warn!(box_name = %c.box_name, cmd_id = %c.cmd_id, thread = %c.thread, "dropping a recorded command older than the runner keeps results");
        let waiting = match (kind, c.turn) {
            ("respond", _) => true,
            ("send", Some(turn)) => self.store.turn_has_runtime_events(&c.thread, turn)?,
            _ => false,
        };
        if !matches!(kind, "send" | "respond") {
            self.store.remote_cmd_done(&c.box_name, &c.cmd_id)?;
            return Ok(());
        }
        let why = format!(
            "twr: the {kind} for this turn never reached box {} within {} h; it is not sent again \
             (the runner may no longer remember it and would run it twice)",
            c.box_name,
            store::OUTBOX_RETENTION.as_secs() / 3600
        );
        self.abandon_remote_cmd(hub, c, why, waiting)
    }

    /// The runner answered recorded command `cmd_id` of box `box_name` with error `e` (it did
    /// not run it: its spool refused it, the session is gone, ...; or `interrupted`: its effect
    /// is unknown). Never only logged:
    /// - a `send`: the prompt did not reach the runtime. The turn ends `error` ("runner could
    ///   not deliver the prompt"), which raises its `error` item, in one batch with the
    ///   command's retirement, and the queue is held ([`super::ThreadRt::queue_held`]) until
    ///   the user acts. When the runtime may have the prompt after all (`interrupted`, or it
    ///   recorded something in that turn) it is stopped as well (below);
    /// - a `respond`: the runtime still awaits the answer, and its item is resolved, so nothing
    ///   would ever move it. In one batch: the command is retired, a durable stop of the
    ///   runtime is recorded (`stop_requests` and its `stop` command), and the open
    ///   turn ends `error` (without one, an `error` item is raised); the queue is held, and the
    ///   stop goes out (nothing is dispatched to the runtime before its exit is recorded:
    ///   [`Core::thread_free`]). A runtime the runner says is gone (`not_running`) is not
    ///   stopped: its exit closes the thread;
    /// - a `stop`: kept (the stop request stands), and sent again at the next connection.
    ///
    /// Safe to repeat: a command no longer recorded is done with.
    pub(super) fn failed_remote_cmd(
        &mut self,
        hub: &Inner,
        box_name: &str,
        cmd_id: &str,
        e: &CmdError,
    ) -> anyhow::Result<()> {
        let Some(c) = self.store.remote_cmd(box_name, cmd_id)? else {
            return Ok(());
        };
        tracing::warn!(box_name, cmd_id, thread = %c.thread, error = %e, "a recorded command failed on the runner");
        let gone = e.code == cmd_errors::NOT_RUNNING;
        let unknown = e.code == cmd_errors::INTERRUPTED;
        match c.cmd_id.split(':').next().unwrap_or_default() {
            "send" => {
                let got_it = match c.turn {
                    Some(turn) if !gone => {
                        unknown || self.store.turn_has_runtime_events(&c.thread, turn)?
                    }
                    _ => false,
                };
                let why = format!("runner could not deliver the prompt: {e}");
                self.abandon_remote_cmd(hub, &c, why, got_it)
            }
            "respond" => {
                let why = format!("runner could not deliver your answer ({e})");
                self.abandon_remote_cmd(hub, &c, why, !gone)
            }
            _ => {
                tracing::error!(box_name, cmd_id, thread = %c.thread, "the runner could not stop a runtime; the stop request stands and the stop goes again at the next connection");
                Ok(())
            }
        }
    }

    /// Gives up recorded `send` / `respond` `c` (it failed or expired, `why`): in one batch,
    /// the command is retired, the turn it was issued in ends `error` when still open, and,
    /// when the runtime `waiting` for what it never got still lives (and has moved on to no
    /// other turn), a durable stop of it is recorded (`stop_requests` and its `stop` command)
    /// together with, when no turn ends, an `error` item for the user. Then the stop goes out,
    /// the queue is held ([`super::ThreadRt::queue_held`]), and the failed turn gets its
    /// reactions (grade, report, `error` item). Nothing to end and nothing to stop: only the
    /// command goes.
    fn abandon_remote_cmd(
        &mut self,
        hub: &Inner,
        c: &store::RemoteCmd,
        why: String,
        waiting: bool,
    ) -> anyhow::Result<()> {
        let Some(view) = self.registry.get(&c.thread).cloned() else {
            self.store.remote_cmd_done(&c.box_name, &c.cmd_id)?;
            return Ok(());
        };
        let open = c.turn.filter(|t| view.current_turn == Some(*t));
        // Never a replacement of the runtime a `respond` was for.
        let stoppable = self
            .rt
            .get(&c.thread)
            .and_then(|r| r.live.as_ref())
            .is_some_and(|l| !l.stop_requested)
            && self.respond_gen_matches(c) != Some(false);
        let stop = (waiting && stoppable && (c.turn.is_none() || open.is_some()))
            .then(|| self.remote_stop(hub, &c.thread))
            .flatten();
        if open.is_none() && stop.is_none() {
            self.store.remote_cmd_done(&c.box_name, &c.cmd_id)?;
            return Ok(());
        }
        let why = match &stop {
            Some(_) => format!("{why}; the runtime was stopped — resume the thread to continue"),
            None => why,
        };
        let mut batch = Batch {
            remote_done: vec![(c.box_name.clone(), c.cmd_id.clone())],
            ..Batch::default()
        };
        if let Some(s) = &stop {
            batch.remote_cmds.push(s.clone());
            batch.stop_requests.push(c.thread.clone());
        }
        // Held before anything is recorded, so nothing the failure's reactions dispatch takes
        // the queue.
        self.rt(&c.thread).queue_held = true;
        let e = anyhow::anyhow!(why);
        match open {
            Some(turn) => self.end_failed_turn(hub, &c.thread, turn, view.session_id, &e, batch)?,
            None => {
                batch.push(
                    &c.thread,
                    None,
                    Source::Hub,
                    EventBody::AttentionRaised(AttentionRaised {
                        attn_id: new_id(),
                        reason: AttentionReason::Error,
                        summary: format!("{}: {e}", view.name),
                        options: vec!["dismiss".into()],
                        ..Default::default()
                    }),
                );
                self.append_batch(hub, batch)?;
            }
        }
        if let Some(s) = &stop {
            self.stop_recorded(hub, &c.thread, s);
        }
        match open {
            Some(turn) => self.react_to_failed_turn(hub, &c.thread, turn),
            None => Ok(()),
        }
    }

    /// Tells `thread`'s live runtime to stop once its stop (`stop`, [`Core::remote_stop`]) is
    /// recorded, as `thread.stop` does: its exit closes the thread as stopped, and until then
    /// nothing is dispatched to it ([`Core::thread_free`]).
    fn stop_recorded(&mut self, hub: &Inner, thread: &str, stop: &store::RemoteCmd) {
        if let Some(rbox) = hub.remote(&stop.box_name) {
            rbox.expect_stop(thread, &stop.cmd_id);
        }
        let rt = self.rt(thread);
        rt.stop_requested = true;
        if let Some(live) = rt.live.as_mut() {
            live.stop_requested = true;
            let _ = live.handle.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    /// What a runner's inventory says of itself is checked before the hub shows or matches
    /// it: a version too long, empty or not printable is `unreadable`, a build that is no build
    /// id none; each says so.
    #[test]
    fn an_inventory_s_version_and_build_are_checked() {
        use twr_core::runner::Inventory;
        let inv = |version: &str, build: Option<&str>| Inventory {
            version: version.into(),
            build: build.map(str::to_string),
            ..Default::default()
        };
        let (ok, odd) = super::checked_inventory(inv("0.2.0+dev.abc", Some("0123456789abcdef")));
        assert_eq!(
            (ok.version.as_str(), ok.build.as_deref(), odd.len()),
            ("0.2.0+dev.abc", Some("0123456789abcdef"), 0)
        );
        for bad in ["", "0.2.0\n\x1b[31m", &"9".repeat(super::VERSION_MAX + 1)] {
            let (c, odd) = super::checked_inventory(inv(bad, None));
            assert_eq!(c.version, "unreadable", "{bad:?}");
            assert_eq!(odd.len(), 1, "{bad:?}");
        }
        for bad in ["../../x", "0123456789ABCDEF", "0123"] {
            let (c, odd) = super::checked_inventory(inv("0.2.0", Some(bad)));
            assert_eq!(c.build, None, "{bad}");
            assert!(odd[0].contains("no build id"), "{odd:?}");
        }
    }

    use std::cell::RefCell;

    use twr_core::config::{BoxConfig, Config};

    use super::*;
    use crate::hub::HubOptions;

    thread_local! {
        /// Runs once, between a connection's `disconnect` and its box going offline.
        static AFTER_DISCONNECT: RefCell<Option<Box<dyn FnOnce()>>> = RefCell::new(None);
    }

    /// The hook [`connection_ended`] calls right after its `disconnect` (see
    /// [`AFTER_DISCONNECT`]).
    pub(super) fn after_disconnect() {
        if let Some(f) = AFTER_DISCONNECT.with(|h| h.borrow_mut().take()) {
            f();
        }
    }

    /// A connection of `rbox` that goes online: what [`serve`] and [`reconcile`] do, under the
    /// core lock.
    fn go_online(hub: &Inner, rbox: &RemoteBox) -> u64 {
        let mut core = hub.core();
        let (tx, _) = mpsc::channel(8);
        let (close, _) = watch::channel(None);
        let epoch = rbox
            .connect(&Inventory::default(), ConnTx::new(tx, close))
            .unwrap();
        assert!(rbox.set_ready(epoch, &[], Vec::new()));
        core.online_boxes.insert(rbox.name().to_string());
        epoch
    }

    /// The end of a connection and its box going offline are one step under the
    /// core lock. A replacement that connects right after the old connection's `disconnect`
    /// waits for that step, and stays online.
    #[tokio::test]
    async fn the_end_of_a_replaced_connection_never_takes_the_replacement_offline() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.boxes.insert(
            "gpu".into(),
            BoxConfig {
                token: Some("gpu-token-0123456789abcdef".into()),
                ..Default::default()
            },
        );
        let hub = crate::Hub::open(HubOptions::new(dir.path().join("twr.db"), cfg))
            .await
            .unwrap();
        let inner = Arc::clone(&hub.inner);
        let rbox = inner.remote("gpu").unwrap();
        let first = go_online(&inner, &rbox);

        // Right after the first connection's `disconnect`, a replacement connects and goes
        // online (on another thread: the core lock may be held here), with every chance to
        // finish before the first connection's end goes on.
        let (tx, rx) = std::sync::mpsc::channel();
        let (inner2, rbox2) = (Arc::clone(&inner), rbox.clone());
        AFTER_DISCONNECT.with(|h| {
            *h.borrow_mut() = Some(Box::new(move || {
                std::thread::spawn(move || {
                    let _ = tx.send(go_online(&inner2, &rbox2));
                });
                std::thread::sleep(Duration::from_millis(300));
            }))
        });
        connection_ended(&inner, &rbox, first, true, "v1");
        let second = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(rbox.is_current(second));
        assert!(
            inner.core().online_boxes.contains("gpu"),
            "the live replacement was taken offline"
        );
        // The first connection's end was recorded once; ending it again changes nothing.
        connection_ended(&inner, &rbox, first, true, "v1");
        assert!(inner.core().online_boxes.contains("gpu"));
        let (items, _) = hub.control_read(0, 1000).unwrap();
        let ends = items
            .iter()
            .filter(|(_, e)| matches!(e.body, EventBody::RunnerDisconnected(_)))
            .count();
        assert_eq!(ends, 1);
    }

    /// A duplicate `turn.ended` frame gets the reactions its turn may be missing only when the
    /// log's end of that turn is the box's runtime's: never for an end the hub recorded itself
    /// (a lost or crashed runtime's, which holds the queue).
    #[tokio::test]
    async fn a_replayed_turn_end_reacts_only_to_ends_the_runtime_reported() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::default();
        cfg.boxes.insert(
            "gpu".into(),
            BoxConfig {
                token: Some("gpu-token-0123456789abcdef".into()),
                ..Default::default()
            },
        );
        let hub = crate::Hub::open(HubOptions::new(dir.path().join("twr.db"), cfg))
            .await
            .unwrap();
        let inner = Arc::clone(&hub.inner);
        let t = "01JTESTTHREAD0000000000000";
        let mut core = inner.core();
        let created = EventBody::ThreadCreated(ThreadCreated {
            name: "remote".into(),
            box_name: "gpu".into(),
            runtime: "fake".into(),
            mode: "auto".into(),
            ..Default::default()
        });
        core.append(&inner, t, None, Source::Hub, created).unwrap();
        let ended = |result| {
            EventBody::TurnEnded(TurnEnded {
                result,
                ..Default::default()
            })
        };
        let turn = |core: &mut Core, n: u64, by: Source| {
            let started = EventBody::TurnStarted(TurnStarted {
                prompt: "p".into(),
                ..Default::default()
            });
            let user = Source::User("test".into());
            core.append(&inner, t, Some(n), user, started).unwrap();
            core.append(&inner, t, Some(n), by, ended(TurnResult::Interrupted))
                .unwrap();
        };
        // Turn 1 ended by the hub (its runtime was lost): no reaction for a replayed end.
        turn(&mut core, 1, Source::Hub);
        let replayed = ended(TurnResult::Completed);
        assert!(core
            .missed_turn_end(t, &replayed, Some(1))
            .unwrap()
            .is_none());
        // Turn 2 ended by the runtime: its reactions may be missing (the latest turn).
        turn(&mut core, 2, Source::Adapter("fake".into()));
        let missed = core.missed_turn_end(t, &replayed, Some(2)).unwrap();
        assert_eq!(missed.map(|m| m.turn), Some(2));
    }
}
