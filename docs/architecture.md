# Architecture

```mermaid
flowchart LR
  subgraph clients
    TUI[twr TUI]
    CLI[twr CLI]
  end
  subgraph hub[twr hub]
    API[Unix socket API<br/>NDJSON]
    LOG[(SQLite<br/>append-only event log<br/>+ control stream)]
    RED[reducers:<br/>thread state, attention stack]
    DEC[decision ladder<br/>code, orchestrator, you]
    WS[WebSocket listener]
  end
  subgraph local[local box]
    A1[claude -p stream-json]
    A2[codex app-server]
    A3[grok agent stdio]
  end
  subgraph remote[remote box]
    RUN[twr runner<br/>spool + replay]
    A4[agent CLIs]
  end
  ORCH[orchestrator thread] -- MCP tools --> MCP[twr mcp] --> API
  TUI --> API
  CLI --> API
  API --> LOG --> RED
  API --> DEC
  LOG -- events --> A1 & A2 & A3
  A1 & A2 & A3 -- normalized events --> LOG
  WS <--> RUN --> A4
  WS --> LOG
```

## The hub

`twr hub` is one process per user. It listens on a Unix socket, mode 0600 in a 0700 directory, so only its owner can connect, and every client (the CLI, the TUI, `twr mcp`) refuses a socket whose listener runs as another user. Bare `twr` starts one in the background when none answers, with its log in the data directory as `hub.log`. Clients speak newline-delimited JSON over the socket. [SKILL.md](../SKILL.md) is the full reference for the methods, events and MCP tools.

## The event log

Everything is an event in an append-only log in SQLite: one stream per thread, plus a global control stream. Each event has an id, a per-thread sequence number, a timestamp, a thread, a turn, a source and a kind. The source says who wrote it: an adapter, the hub, a user client, the orchestrator or a decision provider.

