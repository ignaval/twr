//! Requests from agents. Runtimes on the hub's machine run as the user's
//! own OS user, so being able to connect to the hub socket does not tell the user's terminal
//! from an agent's shell. When a connection opens, the server reads its peer's pid
//! (`SO_PEERCRED` on Linux, `LOCAL_PEEREPID` on macOS, through tokio's `peer_cred`) and asks
//! [`Hub::agent_peer`], which decides with two signals:
//!
//! 1. Ancestry ([`reap::ancestry`], walked to init with no fixed depth that grants anything):
//!    a peer that is, or descends from, a process twr started for a thread is that thread's
//!    agent ([`AgentWhy::Started`]). Processes are matched by pid and start time, so a
//!    recorded pid that now names another process matches nothing; a runtime of this hub
//!    whose start time could not be read is matched by pid alone while it runs, and
//!    so is a runtime this hub has just started, from the moment its adapter's spawn returns
//!    until its `thread.started` / `thread.resumed` records it.
//! 2. The agent marker: every process twr starts for a thread on this machine
//!    carries [`AGENT_ENV`] in its environment, and its descendants inherit it, also once
//!    they have left their parent (`nohup cmd &` from a shell that then exits: reparented to
//!    init, the ancestry no longer shows where it came from). A peer whose ancestry matched
//!    nothing but which, or any process of whose ancestry, carries the variable, with any
//!    value, is an agent ([`AgentWhy::Marker`]; the thread the nearest carrier names, when it
//!    names one): a process that cleared the variable still descends from one that has it.
//!
//! An environment that cannot be read is not one without the marker: a process of
//! the user's own can hide its `/proc/<pid>/environ` from that user (`PR_SET_DUMPABLE` 0). So
//! when the walk is whole and matched nothing, no readable environment carries the marker, and
//! the peer's own environment cannot be read (it is not gone), or that of an ancestor run by
//! the hub's user, the peer is an agent of no known thread ([`AgentWhy::Hidden`]). An
//! ancestor's that the agent cannot have hidden does not count: another user's (root's `sshd`
//! or `login`, a setuid or setgid program), one that started before the hub (the user's
//! `systemd --user`, which is not dumpable), or a process of the user whose parent is another
//! user's (`sshd`'s per-connection process, which switched to the user without exec'ing). See
//! [`reap::first_agent_marker`]. So a hidden ancestor older than the hub (left running by an
//! agent of an earlier hub) or started by a privileged daemon is not noticed, and
//! on macOS (`ps`) a process with an empty environment counts as hidden.
//!
//! An agent's connection may call only the server's `AGENT_METHODS`: reads, and starting and
//! cancelling its own thread's jobs (`hub::jobs`) when it is, or descends from, a process twr
//! started for that thread ([`AgentWhy::Started`]); starting only while the thread's mode may
//! run commands.
//! A connection whose peer the server cannot identify at all (its credentials cannot be read,
//! or name no pid this hub can see) is an agent of no known thread
//! ([`AgentWhy::Unidentified`]), except the hub's own process (its in-process clients).
//!
//! Classification only restricts: the orchestrator token is accepted only from a
//! peer whose ancestry contains the current orchestrator runtime's process
//! ([`AgentPeer::descends_from_runtime`]). A marker naming the orchestrator's thread, an
//! unresolved walk or no classification at all never lets a connection present it. A started
//! process is matched as one runtime of its thread ([`Started::generation`]), and the server
//! walks the peer's ancestry again each time the token is presented or used: a
//! process of an earlier runtime of the orchestrator's thread (one that outlived it, on the
//! connection it opened back then) does not descend from the current one. Each walk also reads
//! which process the peer is ([`PeerWalk::process`]): the token needs the process that opened
//! the connection.
//!
//! What twr started is what the hub already records about each thread: a runtime of the hub's
//! own box (worker or orchestrator) from its `thread.started` / `thread.resumed` `ext.twr.pid`
//! and `ext.twr.start_time` until the log closes that process, and the interactive process of
//! an attach once its client reports it (`attach.child`; for a remote box that is the local
//! `ssh`). Runtimes on remote boxes run on other machines and cannot reach the socket.
//!
//! A walk that cannot be completed does not make the peer the user: a loop, a chain
//! deeper than [`reap::ANCESTRY_DEPTH`], a parent that keeps exiting while the chain is read
//! (retried first), a peer that is gone before it is read (only a process that inherited
//! the connection can still use it), or a peer whose own record cannot be read or parsed while
//! the process table can (a command name that is not UTF-8 is read like any other)
//! make it an agent of no known thread ([`AgentWhy::Unresolved`]) unless its marker names one.
//! The environments are read after the ancestry, so the walk is checked against them: a peer
//! that has exited by the time they have been read (gone, a zombie, or its pid another
//! process's), or a process of the chain whose environment could not be read because it had
//! exited, is not one without the marker. A marked process can connect, hand
//! the connection to a child and exit in between. Such a peer is an agent of no known thread
//! too; an ancestor that exited makes the hub walk the ancestry again first.
//!
//! An ancestor the process table does not show this user ([`AncestryEnd::Concealed`]:
//! its record cannot be read, or it is not there while its child still names it) ends the
//! chain the way init does. procfs mounted with `hidepid=1` or `hidepid=2` hides every process
//! the user may not trace: root's `sshd` and `login`, and `sshd`'s per-connection process of
//! the user (not dumpable), so the user's own terminal reaches one. Agents descend from the
//! processes twr started, which run as the hub's user and are matched on the way up; a chain
//! that reaches a hidden ancestor without passing one of them or a process that carries the
//! marker is decided by the environments that were read, like a whole chain (the rule
//! for an environment that cannot be read included). Unless the hidden ancestor has the pid of
//! a process twr started (a runtime that made itself not dumpable): then the peer is an agent
//! of no known thread. With `hidepid` a process of the user's that is not dumpable is hidden
//! too, so a process an agent starts without the marker (exec'd without it) below one it made
//! not dumpable escapes, still under its runtime: without `hidepid` it would also have to
//! leave its parent (Limits).
//!
//! Limits. A process that both clears its environment (execs again without [`AGENT_ENV`]:
//! Linux and macOS show the environment a process was exec'd with, so unsetting the variable
//! is not enough) and leaves its parent (a background job whose shell exited, a double fork)
//! escapes both checks. So does a process started before its runtime was recorded and
//! without the marker: a runtime reports its process as it starts, before its first turn runs
//! a tool, and the hub knows it from its adapter's spawn on; an attach's client
//! reports its CLI right after starting it, and until then the CLI's marker covers what it
//! starts. So does a command an agent has a daemon of the user's that already runs start for
//! it: `tmux new-window 'twr ...'` against the user's tmux server, `screen -X`,
//! `systemd-run --user`, an IDE's or editor's remote server, `at` or `cron`. That command
//! descends from the daemon and has the daemon's environment, so its ancestry and environment
//! are the user's: classification by process ancestry cannot tell it from the user's own, the
//! same exposure as running the agent with no twr at all. For a repository you do not trust,
//! run the agent in its runtime's sandbox, or as a separate OS user (or in a container, a
//! runner box). A process table that cannot be read at all (`ps` failing on macOS, or no
//! `/proc` on Linux) leaves the peer the user (logged), unless its environment can
//! still be read and carries the marker. A read that fails in the hub itself (its task
//! panicked or was cancelled) is not that: the peer is an agent of no known thread.

use std::collections::HashMap;

use twr_runner::reap::{self, AncestryEnd, ChainMarker, ProcessId, TwrParent, AGENT_ENV};

use super::{Hub, LOCAL_BOX};

/// What twr started for a thread on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartedKind {
    /// The thread's runtime (a worker's or the orchestrator's).
    Runtime,
    /// The interactive process of an attach (`twr attach`).
    Attach,
}

/// A process twr started for a thread on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    /// Thread id.
    pub thread: String,
    /// Thread name.
    pub thread_name: String,
    /// What the process is.
    pub kind: StartedKind,
    /// For a runtime this hub runs, the generation of that runtime: which start of the
    /// thread's runtime the process is (a later one, after a restart, has another). `None` for
    /// an attach's process and for a runtime the log shows running that this hub does not run.
    pub generation: Option<u64>,
}

/// One runtime this hub runs: its thread and the generation of that start of it
/// ([`Hub::orchestrator_token_runtime`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRef {
    /// Thread id.
    pub thread: String,
    /// The runtime's generation.
    pub generation: u64,
}

/// The processes twr started for threads on this machine ([`Hub::started_processes`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StartedTable {
    /// By pid and start time: each runtime of the hub's own box the log shows running with
    /// both recorded, and each attach's reported interactive process.
    pub exact: HashMap<ProcessId, Started>,
    /// By pid alone: each runtime this hub runs whose start time could not be read, while it
    /// runs; and the process each adapter of this hub reported starting
    /// ([`twr_core::adapter::AdapterEnd::spawned`]) while that runtime lives, unless
    /// it is in `exact` already (its `thread.started` recorded it with a start time). Each is
    /// the hub's own child, so its pid names no other process until the hub has seen it exit.
    pub by_pid: HashMap<u32, Started>,
}

impl StartedTable {
    /// Whether no process is recorded.
    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.by_pid.is_empty()
    }

    /// What process `id` was started for, if it is in the table (by pid and start time, or by
    /// pid alone).
    pub fn lookup(&self, id: &ProcessId) -> Option<&Started> {
        self.exact.get(id).or_else(|| self.by_pid.get(&id.pid))
    }

    /// Whether a process with pid `pid` is in the table (whatever its start time).
    fn has_pid(&self, pid: u32) -> bool {
        self.by_pid.contains_key(&pid) || self.exact.keys().any(|id| id.pid == pid)
    }

    /// The first process of `chain` (nearest first) that is in the table, with what it was
    /// started for.
    fn find<'a>(&'a self, chain: &'a [ProcessId]) -> Option<(&'a ProcessId, &'a Started)> {
        chain.iter().find_map(|id| {
            self.exact
                .get(id)
                .or_else(|| self.by_pid.get(&id.pid))
                .map(|s| (id, s))
        })
    }
}

/// Why a connection's peer counts as a thread's agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentWhy {
    /// It is, or descends from, process `pid`, which twr started for the thread.
    Started {
        /// The process twr started.
        pid: u32,
        /// Its start time, as the walk read it: with `pid`, the exact process.
        start_time: u64,
        /// The runtime generation it was matched as ([`Started::generation`]).
        generation: Option<u64>,
        /// What that process is.
        kind: StartedKind,
    },
    /// Its environment, or that of process `pid` of its ancestry, carries [`AGENT_ENV`]
    /// (inherited from a process twr started for a thread).
    Marker {
        /// The nearest process of the chain that carries it (the peer itself, or an ancestor).
        pid: u32,
    },
    /// Its ancestry could not be read to the end (it ended this way), so it may descend from
    /// a process twr started, and it carries no marker.
    Unresolved(AncestryEnd),
    /// Its ancestry is whole (or ends at an ancestor the process table does not show) and
    /// matched nothing, and no environment of it that could be read carries the marker, but
    /// the environment of process `pid` could not be read: the peer
    /// itself, or an ancestor run by the hub's user that started after the hub (a process
    /// that made itself not dumpable hides its environment from its own user).
    Hidden {
        /// The nearest such process (the peer, or an ancestor).
        pid: u32,
    },
    /// Nothing else made it an agent, but process `pid` of its ancestry is a twr hub or a
    /// `twr attach` (`parent`): this hub's, or another workspace's or instance's, whose
    /// runtimes and attaches this hub does not know (and whose hidden environments may be
    /// older than this hub). The user's terminal descends from neither; what a hub starts,
    /// and the agent's CLI an attach runs, do.
    Under {
        /// The nearest such ancestor.
        pid: u32,
        /// What it is.
        parent: TwrParent,
    },
    /// The connection's peer is no process this hub can identify: its credentials could not
    /// be read, or name no pid this hub can see (none, or 0: a process of another pid
    /// namespace). Not the user: the user's own clients connect from processes the hub sees.
    Unidentified,
}

/// What walking a connection's peer found ([`Hub::agent_peer`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PeerWalk {
    /// The peer itself as the walk read it (its pid and start time): which process it is.
    /// `None` when it could not be read (gone, or the process table unreadable).
    pub process: Option<ProcessId>,
    /// The thread whose agent the peer is; `None`: the user.
    pub agent: Option<AgentPeer>,
}

/// A connection's peer that counts as a thread's agent ([`Hub::agent_peer`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPeer {
    /// Thread id, when known: the thread of the process it descends from, or the one its
    /// marker names.
    pub thread: Option<String>,
    /// Thread name as it was when the connection opened (the marker's thread id for a thread
    /// this hub does not know, `unknown` without a thread).
    pub thread_name: String,
    /// Why it counts as an agent.
    pub why: AgentWhy,
    /// The connection's peer.
    pub peer: u32,
}

impl AgentPeer {
    /// A peer twr cannot trace (its ancestry ended with `end` before it was whole): an agent
    /// of no known thread.
    pub fn untraced(peer: u32, end: AncestryEnd) -> AgentPeer {
        AgentPeer {
            thread: None,
            thread_name: "unknown".to_string(),
            why: AgentWhy::Unresolved(end),
            peer,
        }
    }

    /// A connection's peer this hub cannot identify ([`AgentWhy::Unidentified`]): an agent of
    /// no known thread, with no pid (`peer` 0).
    pub fn unidentified() -> AgentPeer {
        AgentPeer {
            thread: None,
            thread_name: "unknown".to_string(),
            why: AgentWhy::Unidentified,
            peer: 0,
        }
    }

    /// The process named in messages: the one twr started, else the peer itself.
    pub fn pid(&self) -> u32 {
        match self.why {
            AgentWhy::Started { pid, .. } => pid,
            AgentWhy::Marker { .. }
            | AgentWhy::Unresolved(_)
            | AgentWhy::Hidden { .. }
            | AgentWhy::Under { .. }
            | AgentWhy::Unidentified => self.peer,
        }
    }