The control stream carries what clients care about across all threads: thread lifecycle (`thread.created`, `thread.started`, `thread.stopped`, `thread.crashed`, `thread.parked`, `thread.resumed`, `thread.attached`, `thread.detached`, `thread.mode_changed`, `thread.model_changed`, `thread.deleted`), turn starts and ends, session-scoped task updates, attention items, reports, decisions, git snapshots, runner connections, context updates, worktree disk measurements and trims (`worktree.disk`, `worktree.trimmed`) and subscription usage (`account.usage`, on the hub's own thread: the latest usage windows of a runtime's account, which the hub folds from its adapters' `runtime.limits` reports without recording those; see [runtimes.md](runtimes.md#subscription-usage)).

`thread.mode_changed{mode, previous, by}` records a new permission mode for a thread, set by you or by the orchestrator relaying what you asked (`by: user | orchestrator`). One with `mode == previous` records that you chose the mode the thread already had: nothing changes or restarts, but an earlier request of yours to the orchestrator no longer covers that thread's mode. The thread's mode is `thread.created`'s, then the latest `thread.mode_changed`'s, and every runtime start uses it. A live runtime started in the old mode is stopped once it is idle and started again on the same session; that stop is a `thread.stopped` with `ext.twr.reason: "mode_change"`, which doesn't mark the thread stopped. `thread.model_changed{model, effort, previous_model, previous_effort, by}` does the same for the thread's model and reasoning effort (null is the runtime's default), set by you or by the orchestrator when you ask it; its restart's stop says `"model_change"` (or `"mode_change"` when the mode changed too). With `runtime` and `previous_runtime` it moves the thread to another runtime: the old runtime stops with `"runtime_change"` and isn't started again, and the thread's next turn starts a new session of the new one, its prompt prefixed with a handoff built from the log (`turn.started` `ext.twr.runtime_handoff`). `thread.deleted` records that you confirmed deleting a thread: its runtime stopped, its worktree removed (or, when git could not remove all of it, left without its `.git` and named in `worktree_left`) and its branch deleted. The thread then leaves every listing, and nothing more is recorded on it. The control stream never carries agent text or tool calls; those stay in the thread's own stream. Each control item has a global sequence number, `gseq`, and a subscription delivers the stored backlog and then live items with no gaps or duplicates.

Payloads over 4 KB keep a preview in the event, and the rest moves to a content-addressed blob store. `twr events --hydrate` puts it back.

## Reducers

Thread state, the attention stack and reports are reducers over the log. Nothing else is stored. Replaying the log from the start gives the same state as the live hub, which is how a hub restart recovers and what the runner fuzz harness checks.

A thread's status is one of `idle`, `running`, `background`, `waiting`, `done`, `error`, `parked` or `attached`. The attention stack orders items by reason, then age: permission, question, error, review, stalled. The TUI puts threads with items first, in that order, then running/background, failed, done, idle and parked threads.

`task.updated {task_id, running, description, heartbeat}` is on the control stream, with no turn number. The reducer keeps a map of running tasks across `turn.ended` and `report.ready`. An open turn is `running`; with no turn and at least one task it is `background`. Permission/question items still take precedence as `waiting`. Idle/done require both an empty task map and no open turn. Terminal task updates remove by id; duplicate completions do nothing. Stops, parks, crashes and new runtime process starts clear the map. Runtime-generated follow-ups are recorded as `turn.started {from: runtime}` and receive the next hub turn number, without a user prompt. Only the hub numbers turns. Adapters send a runtime's own turn and its events without a number. The start and end of such a turn carry the adapter's own id for it (`ext.twr.runtime_turn`). If the hub has a turn of its own open when the start arrives, because a prompt is on its way to the runtime, it drops the start. The runtime turn's output then goes into the open turn, and its end, whose id isn't the open turn's, closes nothing. Its usage is added once to the end of the turn that took it in. The hub keeps that link in memory, so a hub restart in between loses the usage. The hub decides from the log alone, so a runner's replay gets the same answer. A request that belongs to the runtime's session rather than to its turn, such as a Codex child agent's approval, carries `ext.twr.session` and is recorded outside any turn, so a turn's end doesn't close it.

Thread JSON adds `background_task_count` and `background_tasks` (id → description and last liveness timestamp), and `activity`, one line on what the thread does now, which the hub folds from the same events without recording anything (`hub::activity`, phrased by `twr_core::activity`). The CLI, MCP, TUI cards/grid and status line all derive their display from that state; background threads remain visible in Focus mode and animate like running threads.

A report for a turn that yielded to tasks remains available as an interim report (`ext.twr.background_tasks`, with each task's id and start in `ext.twr.background_task_starts`). A `report.write` for that turn while any of those tasks still runs keeps the marker and raises no review; tasks a later turn started, or an id reused by a new task, don't count. A `report.write` for a follow-up turn keeps `ext.twr.turn_from`. It does not raise a completion/review notice and fires no watch. The conversation says the worker "yielded with N background tasks still running (interim report: the result comes later)"; the last task completion produces one background-finished update when no turn is open. Progress and intermediate completions do not produce conversation notices. An automatic follow-up turn gets its own final report, marked `ext.twr.turn_from: "runtime"`; the conversation says the worker "finished, in a follow-up turn it started after background work". An orchestrator watch fires on that final report, or, when no follow-up turn starts within 5 seconds of the tasks ending, on the idle worker ([orchestrator.md](orchestrator.md#watches)).

When the last of a worker's tasks ends (not a user's `!` command) with no turn open, its last turn completed and its report interim, the hub waits `wake_after_secs` (30 s; `hub::wake`) by log time. If by then no turn started, no task started again and nothing is queued, and the thread is not stopped, parked, attached, crashed or waiting on the user, it queues a prompt of its own (`from: runtime`, source `hub`, like a job's wake) listing what ended since that turn, each job with its result and log, with a `conversation.update` on the worker recording `ext.twr.auto_wake = {turn, tasks}` in the same batch. At most one wake per interim report (its turn) and 5 in a row with no turn the user or the orchestrator started in between; past that the hub records `ext.twr.auto_wake_skipped = {turn, why}` instead. Both are folded from the log; a wake still waiting is memory only, so a hub restarted meanwhile wakes nothing. A runtime being stopped (a settings restart) or a box offline holds a wake up for up to 10 minutes, and the idle line waits with it. A turn the Claude Code adapter opened for a notification and ended `no_run` handled nothing: its report repeats the interim report it followed (`ext.twr.no_run_of`, said nowhere again), so the worker counts as idle after an interim report, for the wake (at once, or when its last task ends), the idle line and `send --after` alike. The orchestrator's held idle line waits for the wake's decision, and the wake's `turn.started` drops it. A `send --after` gate counts a worker with a wake pending as not settled.

The orchestrator's own background work also defers automatic notice turns. User prompts may still reach a runtime between turns while tasks are live; twr does not interrupt those tasks to deliver a prompt. Settings restarts wait for the task map to empty.

Stall detection includes background work, but inventory heartbeats only renew task liveness; they do not claim progress or dismiss a stalled card. No stall is raised while a job of the thread's own runs (renewed within 3 minutes): its turn or its runtime's watcher waits on it. A turn open and silent for `min(stall_after_secs, 5 min)`, counted from its last event and from when the oldest prompt queued for it arrived, while that prompt waits, gets a hung-turn card instead of a plain stall: a hub `stalled` item with `ext.twr.hung = {turn, waiting}` and the options `interrupt` (the turn is interrupted as `message.send_now` does, and what waits runs next) and `wait` (no other hung-turn card for that turn). It replaces an open plain stall in the same batch, notifies as `hung`, and reaches the orchestrator as an urgent `reason=hung` line; only the user answers it. Settings restarts wait for tasks to drain. Park/attach require force while background work runs. Stop and forced park use the existing durable stop and process-group/marked-descendant reaping paths. Remote frames carry the same task events and generation fencing; reconnects retain tasks for live sessions, and missing runtimes clear them.

## Attention items

An attention item is something waiting on you. Agents raise permissions and questions. The hub raises routing questions, reviews, errors, stalled notices and hung-turn cards. Each item has an id and a revision, the control `gseq` of its latest raise. An answer names the revision it was given for, and the hub refuses it if the item was raised again since. [usage.md](usage.md#answer-permissions-and-questions) shows how that looks from the CLI.

## The decision ladder

Three places in the hub need a judgment call:

| Site | Question | Default when nothing answers |
|---|---|---|
| `route` | which thread gets an unaddressed message | ask you |
| `grade` | how a finished turn went | `success` at 0.6 |
| `block_kind` | whether a failed turn is transient | needs you, at 0.5 |

A message a client sends with `say {to: "orchestrator"}`, as the TUI's orchestrator conversation does, skips the `route` site and goes to the orchestrator as if the ladder had delegated it.

Each site is a ladder of providers. Code rules go first. For `route`, the orchestrator comes next, then you; for `grade` and `block_kind`, the site's default. The first answer wins. Every rung that answered is logged as a `decision.made` event with its confidence and latency, so the TUI's decisions table (`/decisions`) can show who decided what.

## Adapters

Adapters drive each agent CLI through its headless JSON mode and turn its output into a small shared event schema: `agent.text`, `agent.thinking`, `agent.tool_call`, `agent.tool_result`, `attention.raised`, `turn.ended` and so on. Runtime-specific extras go under `data.ext.<runtime>`.

A new runtime is one file in `crates/twr-adapters` that implements the `Adapter` trait from `twr-core`. `spawn` starts a session and returns a handle: the adapter sends events up and takes `send`, `respond` and `stop` commands. `resume` reopens a session, and `attach_command` names the interactive CLI for `twr attach`. [runtimes.md](runtimes.md) lists the adapters and their tiers.

## The orchestrator

The orchestrator is just another thread, running whatever runtime you configure, with the hub's tools exposed over MCP by `twr mcp`. The hub gives each orchestrator process a fresh token, passed to its MCP server through a 0600 file and never on a command line. A connection that presents the token is the orchestrator and may call only a subset of the socket methods. A connection from a process the hub started for a thread, or from one of its children, is that thread's agent: it may read, and start and cancel its own thread's jobs, starting them only while the thread's mode may run commands ([usage.md](usage.md#security)). The relay rule decides when it may pass on your answer. The hub checks where the message came from and what was on screen when you sent it, then reads the message itself: a message that is one short command is applied at once, and anything else goes on a confirm card that only you can answer. See [orchestrator.md](orchestrator.md).

## Runners

A runner runs one remote box's threads. It connects out to the hub's WebSocket listener, spools events on disk while disconnected, and replays them on reconnect. The hub commits each event with the runner's watermark before acknowledging it, so nothing is lost or recorded twice. See [runners.md](runners.md).

## Crates

| Crate | What it holds |
|---|---|
| `twr-core` | events, reducers, attention stack, the `Adapter` trait, config, socket protocol |
| `twr-hub` | the hub: event store, control stream, socket API, adapter supervisors |
| `twr-adapters` | the runtime adapters |
| `twr-decisions` | the decision ladder and its code and orchestrator rungs, and the short-command reading of the orchestrator's relayed answers and mode changes |
| `twr-mcp` | the MCP server that exposes the hub to the orchestrator |
| `twr-docs` | twr's docs compiled in, split into sections and searched, for the orchestrator's `read_docs` |
| `twr-runner` | `twr runner`, worktrees and git snapshots, local and remote |
| `twr-tui` | the terminal UI |
| `twr-cli` | the `twr` binary |