    /// Whether the peer is, or descends from, the process of runtime `rt` (that start of its
    /// thread's runtime), as the hub recorded it when the peer's ancestry was walked (pid and
    /// start time, or the pid its adapter reported starting): the only proof of where a
    /// connection comes from that the orchestrator token is accepted with. A
    /// marker naming its thread is not, and neither is a process of another runtime of the
    /// same thread, an earlier one that outlived its restart.
    pub fn descends_from_runtime(&self, rt: &RuntimeRef) -> bool {
        matches!(
            self.why,
            AgentWhy::Started {
                kind: StartedKind::Runtime,
                generation: Some(g),
                ..
            } if g == rt.generation
        ) && self.thread.as_deref() == Some(rt.thread.as_str())
    }

    /// Who the peer is, briefly: "w1's agent (pid N)".
    pub fn summary(&self) -> String {
        match self.why {
            AgentWhy::Unresolved(_) | AgentWhy::Hidden { .. } => {
                format!("a process twr cannot trace (pid {})", self.peer)
            }
            AgentWhy::Under { .. } => format!("an agent's process (pid {})", self.peer),
            AgentWhy::Unidentified => "a process twr cannot identify".to_string(),
            _ => format!("{}'s agent (pid {})", self.thread_name, self.pid()),
        }
    }

    /// Who the peer is and why it counts as an agent.
    pub fn detail(&self) -> String {
        match self.why {
            AgentWhy::Started { pid, .. } => format!(
                "{}'s agent (pid {pid}, a process twr started for it)",
                self.thread_name
            ),
            AgentWhy::Marker { pid } if pid == self.peer => format!(
                "{}'s agent (pid {}, which carries {AGENT_ENV}: twr started it, or what it \
                 descends from, for that thread)",
                self.thread_name, self.peer
            ),
            AgentWhy::Marker { pid } => format!(
                "{}'s agent (pid {}, which descends from pid {pid}, which carries {AGENT_ENV}: \
                 twr started that process, or what it descends from, for that thread)",
                self.thread_name, self.peer
            ),
            AgentWhy::Unresolved(_) => format!(
                "a process twr cannot trace (pid {}: its ancestry could not be read to the \
                 end), which counts as an agent",
                self.peer
            ),
            AgentWhy::Hidden { pid } if pid == self.peer => format!(
                "a process twr cannot trace (pid {}: its environment, which would show \
                 {AGENT_ENV}, cannot be read), which counts as an agent",
                self.peer
            ),
            AgentWhy::Hidden { pid } => format!(
                "a process twr cannot trace (pid {}: the environment of pid {pid}, of its \
                 ancestry, which would show {AGENT_ENV}, cannot be read), which counts as an \
                 agent",
                self.peer
            ),
            AgentWhy::Under { pid, parent } => format!(
                "a process that descends from {} (pid {}, under pid {pid}: maybe another \
                 workspace's), which counts as an agent",
                match parent {
                    TwrParent::Hub => "a twr hub",
                    TwrParent::Attach => "a `twr attach`",
                },
                self.peer
            ),
            AgentWhy::Unidentified => "a process twr cannot identify (the connection's peer \
                 credentials name no process this hub can see), which counts as an agent"
                .to_string(),
        }
    }
}

/// How many times [`settled_ancestry`] walks a peer's ancestry while a parent exits under it.
const WALK_TRIES: usize = 3;

/// The ancestry of `peer`, walked again (up to [`WALK_TRIES`] times) while a parent exited
/// during the walk (the peer was being reparented). Blocks.
fn settled_ancestry(peer: u32) -> reap::Ancestry {
    let mut a = reap::ancestry(peer);
    for _ in 1..WALK_TRIES {
        let racing = match a.end {
            AncestryEnd::Reused => true,
            AncestryEnd::Gone => !a.chain.is_empty(),
            _ => false,
        };
        if !racing {
            break;
        }
        a = reap::ancestry(peer);
    }
    a
}

impl Hub {
    /// Whether `peer` (a socket connection's peer) is a thread's agent, and whose (see the
    /// module docs): the nearest process of its ancestry, itself included, that twr started
    /// for a thread ([`Hub::started_processes`]); else the nearest [`AGENT_ENV`] marker of
    /// its ancestry, itself first; else, when its ancestry could not be read to the end, an
    /// agent of no known thread. `None` (the user) for anything else. Also says which process
    /// the peer was when it was read ([`PeerWalk::process`]: the walk's first process), so a
    /// caller can tell whether its pid still names the process that connected.
    /// Reads the process table off the async runtime.
    ///
    /// The environments are read after the ancestry, so the walk is checked against them: a
    /// peer that is no longer the process the walk read once they have been read (it exited,
    /// maybe handing the connection to a child), or whose environment could not be read
    /// because it had exited, is an agent of no known thread; an ancestor
    /// that had exited walks the ancestry again (up to [`WALK_TRIES`] times in all, then the
    /// same).
    ///
    /// A read that fails as a task (it panicked, or the runtime is shutting down) is no
    /// process table that cannot be read: the peer is an agent of no known thread
    /// ([`AgentWhy::Unresolved`] with [`AncestryEnd::Unreadable`]).
    pub async fn agent_peer(&self, peer: u32) -> PeerWalk {
        let mut walks = 1;
        loop {
            let Some(ancestry) = self.peer_ancestry(peer).await else {
                return PeerWalk {
                    process: None,
                    agent: Some(AgentPeer::untraced(peer, AncestryEnd::Unreadable)),
                };
            };
            let process = ancestry.chain.first().copied().filter(|p| p.pid == peer);
            let rewalk = walks < WALK_TRIES;
            match self.classify(peer, &ancestry, rewalk).await {
                Decision::Agent(agent) => return PeerWalk { process, agent },
                Decision::Rewalk => walks += 1,
            }
        }
    }

    /// The ancestry of `peer`, settled ([`settled_ancestry`]), read off the async runtime;
    /// `None` when the task reading it failed.
    async fn peer_ancestry(&self, peer: u32) -> Option<reap::Ancestry> {
        match tokio::task::spawn_blocking(move || settled_ancestry(peer)).await {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::warn!(peer, error = %e, "cannot read the ancestry of a connection's peer; it counts as an agent");
                None
            }
        }
    }

    /// What `peer`, with `ancestry`, is ([`Hub::agent_peer`]); `rewalk`: whether an ancestor
    /// that exited may have the ancestry walked again ([`decide`]). A failed read of the
    /// environments makes it an agent of no known thread.
    async fn classify(&self, peer: u32, ancestry: &reap::Ancestry, rewalk: bool) -> Decision {
        let started = self.started_processes();
        if let Some(a) = started_agent(peer, ancestry, &started) {
            return Decision::Agent(Some(a));
        }
        let hook = self.inner.marker_read_hook.clone();
        let walked = ancestry.clone();
        let read = tokio::task::spawn_blocking(move || {
            if let Some(hook) = hook {
                hook(peer);
            }
            let (marker, left) = read_markers(peer, &walked);
            (marker, left, twr_ancestor(&walked, reap::process_args))
        });
        let (marker, left, hub) = match read.await {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(peer, error = %e, "cannot read the environments of a connection's peer and its ancestors; it counts as an agent");
                return Decision::Agent(Some(AgentPeer::untraced(peer, AncestryEnd::Unreadable)));
            }
        };
        let core = self.inner.core();
        let decision = decide(peer, ancestry, marker, left, rewalk, &started, |id| {
            core.registry.get(id).map(|v| v.name.clone())
        });
        under_hub(decision, peer, hub)
    }

    /// The processes twr started for threads on this machine: each runtime of the hub's own
    /// box the log shows running, by pid and start time (by pid alone while this hub runs it
    /// when its start time could not be read), each runtime this hub runs by the pid its
    /// adapter reported starting until the log records it with a start time, and each
    /// attach's reported interactive process. A runtime this hub runs carries its generation.
    pub fn started_processes(&self) -> StartedTable {
        self.inner.core().started_table()
    }

    /// Whether `peer` still descends from `started` (a process twr started, pid and start
    /// time, that its walk matched), as a fresh walk confirmed link by link shows: the ancestry
    /// is walked again and must reach `started`, then each process of it up to `started` is
    /// read once more ([`reap::chain_holds`]): still running, the same start time, its parent
    /// still the next one. A process that exits while the ancestry is walked stays readable
    /// as a zombie with its old parent until it is reaped, while its children have a new
    /// parent at once: reading the chain again after the walk sees that. Reads the process
    /// table off the async runtime; `false` when that fails.
    pub async fn confirm_descent(&self, peer: u32, started: ProcessId) -> bool {
        let hook = self.inner.peer_confirm_hook.clone();
        tokio::task::spawn_blocking(move || {
            let a = settled_ancestry(peer);
            let Some(i) = a.chain.iter().position(|p| *p == started) else {
                return false;
            };
            if let Some(hook) = hook {
                hook(peer);
            }
            reap::chain_holds(&a.chain[..=i])
        })
        .await
        .unwrap_or(false)
    }
}

impl super::Core {
    /// [`Hub::started_processes`], under the hub's lock.
    pub(super) fn started_table(&self) -> StartedTable {
        let core = self;
        let mut out = StartedTable::default();
        for (id, rt) in &core.rt {
            let Some(view) = core.registry.get(id) else {
                continue;
            };
            let generation = rt.live.as_ref().map(|l| l.generation);
            let started = |kind| Started {
                thread: id.clone(),
                thread_name: view.name.clone(),
                kind,
                generation: generation.filter(|_| kind == StartedKind::Runtime),
            };
            match (view.box_name == LOCAL_BOX, rt.pid, rt.start_time) {
                (true, Some(pid), Some(start_time)) => {
                    out.exact
                        .insert(ProcessId { pid, start_time }, started(StartedKind::Runtime));
                }
                // The hub knows when its own runtime exits (`process_live` is cleared then).
                (true, Some(pid), None) if rt.process_live && rt.live.is_some() => {
                    out.by_pid.insert(pid, started(StartedKind::Runtime));
                }
                _ => {}
            }
            // As soon as its adapter's spawn returned: before the runtime reports its
            // process (`thread.started`, which may come after it has started MCP servers or
            // hooks) and before its start time is read.
            let spawned = rt.live.as_ref().and_then(|l| l.handle.pid());
            if let (true, Some(pid)) = (view.box_name == LOCAL_BOX, spawned) {
                let recorded = rt.process_live && rt.pid == Some(pid) && rt.start_time.is_some();
                if !recorded {
                    out.by_pid.insert(pid, started(StartedKind::Runtime));
                }
            }
            if let Some(c) = rt.attach.as_ref().and_then(|a| a.child.as_ref()) {
                let id = ProcessId {
                    pid: c.pid,
                    start_time: c.start_time,
                };
                out.exact.insert(id, started(StartedKind::Attach));
            }
        }
        out
    }
}

/// The peer as the agent of the nearest process of its ancestry that twr started for a thread
/// (`started`), if any ([`Hub::agent_peer`]).
fn started_agent(
    peer: u32,
    ancestry: &reap::Ancestry,
    started: &StartedTable,
) -> Option<AgentPeer> {
    let (id, s) = started.find(&ancestry.chain)?;
    Some(AgentPeer {
        thread: Some(s.thread.clone()),
        thread_name: s.thread_name.clone(),
        why: AgentWhy::Started {
            pid: id.pid,
            start_time: id.start_time,
            generation: s.generation,
            kind: s.kind,
        },
        peer,
    })
}

/// The nearest of `ancestry`'s processes above the peer that is a twr hub or a `twr attach`
/// ([`reap::twr_parent`] on its arguments, read with `args_of`), if any. Blocks.
fn twr_ancestor(
    ancestry: &reap::Ancestry,
    args_of: impl Fn(u32) -> Option<Vec<String>>,
) -> Option<(u32, TwrParent)> {
    ancestry
        .chain
        .iter()
        .skip(1)
        .find_map(|p| Some((p.pid, reap::twr_parent(&args_of(p.pid)?)?)))
}

/// `decision`, unless it makes `peer` the user while it descends from a twr hub or attach
/// (`under`, the nearest such ancestor): then an agent of no known thread
/// ([`AgentWhy::Under`]). Only restricts: an agent stays the agent it is.
fn under_hub(decision: Decision, peer: u32, under: Option<(u32, TwrParent)>) -> Decision {
    match (decision, under) {
        (Decision::Agent(None), Some((pid, parent))) => {
            tracing::warn!(
                peer,
                ancestor = pid,
                ?parent,
                "a connection's peer descends from a twr hub or attach; it counts as an agent"
            );
            Decision::Agent(Some(AgentPeer {
                thread: None,
                thread_name: "unknown".to_string(),
                why: AgentWhy::Under { pid, parent },
                peer,
            }))
        }
        (d, _) => d,
    }
}

/// Whether this machine's process `pid` is a thread's agent as far as can be told without a
/// hub's record of what it started: for a command that changes twr's files without asking a
/// hub (`twr ws new`, `twr ws rm`), about its own process. An [`AGENT_ENV`] marker along its
/// ancestry, an environment it cannot read there, a walk it cannot finish, or a twr hub or
/// attach among its ancestors makes it one ([`unmatched`], [`AgentWhy::Under`]). Blocks
/// (reads the process table).
pub fn local_agent(pid: u32) -> Option<AgentPeer> {
    let ancestry = settled_ancestry(pid);
    let (marker, _) = read_markers(pid, &ancestry);
    let under = twr_ancestor(&ancestry, reap::process_args);
    let agent = unmatched(pid, &ancestry, marker, &StartedTable::default(), |_| None);
    match under_hub(Decision::Agent(agent), pid, under) {
        Decision::Agent(a) => a,
        Decision::Rewalk => None,
    }
}

/// The [`AGENT_ENV`] marker along `peer`'s `ancestry` ([`reap::first_agent_marker`]), and
/// whether the peer, as the walk read it, is no longer that process once the environments have
/// been read (gone, a zombie, or another process with its pid). A peer the
/// walk did not read, or a process table that cannot be read, is not checked. Blocks.
fn read_markers(peer: u32, ancestry: &reap::Ancestry) -> (ChainMarker, bool) {
    let marker = reap::first_agent_marker(&marker_pids(peer, ancestry));
    let left = ancestry
        .chain
        .first()
        .filter(|p| p.pid == peer)
        .is_some_and(|p| reap::process_liveness(p.pid, Some(p.start_time)) == reap::Alive::No);
    (marker, left)
}

/// What [`Hub::agent_peer`] makes of one walk of a peer's ancestry.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// The peer is this thread's agent (`None`: the user).
    Agent(Option<AgentPeer>),
    /// An ancestor had exited when its environment was read: walk the ancestry again.
    Rewalk,
}

/// What a peer whose ancestry matched no process twr started is, given what its environments
/// showed (`marker`) and whether it had left (`left`: it is no longer the process the walk
/// read once they were read; [`read_markers`]). A marker decides as always
/// (it only restricts). Else a peer that left, or whose environment could not be read because
/// it had exited, is an agent of no known thread: the connection may be in the hands of a
/// process it handed it to, and what the walk read is not its ancestry. An ancestor whose
/// environment could not be read because it had exited makes the ancestry be walked again
/// while `rewalk`, and is then an agent of no known thread. Otherwise [`unmatched`].
fn decide(
    peer: u32,
    ancestry: &reap::Ancestry,
    marker: ChainMarker,
    left: bool,
    rewalk: bool,
    started: &StartedTable,
    name_of: impl Fn(&str) -> Option<String>,
) -> Decision {
    match marker {
        ChainMarker::Carried { .. } => {}
        _ if left => {
            tracing::warn!(
                peer,
                "a connection's peer exited while it was classified; it counts as an agent"
            );
            return Decision::Agent(Some(AgentPeer::untraced(peer, AncestryEnd::Gone)));
        }
        ChainMarker::Gone { pid } if pid != peer && rewalk => {
            tracing::debug!(
                peer,
                ancestor = pid,
                "an ancestor of a connection's peer exited while it was classified; walking \
                 its ancestry again"
            );
            return Decision::Rewalk;
        }
        _ => {}
    }
    Decision::Agent(unmatched(peer, ancestry, marker, started, name_of))
}

/// The processes whose environments [`reap::first_agent_marker`] reads for `peer`: the peer
/// itself, then each ancestor read (the peer alone when none could be).
fn marker_pids(peer: u32, ancestry: &reap::Ancestry) -> Vec<u32> {
    let mut pids: Vec<u32> = ancestry.chain.iter().map(|p| p.pid).collect();
    if pids.first() != Some(&peer) {
        pids.insert(0, peer);
    }
    pids
}

/// The thread id an [`AGENT_ENV`] value names: what precedes its first `:`, when not empty.
fn marker_thread(value: &str) -> Option<&str> {
    value
        .split_once(':')
        .map(|(thread, _)| thread)
        .filter(|t| !t.is_empty())
}

/// What a peer whose ancestry matched no process twr started is ([`Hub::agent_peer`]), given
/// its ancestry, what its environments showed of the [`AGENT_ENV`] marker
/// ([`reap::first_agent_marker`]), what twr started (`started`) and the names of the threads
/// the hub knows.
///
/// An ancestry that ends at an ancestor the process table does not show
/// ([`AncestryEnd::Concealed`]: procfs mounted with `hidepid`, where the
/// user's own SSH or login session has such ancestors) is decided like a whole one, by the
/// environments that were read: an agent descends from a process twr started, and those run as
/// the hub's user, so a chain that crosses into a process this user cannot see without first
/// passing one of them, or a process that carries the marker, does not run under twr (or comes
/// from a process that both cleared the marker and left its runtime, the limit of the module
/// docs). Unless the hidden ancestor has the pid of a process twr started (a runtime that made
/// itself not dumpable): then the peer is an agent of no known thread.
fn unmatched(
    peer: u32,
    ancestry: &reap::Ancestry,
    marker: ChainMarker,
    started: &StartedTable,
    name_of: impl Fn(&str) -> Option<String>,
) -> Option<AgentPeer> {
    let hidden = match marker {
        ChainMarker::Carried { pid, value } => {
            let thread = marker_thread(&value).map(str::to_string);
            let thread_name = match &thread {
                Some(t) => name_of(t).unwrap_or_else(|| t.clone()),
                None => "unknown".to_string(),
            };
            return Some(AgentPeer {
                thread,
                thread_name,
                why: AgentWhy::Marker { pid },
                peer,
            });
        }
        // A process of the chain had exited when its environment was read:
        // the chain is not what the peer descends from any more.
        ChainMarker::Gone { pid } => {
            tracing::warn!(
                peer,
                exited = pid,
                "a process of a connection's peer's ancestry exited while it was classified; it \
                 counts as an agent"
            );
            return Some(AgentPeer::untraced(peer, AncestryEnd::Gone));
        }
        ChainMarker::Hidden { pid } => Some(pid),
        ChainMarker::Absent => None,
    };
    match ancestry.end {
        AncestryEnd::Concealed { pid } if started.has_pid(pid) => {
            tracing::warn!(
                peer,
                ancestor = pid,
                "an ancestor of a connection's peer that twr started cannot be read; it counts \
                 as an agent"
            );
            Some(AgentPeer::untraced(peer, ancestry.end))
        }
        AncestryEnd::Init | AncestryEnd::Concealed { .. } => {
            let pid = hidden?;
            tracing::warn!(
                peer,
                hidden = pid,
                "an environment of a connection's peer or its ancestry cannot be read; it \
                 counts as an agent"
            );
            Some(AgentPeer {
                thread: None,
                thread_name: "unknown".to_string(),
                why: AgentWhy::Hidden { pid },
                peer,
            })
        }
        AncestryEnd::Unknown => {
            tracing::warn!(
                peer,
                read = ancestry.chain.len(),
                "the process table could not be read for a connection's peer; it counts as \
                 the user"
            );
            None
        }
        // Gone, Reused, Depth, Loop: the chain is not whole.
        end => {
            tracing::warn!(
                peer,
                ?end,
                read = ancestry.chain.len(),
                "the ancestry of a connection's peer could not be read to the end; it counts \
                 as an agent"
            );
            Some(AgentPeer::untraced(peer, end))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(pid: u32, start_time: u64) -> ProcessId {
        ProcessId { pid, start_time }
    }

    fn started(thread: &str, kind: StartedKind) -> Started {
        Started {
            thread: thread.into(),
            thread_name: format!("{thread}-name"),
            kind,
            generation: Some(1),
        }
    }

    fn found<'a>(table: &'a StartedTable, chain: &'a [ProcessId]) -> Option<(u32, &'a str)> {
        table.find(chain).map(|(p, s)| (p.pid, s.thread.as_str()))
    }

    fn runtime(thread: &str, generation: u64) -> RuntimeRef {
        RuntimeRef {
            thread: thread.into(),
            generation,
        }
    }

    /// The nearest started process in the chain decides; a recorded pid whose start time is not
    /// the live process's (the pid was reused) matches nothing, unless that runtime is matched
    /// by pid alone (its start time could not be read).
    /// A peer the rest leaves the user is an agent when a twr hub or attach is among its
    /// ancestors (not the peer itself); one that is an agent stays the agent it is.
    #[test]
    fn a_peer_under_a_hub_or_an_attach_is_an_agent() {
        let chain = reap::Ancestry {
            chain: vec![id(7, 70), id(6, 60), id(5, 50), id(4, 40)],
            end: AncestryEnd::Init,
        };
        let words = |a: &str| Some(a.split(' ').map(str::to_string).collect::<Vec<_>>());
        let args = |pid: u32| match pid {
            7 => words("/usr/bin/twr --socket /x hub"),
            6 => words("sh run.sh"),
            5 => words("/home/u/.local/bin/twr --socket /run/twr/ws/a.sock --workspace a hub"),
            _ => words("bash"),
        };
        assert_eq!(twr_ancestor(&chain, args), Some((5, TwrParent::Hub)));
        let attach = |pid: u32| match pid {
            5 => words("twr -w a attach api"),
            _ => words("bash"),
        };
        assert_eq!(twr_ancestor(&chain, attach), Some((5, TwrParent::Attach)));
        assert_eq!(twr_ancestor(&chain, |_| words("bash")), None);
        assert_eq!(twr_ancestor(&chain, |_| None), None);
        // The user: an agent under the hub.
        let d = under_hub(Decision::Agent(None), 7, Some((5, TwrParent::Hub)));
        let Decision::Agent(Some(a)) = d else {
            panic!("{d:?}")
        };
        assert_eq!(
            a.why,
            AgentWhy::Under {
                pid: 5,
                parent: TwrParent::Hub
            }
        );
        assert!(
            a.detail().contains("descends from a twr hub"),
            "{}",
            a.detail()
        );
        // Not under one: the user stays the user.
        assert_eq!(
            under_hub(Decision::Agent(None), 7, None),
            Decision::Agent(None)
        );
        // An agent stays the agent it was.
        let marked = AgentPeer {
            thread: Some("t".into()),
            thread_name: "t".into(),
            why: AgentWhy::Marker { pid: 6 },
            peer: 7,
        };
        let hub = Some((5, TwrParent::Hub));
        assert_eq!(
            under_hub(Decision::Agent(Some(marked.clone())), 7, hub),
            Decision::Agent(Some(marked))
        );
        assert_eq!(under_hub(Decision::Rewalk, 7, hub), Decision::Rewalk);
    }

    #[test]
    fn the_nearest_started_ancestor_with_the_same_start_time_decides() {
        let mut table = StartedTable::default();
        table
            .exact
            .insert(id(300, 50), started("w1", StartedKind::Runtime));
        table
            .exact
            .insert(id(200, 40), started("orch", StartedKind::Runtime));
        // A tool command (500) of a shell (400) of runtime 300, itself under 200.
        let chain = [
            id(500, 70),
            id(400, 60),
            id(300, 50),
            id(200, 40),
            id(100, 1),
        ];
        assert_eq!(found(&table, &chain), Some((300, "w1")));
        // The runtime itself connecting.
        assert_eq!(found(&table, &chain[2..]), Some((300, "w1")));
        // Pid 300 now names a process started later: it is not the recorded runtime, and the
        // next started ancestor (if any) decides.
        let reused = [id(500, 70), id(300, 65), id(100, 1)];
        assert_eq!(found(&table, &reused), None);
        let reused_under_orch = [id(500, 70), id(300, 65), id(200, 40)];
        assert_eq!(found(&table, &reused_under_orch), Some((200, "orch")));
        // The user's terminal descends from none of them.
        assert_eq!(found(&table, &[id(900, 80), id(800, 2)]), None);
        assert_eq!(found(&table, &[]), None);
        // A runtime whose start time could not be read: any process with its pid.
        table
            .by_pid
            .insert(400, started("w2", StartedKind::Runtime));
        assert_eq!(found(&table, &chain), Some((400, "w2")));
        assert!(!table.is_empty());
        assert!(StartedTable::default().is_empty());
    }

    fn ancestry(end: AncestryEnd, len: usize) -> reap::Ancestry {
        reap::Ancestry {
            chain: (0..len as u32).map(|i| id(1000 + i, 5)).collect(),
            end,
        }
    }

    fn names(id: &str) -> Option<String> {
        (id == "01KNOWN").then(|| "w1".to_string())
    }

    /// Without a started ancestor: a marker on the peer or any ancestor makes the peer an
    /// agent (of the thread the nearest carrier names, any value counts); without one, a
    /// whole chain or an unreadable table is the user, and a walk that could not be completed,
    /// or a whole one with an environment that cannot be read where it counts, is an agent of
    /// no known thread.
    #[test]
    fn an_unmatched_peer_is_an_agent_by_its_marker_or_an_unresolved_walk() {
        let whole = ancestry(AncestryEnd::Init, 3);
        let none = StartedTable::default();
        let marked = |pid: u32, value: &str| ChainMarker::Carried {
            pid,
            value: value.to_string(),
        };
        let absent = || ChainMarker::Absent;
        assert_eq!(unmatched(7, &whole, absent(), &none, names), None);
        let a = unmatched(7, &whole, marked(7, "01KNOWN:3"), &none, names).unwrap();
        assert_eq!(
            (a.thread.as_deref(), a.thread_name.as_str(), a.why, a.pid()),
            (Some("01KNOWN"), "w1", AgentWhy::Marker { pid: 7 }, 7)
        );
        assert!(a
            .detail()
            .starts_with("w1's agent (pid 7, which carries TWR_AGENT"));
        // Carried by an ancestor only (the peer cleared it): still that thread's agent.
        let a = unmatched(7, &whole, marked(1001, "01KNOWN:3"), &none, names).unwrap();
        assert_eq!((a.why, a.pid()), (AgentWhy::Marker { pid: 1001 }, 7));
        assert!(a.detail().starts_with(
            "w1's agent (pid 7, which descends from pid 1001, which carries TWR_AGENT"
        ));
        // A marker never makes a peer the runtime's own: only a recorded process does.
        assert!(!a.descends_from_runtime(&runtime("01KNOWN", 3)));
        let a = unmatched(7, &whole, marked(7, "01GONE:attach.2"), &none, names).unwrap();
        assert_eq!(
            (a.thread.as_deref(), a.thread_name.as_str()),
            (Some("01GONE"), "01GONE")
        );
        for odd in ["", "nocolon", ":1"] {
            let a = unmatched(7, &whole, marked(7, odd), &none, names).unwrap();
            assert_eq!(
                (a.thread, a.why),
                (None, AgentWhy::Marker { pid: 7 }),
                "{odd:?}"
            );
        }
        let table_unknown = ancestry(AncestryEnd::Unknown, 0);
        assert_eq!(unmatched(7, &table_unknown, absent(), &none, names), None);
        assert_eq!(
            unmatched(7, &table_unknown, marked(7, "01KNOWN:1"), &none, names).map(|a| a.why),
            Some(AgentWhy::Marker { pid: 7 })
        );
        for end in [
            AncestryEnd::Depth,
            AncestryEnd::Loop,
            AncestryEnd::Reused,
            AncestryEnd::Gone,
            AncestryEnd::Unreadable,
        ] {
            for len in [0, 4] {
                let a = unmatched(7, &ancestry(end, len), absent(), &none, names).unwrap();
                assert_eq!(
                    (a.thread.as_deref(), a.why),
                    (None, AgentWhy::Unresolved(end))
                );
                assert!(a
                    .summary()
                    .starts_with("a process twr cannot trace (pid 7)"));
                assert!(!a.descends_from_runtime(&runtime("01KNOWN", 1)));
            }
            // Its marker still names its thread.
            let a = unmatched(
                7,
                &ancestry(end, 4),
                marked(1002, "01KNOWN:1"),
                &none,
                names,
            );
            assert_eq!(a.map(|a| a.why), Some(AgentWhy::Marker { pid: 1002 }));
            // A hidden environment does not change why it is restricted.
            let a = unmatched(
                7,
                &ancestry(end, 4),
                ChainMarker::Hidden { pid: 7 },
                &none,
                names,
            );
            assert_eq!(a.map(|a| a.why), Some(AgentWhy::Unresolved(end)));
        }
        // A whole chain with an environment that cannot be read where it counts: an agent of
        // no known thread, never the orchestrator's runtime.
        let hidden = |pid| ChainMarker::Hidden { pid };
        let a = unmatched(7, &whole, hidden(7), &none, names).unwrap();
        assert_eq!(
            (a.thread.as_deref(), a.thread_name.as_str(), a.why, a.pid()),
            (None, "unknown", AgentWhy::Hidden { pid: 7 }, 7)
        );
        assert!(a
            .summary()
            .starts_with("a process twr cannot trace (pid 7)"));
        assert!(
            a.detail()
                .starts_with("a process twr cannot trace (pid 7: its environment, which would show TWR_AGENT, cannot be read)"),
            "{}",
            a.detail()
        );
        assert!(!a.descends_from_runtime(&runtime("01KNOWN", 1)));
        let a = unmatched(7, &whole, hidden(1001), &none, names).unwrap();
        assert_eq!((a.why, a.pid()), (AgentWhy::Hidden { pid: 1001 }, 7));
        assert!(a
            .detail()
            .contains("the environment of pid 1001, of its ancestry"));
        // A process table that cannot be read leaves it the user, as without the marker.
        assert_eq!(unmatched(7, &table_unknown, hidden(7), &none, names), None);
        // Only a recorded process of that runtime of the thread proves the peer descends from
        // it: not another thread's, an attach's, or an earlier runtime of the same thread.
        let started = |thread: &str, kind, generation| AgentPeer {
            thread: Some(thread.into()),
            thread_name: "o".into(),
            why: AgentWhy::Started {
                pid: 40,
                start_time: 9,
                generation,
                kind,
            },
            peer: 7,
        };
        let orch = runtime("01ORCH", 2);
        assert!(started("01ORCH", StartedKind::Runtime, Some(2)).descends_from_runtime(&orch));
        assert!(!started("01W1", StartedKind::Runtime, Some(2)).descends_from_runtime(&orch));
        assert!(!started("01ORCH", StartedKind::Attach, None).descends_from_runtime(&orch));
        assert!(!started("01ORCH", StartedKind::Runtime, Some(1)).descends_from_runtime(&orch));
        assert!(!started("01ORCH", StartedKind::Runtime, None).descends_from_runtime(&orch));
    }

    /// The walk is checked against the environment reads that follow it: a
    /// peer that left (exited, maybe handing its connection to a child), or whose environment
    /// could not be read because it had exited, is an agent of no known thread whatever the
    /// walk found; an ancestor that had exited walks the ancestry again, then counts the same.
    /// A marker still decides.
    #[test]
    fn a_peer_or_ancestor_that_exits_while_classified_is_not_the_user() {
        let none = StartedTable::default();
        let gone = |pid| ChainMarker::Gone { pid };
        let untraced = Decision::Agent(Some(AgentPeer::untraced(7, AncestryEnd::Gone)));
        for end in [
            AncestryEnd::Init,
            AncestryEnd::Concealed { pid: 999 },
            AncestryEnd::Unknown,
        ] {
            let a = ancestry(end, 3);
            let d = |marker, left, rewalk| decide(7, &a, marker, left, rewalk, &none, names);
            // The peer left after its environment was read without the marker.
            assert_eq!(d(ChainMarker::Absent, true, true), untraced, "{end:?}");
            assert_eq!(d(ChainMarker::Hidden { pid: 1001 }, true, true), untraced);
            // Its environment could not be read because it had exited.
            assert_eq!(d(gone(7), false, true), untraced);
            assert_eq!(d(gone(7), false, false), untraced);
            // An ancestor had exited: walk again, then an agent of no known thread.
            assert_eq!(d(gone(1001), false, true), Decision::Rewalk);
            assert_eq!(d(gone(1001), false, false), untraced);
            assert_eq!(d(gone(1001), true, true), untraced);
            let marked = ChainMarker::Carried {
                pid: 1002,
                value: "01KNOWN:3".into(),
            };
            let Decision::Agent(Some(a)) = d(marked, true, true) else {
                panic!("a marker restricts");
            };
            assert_eq!(a.why, AgentWhy::Marker { pid: 1002 });
        }
        // Nothing exited: as before.
        let whole = ancestry(AncestryEnd::Init, 3);
        let d = |marker| decide(7, &whole, marker, false, true, &none, names);
        assert_eq!(d(ChainMarker::Absent), Decision::Agent(None));
        let Decision::Agent(Some(a)) = d(ChainMarker::Hidden { pid: 7 }) else {
            panic!("a hidden environment restricts");
        };
        assert_eq!(a.why, AgentWhy::Hidden { pid: 7 });
    }

    /// An ancestry that ends at an ancestor the process table does not show (with
    /// `hidepid`) is decided like a whole one: the user without a marker, an agent with one or
    /// with a same-user environment that cannot be read, and an agent of no known
    /// thread when the hidden ancestor has the pid of a process twr started.
    #[test]
    fn a_hidden_ancestor_ends_the_chain_like_init() {
        let concealed = ancestry(AncestryEnd::Concealed { pid: 999 }, 3);
        let none = StartedTable::default();
        let marked = ChainMarker::Carried {
            pid: 1001,
            value: "01KNOWN:3".into(),
        };
        assert_eq!(
            unmatched(7, &concealed, ChainMarker::Absent, &none, names),
            None
        );
        let a = unmatched(7, &concealed, marked, &none, names).unwrap();
        assert_eq!(
            (a.thread.as_deref(), a.why),
            (Some("01KNOWN"), AgentWhy::Marker { pid: 1001 })
        );
        let a = unmatched(
            7,
            &concealed,
            ChainMarker::Hidden { pid: 1001 },
            &none,
            names,
        );
        assert_eq!(a.map(|a| a.why), Some(AgentWhy::Hidden { pid: 1001 }));
        let mut table = StartedTable::default();
        table
            .by_pid
            .insert(500, started("01W1", StartedKind::Runtime));
        table
            .exact
            .insert(id(600, 4), started("01W2", StartedKind::Attach));
        assert_eq!(
            unmatched(7, &concealed, ChainMarker::Absent, &table, names),
            None
        );
        for pid in [500, 600] {
            let end = AncestryEnd::Concealed { pid };
            let a = unmatched(7, &ancestry(end, 3), ChainMarker::Absent, &table, names).unwrap();
            assert_eq!(
                (a.thread.as_deref(), a.why),
                (None, AgentWhy::Unresolved(end))
            );
            assert!(!a.descends_from_runtime(&runtime("01W1", 1)));
        }
    }

    /// A read of the environments that fails in the hub itself (its task panicked: here a
    /// test hook that panics) is no absent marker: the peer is an agent of no known thread, not
    /// the user.
    #[tokio::test]
    async fn a_read_that_fails_in_the_hub_does_not_make_the_peer_the_user() {
        let dir = tempfile::tempdir().unwrap();
        let mut opts = crate::HubOptions::new(
            dir.path().join("twr.db"),
            twr_core::config::Config::default(),
        );
        opts.marker_read_hook = Some(std::sync::Arc::new(|_| panic!("injected")));
        let hub = Hub::open(opts).await.unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env_remove(AGENT_ENV)
            .spawn()
            .unwrap();
        let walk = hub.agent_peer(child.id()).await;
        let _ = child.kill();
        let _ = child.wait();
        let agent = walk.agent.expect("an agent, not the user");
        assert_eq!(agent.why, AgentWhy::Unresolved(AncestryEnd::Unreadable));
        assert_eq!(agent.thread, None);
    }

    /// [`Hub::agent_peer`]'s decision, made on this thread (where [`reap::with_proc_faults`]
    /// applies).
    #[cfg(target_os = "linux")]
    fn classify_now(peer: u32, started: &StartedTable) -> (reap::Ancestry, Option<AgentPeer>) {
        let mut walks = 1;
        loop {
            let a = settled_ancestry(peer);
            if let Some(agent) = started_agent(peer, &a, started) {
                return (a, Some(agent));
            }
            let (marker, left) = read_markers(peer, &a);
            match decide(peer, &a, marker, left, walks < WALK_TRIES, started, names) {
                Decision::Agent(agent) => return (a, agent),
                Decision::Rewalk => walks += 1,
            }
        }
    }

    /// Until `pid` has exec'd (its environment shows `TWR_TEST_SETTLED=1`).
    #[cfg(target_os = "linux")]
    fn settled(pid: u32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !reap::process_environ(pid)
            .is_ok_and(|e| reap::environ_has(&e, "TWR_TEST_SETTLED", "1"))
        {
            assert!(std::time::Instant::now() < deadline, "{pid} never exec'd");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    /// The user's process under ancestors procfs hides (`hidepid=1`: their records cannot be
    /// read; `hidepid=2`: they are not there), here this test process above a
    /// child of it without the marker: the user. The peer itself hidden: an agent of no known
    /// thread. No `/proc` at all: the user, the process table unknown.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_user_under_hidden_ancestors_stays_the_user() {
        let mut user = std::process::Command::new("sleep")
            .arg("30")
            .env("TWR_TEST_SETTLED", "1")
            .env_remove(AGENT_ENV)
            .spawn()
            .unwrap();
        let (peer, me) = (user.id(), std::process::id());
        settled(peer);
        let none = StartedTable::default();
        let faults = |denied: &[u32], hidden: &[u32], unavailable| reap::ProcFaults {
            unavailable,
            denied: denied.to_vec(),
            hidden: hidden.to_vec(),
        };
        let run = |f| reap::with_proc_faults(f, || classify_now(peer, &none));
        let concealed = AncestryEnd::Concealed { pid: me };
        let hidepid1 = run(faults(&[me], &[], false));
        let hidepid2 = run(faults(&[], &[me], false));
        let peer_denied = run(faults(&[peer], &[], false));
        let peer_hidden = run(faults(&[], &[peer], false));
        let no_proc = run(faults(&[], &[], true));
        let _ = user.kill();
        let _ = user.wait();
        for (a, agent) in [hidepid1, hidepid2] {
            assert_eq!((a.chain.len(), a.end), (1, concealed));
            assert_eq!(agent, None);
        }
        assert_eq!(
            peer_denied.1.map(|a| a.why),
            Some(AgentWhy::Unresolved(AncestryEnd::Unreadable))
        );
        assert_eq!(
            peer_hidden.1.map(|a| a.why),
            Some(AgentWhy::Unresolved(AncestryEnd::Gone))
        );
        assert_eq!(no_proc.0.end, AncestryEnd::Unknown);
        assert_eq!(no_proc.1, None);
    }

    /// An agent under a runtime (here a shell carrying the marker, whose child cleared it)
    /// stays one when an ancestor above the runtime is hidden: by the
    /// recorded runtime in its chain, else by the runtime's marker; a hidden runtime that was
    /// recorded makes it an agent of no known thread.
    #[cfg(target_os = "linux")]
    #[test]
    fn an_agent_under_a_runtime_stays_one_above_a_hidden_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut runtime_sh = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "env -u {AGENT_ENV} TWR_TEST_SETTLED=1 sleep 30 & echo $! > {p}.tmp; \
                     mv {p}.tmp {p}; wait",
                    p = pidfile.display()
                ),
            ])
            .env(AGENT_ENV, "01KNOWN:1")
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let peer: u32 = loop {
            if let Ok(t) = std::fs::read_to_string(&pidfile) {
                break t.trim().parse().unwrap();
            }
            assert!(std::time::Instant::now() < deadline, "no pid file");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        settled(peer);
        let (rt, me) = (runtime_sh.id(), std::process::id());
        let rt_id = id(rt, reap::process_start_time(rt).unwrap());
        let mut recorded = StartedTable::default();
        recorded
            .exact
            .insert(rt_id, started("01KNOWN", StartedKind::Runtime));
        let none = StartedTable::default();
        let denied = |pid| reap::ProcFaults {
            denied: vec![pid],
            ..Default::default()
        };
        let above = reap::with_proc_faults(denied(me), || {
            [classify_now(peer, &recorded), classify_now(peer, &none)]
        });
        let hidden_rt = reap::with_proc_faults(denied(rt), || classify_now(peer, &recorded));
        let _ = reap::signal(peer, false, "KILL");
        let _ = runtime_sh.wait();
        let [(a, by_runtime), (_, by_marker)] = above;
        assert_eq!(a.end, AncestryEnd::Concealed { pid: me });
        assert_eq!(
            a.chain.iter().map(|p| p.pid).collect::<Vec<_>>(),
            [peer, rt]
        );
        let by_runtime = by_runtime.unwrap();
        assert_eq!(by_runtime.thread.as_deref(), Some("01KNOWN"));
        assert!(by_runtime.descends_from_runtime(&runtime("01KNOWN", 1)));
        assert_eq!(by_marker.map(|a| a.why), Some(AgentWhy::Marker { pid: rt }));
        let end = AncestryEnd::Concealed { pid: rt };
        assert_eq!(hidden_rt.0.end, end);
        assert_eq!(hidden_rt.1.map(|a| a.why), Some(AgentWhy::Unresolved(end)));
    }
}
