# Usage

A walk through one session: start a thread, talk to it, answer what it asks, read its report, step into it, and put it away.

## Start a thread

```sh
# Opens the TUI and starts the hub in the background if it isn't running.
twr

# In another terminal: a thread on a repo. It gets its own worktree on branch twr/api.
twr thread new ~/code/api --name api
```

The project is a name from `[projects]` in the [config](configuration.md), a directory, or the folder name of a git repository in one of your code folders:

```sh
# Finds ~/code/orbit-webapp (or ~/code/<group>/orbit-webapp) when it's the only match.
twr thread new orbit-webapp
```

With several logins for one runtime, `--account work` (or `--runtime claude-code:work`) picks the account; a project can pin one. See [accounts.md](accounts.md).

twr looks in `~/code`, `~/src`, `~/projects`, `~/dev`, `~/Developer`, `~/repos`, `~/git` and `~/workspace`, and one level below each. `project_roots` in the [config](configuration.md#notes-on-some-keys) changes the list. The repository found is used as if you had typed its path, and the hub's answer names it. If two repositories have that name, twr lists both and creates nothing; give the path of the one you mean. If none does, the error says where it looked. The TUI's `/new orbit-webapp` and the orchestrator ("open a new session on my orbit-webapp project") work the same way. On the command line, a directory of that name in the current directory is taken first.

Without `--name`, twr makes one up. Other options:

| Option | Effect |
|---|---|
| `--runtime R` | `claude-code`, `codex`, `grok-build`, `acp:<command>`, a `[runtimes]` preset or `pty:<command>` ([runtimes.md](runtimes.md)) |
| `--model M` | the model; default `[threads] model`, else the runtime's default |
| `--mode M` | the runtime's permission mode; default `[threads] default_mode`, which is `auto` unless you set it ([runtimes.md](runtimes.md#permission-modes)) |
| `--alias A` | another name for the thread, repeatable |
| `--box B` | run on a remote box ([runners.md](runners.md)) |
| `--base REF` | start the thread's branch from this ref instead of `HEAD` (namespaced with `TWR_HOME`; [worktrees.md](worktrees.md)) |
| `--worktree`, `--no-worktree` | override the worktree setting for this thread ([worktrees.md](worktrees.md)) |

```sh
# Any agent that speaks the Agent Client Protocol over stdio.
twr thread new ~/code/api --name gem --runtime 'acp:gemini --experimental-acp'

# Any other interactive CLI, under a pseudo-terminal (degraded).
twr thread new ~/code/api --name aider --runtime 'pty:aider --model sonnet'
```

`twr thread list` shows every thread with its status, runtime, permission mode, model, project, turns, what its turns spent (`SPENT`: `1.2M tok $3.40`, see [Subscription usage](#subscription-usage)), how much disk its worktree takes (`DISK`, [worktrees.md](worktrees.md#disk-use)), open items, queued prompts and, last, its activity: one line on what it is doing now, such as `implementing scroll anchoring · running tests` or `waiting for permission: Bash: git push` ([tui.md](tui.md#the-activity-line)), cut to 60 characters. Parked threads are left out, and a line under the table names them (`2 parked threads hidden: old, older (twr thread list --all)`). `--all` (`-a`) lists them too.

### Change a thread's mode

```sh
twr mode api plan     # from now on, api runs in plan mode
```

The mode goes into the log (`thread.mode_changed`). A thread that is idle restarts its runtime right away, on the same session. One in the middle of a turn finishes that turn first, including any permission it is waiting on, and restarts before its next queued prompt runs. A runtime with background tasks of its own (a background shell, a subagent) keeps them running across a change that keeps or widens the mode, and restarts once they end. A narrower mode doesn't wait for them, or for a request one of them is waiting on: while they run the runtime can start turns of its own in the old mode, so it restarts right away, or once its current turn ends, its background tasks end with it, and their open requests close as they would on a stop. `twr mode` says so (`its background tasks end with it`). A mode is narrower when the runtime loses anything it could do without asking, even at the same level: Codex `on-request` to `manual` asks before commands that ran unasked, and `auto` to `never` loses network access. A parked or stopped thread starts in the new mode next time. Setting the mode it already has changes nothing and restarts nothing, but it is recorded as your choice (`thread.mode_changed` with the same mode before and after): a request you made to the orchestrator before it can no longer change that thread's mode. The orchestrator's mode can't be changed this way; switch the orchestrator with `twr orchestrator use`. You can also ask the orchestrator, as in "set @api to plan mode" or "set atlas-1 to plan mode" ([orchestrator.md](orchestrator.md#changing-a-threads-mode)).

### Change a thread's model and effort

```sh
twr model api                              # show api's model and reasoning effort
twr model api claude-fable-5-1 --effort high
twr model api --effort default             # back to the runtime's default effort
twr model api gpt-6.1-sol --effort xhigh --runtime codex   # move api to Codex
```

The model is the runtime's own id or alias (Claude Code takes `fable`, `opus`, `sonnet` or a full id). The effort must be one the runtime takes: Claude Code `low`, `medium`, `high`, `xhigh`, `max`; Codex `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra`; Grok Build `low`, `medium`, `high`, `xhigh`; an ACP agent its own values. `default` returns either one to the runtime's default, and `twr thread new --effort E` sets the effort from the start. Once a thread has a session, `default` works only where its runtime falls back to the default on resume: the effort on Claude Code. Codex, Grok Build and ACP agents keep a session's model and effort, and Claude Code keeps its model, so twr refuses `default` there and you need to pick a value ([runtimes.md](runtimes.md#models-and-reasoning-effort)). The change goes into the log (`thread.model_changed`) and applies like a mode change: an idle thread restarts its runtime right away on the same session, a running one finishes its turn first, and a parked or stopped one starts with it next time. Setting what the thread already has records nothing. Not for the orchestrator (`twr orchestrator use`) or `pty` threads, whose command line is their own. Ask the orchestrator and it does the same ([orchestrator.md](orchestrator.md#changing-a-threads-model-and-effort)).

`--runtime R` moves the thread to another runtime: `claude-code`, `codex`, `grok-build`, `acp:<command>` or a `[runtimes]` preset, but not `pty:`. A session can't move between agents, so the thread keeps its name, worktree, branch, queued prompts and permission mode, and starts a new session of the new runtime. Its current runtime stops: an idle one right away, a running one once its turn ends. The new session starts with the thread's next turn, whose prompt carries a handoff of what the previous session did: the thread's first prompt, its worktree and branch, its latest reports and its recent turns. A model or effort you leave out is the new runtime's default, since the old runtime's names mean nothing to it, and `default` is always fine with a move, since the new session is a new one. Moving back to a runtime the thread left a session of earlier, as in `twr model api --runtime claude-code` after the move above, resumes that session instead: its next turn gets a handoff of only what the thread did since it left (none when it did no work), a model or effort you leave out is the one it had then, and a `default` the runtime would not take on resume starts a new session instead. If the session can't be resumed, twr retries that turn once on a new session with the full handoff, then the turn's message ([runtimes.md](runtimes.md#moving-a-thread-back)). The thread's permission mode must be one the new runtime takes, and an attached thread can't move. Until the new runtime's session has started, `twr attach` refuses (also for a session the move resumes, which hasn't been told what happened since yet), so send it a prompt first. `twr model api` then says `its next turn starts a new codex session` until that session has started (`its next turn resumes its earlier claude-code session <id>` on a move back). The orchestrator can move a thread too, but only to `claude-code`, `codex`, `grok-build` or a `[runtimes]` preset, and, unless the thread is in `plan`, `read-only` or `dontAsk`, only with your confirmation on a card ([orchestrator.md](orchestrator.md#changing-a-threads-model-and-effort)).

### Rename a thread

```sh
twr rename twr-1 notifications-design
```

The new name follows the rules of `--name` and must be free, in any case: `API` and `api` count as one name. Only the name changes: the thread keeps its worktree path and its branch, which keep the name they were made with (`<data>/worktrees/<project>/twr-1`, `twr/twr-1`). Moving the worktree would pull the directory out from under a running agent and lose sessions that Claude Code and Codex keep by directory, and the branch may already be pushed or checked out somewhere else. So the old name stays the thread's own: `twr attach twr-1` still finds it, and no other thread can take the name, since its worktree and branch would collide. The thread can take an old name back. Its aliases stay too. An old name reaches its thread before any name that merely starts with it, so after `api` becomes `gateway`, `@api` doesn't go to `api-client`. Deleting the thread frees its old names along with its name; restored, it takes back the ones no other thread took meanwhile. The change goes into the log (`thread.renamed`), and transcripts, `twr events` and the TUI show it. The orchestrator can rename a thread as well ([orchestrator.md](orchestrator.md#naming-threads)). Orchestrator threads can't be renamed.

## Talk to threads

```sh
# Straight to one thread. The prompt is queued if a turn is running.
twr send api "add a health check endpoint and a test for it"

# Or say it and let the hub route it.
twr say "on api, also bump the version"
twr say "create a thread for ~/code/web and make the footer links relative"
```

`say` routes a message in this order. `@api ...` or `on api, ...` goes to that thread. If there is only one worker, it goes there. `new <project>: ...` starts a thread. Anything else goes to the orchestrator. With no orchestrator runtime, the hub asks you which thread should get it, on a `twr asks` card.

In the TUI, a typed line goes through `say` too. Start it with `>` to send it to the focused thread.

## Wait for threads

`twr wait` blocks until a thread finishes or needs you, and prints how it ended: its report when it finished.

```sh
twr wait web                       # until web finishes its work, or waits on you
twr wait web db --all --timeout 1h # until both have, at most an hour
twr wait web --json                # {thread, name, state, turn, report, grade}
```

A thread counts as finished when it has no turn running, no background work or job, no prompt queued for it (also one held by `--after`, even after a failed turn: it goes out by itself once the threads it waits for finish), and the report of its last turn written. When it ended a turn with background work still running, its runtime may start a turn of its own once that work ends, so twr waits five more seconds after the work ends; after a failure too, as an automatic retry may follow. A stopped or parked thread doesn't go on, whatever is queued for it, and counts at once. A thread on a [runner box](runners.md) that isn't connected doesn't count until its box is back and caught up. `--timeout` bounds the whole wait, the hub's answers included. The exit code says how it ended: 0 finished (the text says when its report says it gave up or needs verification), 2 waits on you (also on a card about a prompt held for it with `--after`), 3 failed, was interrupted, stopped or parked, 124 timed out. With several threads it returns at the first one; with `--all`, once each has, with the highest of their codes.

An agent can run it too, since it only reads. Its own commands time out, so it waits inside a [job](#jobs): `twr job run --name wait-web -- twr wait web`. The job ends when web does, and the agent is told, with web's report in the job's output.

### Send a prompt after another thread finishes

`twr send --after` holds a prompt until the threads it names have each finished their next turn, then sends it as if you had sent it then. It needs no orchestrator turn:

```sh
twr send api --after web "rebase on twr/web and fix what breaks. web's report: {report:web}"
twr send integrator --after web,db "merge twr/web and twr/db"
twr send --cancel 01J7Q            # drop it before it goes
```

`{report:web}` becomes web's report when the prompt goes, as it is: a placeholder inside a report stays text. It is the report of web's last turn; a turn that was interrupted has none, and `(no report)` stands in. A thread counts as finished as for `twr wait` (an attached one doesn't, until you leave it), but only with a turn that ended after the prompt was queued, so a thread that is idle now waits for its next one. One parked or stopped after that turn counts at once, whatever is queued for it, also prompts held for it with `--after`: how that turn ended decides. Five seconds pass after it settles before the prompt goes, in case its runtime starts a turn of its own.

The prompt goes as yours, `{report:…}` included: the other thread's report, agent text nobody reads on the way, reaches the thread with your authority, as if you had typed it. Use `{report:…}` where you would send that report on unread yourself ([security-model.md](security-model.md#text-from-agents)).

If a thread it waits for didn't finish well (its turn failed or was interrupted, or its report says it gave up or needs verification), a card on the hub asks you whether to send it anyway. `Send` sends it now, and `Drop` (or dismissing the card) drops it. The card doesn't hold anything back: if that thread then finishes a turn well, say after an automatic retry, the prompt goes and the card closes. Deleting either thread drops the prompt, for good: restoring the thread from the trash doesn't bring it back, and the delete card counts it among the queued prompts that go. `twr stop` on the thread it's for drops it too, with the thread's queue.

A prompt for the orchestrator can't wait: a message to it counts as yours from the moment you write it, so tell it now and ask it to watch the thread. Prompts can't wait for each other in a circle: `twr send web --after api` is refused while a prompt for api waits for web. The prompt shows in its thread's transcript, and its card reads `queued until web finishes`. `twr send --after` prints the id `--cancel` takes, and the socket API's `thread.get` lists the prompt under `queued_after`. It's kept in the event log, so a hub restart keeps it. The orchestrator's `send` tool takes `after` too, which is cheaper than a watch when all it would do on waking is send that prompt. [recipes.md](recipes.md#a-review-loop) builds a review loop from it.

## Jobs

A worker that has a long command to run (a test suite, a release build) can hand it to the hub as a job instead of running it itself:

```sh
twr job run -- cargo test --workspace          # from the worker's shell: a job for its own thread
twr job run --slots 2 --timeout 2h -- make all # 2 of the machine's job slots; cancelled after 2h
twr job run --thread api -- cargo test         # from your terminal: name the thread
twr job list                                   # queued and running jobs
twr job log 01J7Q                              # the end of a job's output (-n 200 for more)
twr job cancel 01J7Q                           # SIGTERM to its process group, SIGKILL 5 s later
```

A line you type in the TUI that starts with `!` runs as a job too, at once, without slots: see [tui.md](tui.md#shell-commands). `twr job run` returns at once with the job's id. The hub runs the command (as argv: `-- sh -c '...'` for a shell line) in the thread's worktree, with the environment its runtime gets, and its output, stdout and stderr together, goes to `<data dir>/jobs/<id>.log`. When it ends, the thread gets a prompt, as a turn its runtime started itself: `[twr job 01J7Q... cargo test] `cargo test --workspace` exited 101 after 14m 12s.`, the last 40 lines of its output and the path of its whole log. `--no-wake` skips that prompt.

An agent should let its jobs wake it, end its turn, and handle the result when the prompt comes, rather than start a quiet job and a watcher of its own. If its background work ends anyway with nothing to start the next turn (a `--no-wake` job, a watcher whose end the runtime didn't follow up), the hub wakes it itself: see [below](#when-background-work-ends).

A job runs in a session of its own, without a controlling terminal, also when the hub runs in your terminal: a command that reads the terminal or sets its modes (a password prompt, `stty`, a pager) gets an error instead of stopping the job. What the command starts stays in the job's process group, which a cancel ends. A process that starts a session of its own (`setsid`) or daemonizes leaves that group: `twr job cancel` doesn't stop it.

Every job waits for job slots, one by default (`--slots N` for more): the machine has `[threads] job_slots` of them (its CPUs by default), shared by every twr hub of this home, and each hub's jobs take them in the order they came. So when several threads build and test at once, their jobs queue instead of loading the machine all at once.

While a job is queued or runs it's one of its thread's background tasks: the card says `background`, the activity line says `job cargo test: running 6m 0s` (or `queued (2 ahead)`), as the job does now, and the turn that started it ends with an interim report. A job outlives its runtime: a settings restart or a crash doesn't end it, and neither a settings restart nor an automatic retry waits for it. `twr stop`, a forced park and a deletion cancel the thread's jobs, and its runtime can't start one while it's stopping or stopped; an attach doesn't wait for them, and the attached CLI can start jobs while the attach lasts. A job an agent started (its runtime or the attached CLI) doesn't wake a stopped thread when it ends. After a crash that failed the thread's turn, a job's wake waits in the queue with the rest until you send to the thread or resume it. A hub that stops cancels every job and records how each ended. Before a job's command runs, the hub records its process group. A job a hub didn't see end (it died) is recorded as lost when the next hub starts, after what is left of its process group is killed (also a process that cleared twr's variables from its environment; if the hub can't verify that nothing of it still runs, the job isn't recorded and its thread starts nothing until a later hub start verifies it), and its thread is told, unless the job was started with `--no-wake` or the thread is stopped or parked. Jobs run only on the hub's own box for now, and a thread may have at most 8 queued or running.

A job's processes count as the thread's agent, as its runtime's do ([Security](#security)): they can read the hub but change nothing in it, and they can't start jobs. A thread's agent cancels its own thread's jobs, and starts them only while the thread's mode lets it run commands: `manual` or wider. Both only from a process that still runs under its thread's runtime or attached CLI: the hub checks again at each request, so a process that left them (its parent exited) or outlived them still reads but starts and cancels no job. In `plan`, `dontAsk` or `read-only`, or in an ACP agent's own mode that twr doesn't know, its `twr job run` is refused; give the thread a mode twr knows, such as `manual`, if it should run jobs. The mode is checked when the job is queued and again when it starts: a job still waiting for its slots when the mode narrows ends as `failed_to_start`, saying why. A job that already runs keeps running when the thread's mode changes. Like transcripts, every thread's jobs are open to read: any agent can list them and read their output. Your own jobs work in any mode. The orchestrator runs none. A job runs outside the runtime's own sandbox, as the hub's child, so it can do whatever you can, like a command an agent hands to `systemd-run --user`. Only an agent that can reach the hub's socket can start one: Codex's sandbox blocks local sockets in every mode but `auto` and the bypass modes (`bypassPermissions`, `danger-full-access`, `yolo`, which run without a sandbox; [runtimes.md](runtimes.md#permission-modes)), and `auto` already lets an agent reach outside it. Claude Code has no OS sandbox unless you configure one. If a runtime's sandbox is your boundary, keep its threads from the socket.

## Resource limits

By default every worker on the hub's machine runs as the hub's child, with the whole machine to itself. Two settings limit them, both off unless you set them:

```toml
[threads]
cpu_weight = 20      # the workers' CPU weight, together (systemd's default for anything else is 100)
memory_high = "8G"   # each worker's memory before it's slowed down (or "25%" of the machine's)
```

With either set, the hub starts each local worker's runtime, and each job, under `systemd-run --user --scope` in `twr.slice`, and sets that slice's CPU weight (until the next boot). The workers then share the CPU by weight with everything else your systemd user manager runs: at 20, when the CPUs are busy, they get together about a sixth of what its other units (your desktop apps, a hub running as a user service) ask for, so those stay responsive while the workers build. A login session (an SSH or mosh login, where the hub and the TUI often run) is outside the user manager, so moving the workers out of it already keeps its share for it. Over `memory_high`, the kernel reclaims a worker's memory (its runtime's and everything it starts) hard and slows it down, which takes pressure off the rest of the machine. It's a threshold, not a cap: a worker can still go over it, and the machine can still swap or run out of memory. `systemctl --user status twr.slice` shows each worker's scope with its CPU and memory use.

A scope's runtime is still the same process, the hub's child, so stopping it, the agent guard and twr's cleanup see it as before. The hub checks for systemd once, when it starts, and finds `systemd-run`, `systemctl`, `nice` and `env` on its own `PATH`, keeping each by its full path (a relative `PATH` entry is taken from the directory the hub started in, never from a worker's worktree): `twr info` says `limits: workers under systemd` (`worker_limits` in `twr info --json`). Without it (macOS, a container without a user manager) `cpu_weight` becomes a niceness (`nice -n`, `limits: workers under nice`) and `memory_high` does nothing; the hub's log says so. When systemd runs the scopes but won't set the slice's weight, the weight becomes the niceness too (`limits: workers under systemd and nice` with `memory_high` set). The orchestrator and `twr attach` always run unlimited, and so do a runner box's workers. Changes apply to runtimes and jobs started after the hub restarts.

## Answer permissions and questions

When an agent wants to run a command or edit a file that needs approval, its card in the TUI turns into a permission card. Select it (Tab), press Enter for its action popup, then `1` to allow or `2` to deny. A question card lists its options the same way. From a script or another terminal:

```sh
twr attention                         # what's waiting on you (ids as <attn_id>@<revision>)
twr respond api allow                 # show api's top item, answer it after you confirm (y/N)
twr respond api allow -y              # same without the prompt (answers exactly the item printed)
twr respond 01J...:1:perm-3@42 deny   # answer that revision of an item
twr respond 01J...:1:q-1@57 2 -m "use the staging DB"   # option 2, with a message
twr attention --all                   # also reviews, errors, stalled notices, routing questions
twr attention dismiss 01J...@57       # close one of those without answering it (dismiss in the TUI's popup)
```

An answer is an option label (`allow`, `deny`) or its number. A question without options takes free text.

`twr say` never answers a permission or question. It can't tell which ones you had seen. A message that looks like an answer ("yes", "allow", "2", "allow both", "@api yes, @web no", "allow api and then summarize") while one is pending isn't sent anywhere either, not even to the orchestrator. `say` lists what's pending and the `twr respond <id>@<rev>` that answers it. The TUI's input line does answer the card on screen, because the TUI tells the hub what you had seen.

When you tell the orchestrator to answer or change a mode, the hub applies it at once only when your whole message is one short command, such as "allow both", "approve atlas, deny the webapp one" or "set api to plan mode", and only for permissions. Anything else ("allow teh webapp", "allow api if the tests pass", "allow api. What changed?", any answer to a question) is not applied yet. The hub puts what the orchestrator proposed on a confirm card for you, with Confirm and Cancel. `twr attention` lists the card on the `hub` thread, and `twr respond <id>@<rev> Confirm` applies it. See [orchestrator.md](orchestrator.md#short-commands) for the exact forms and [the confirm card](orchestrator.md#the-confirm-card).

Items the hub raised itself can be dismissed: a "which thread should get this?" question, a confirm card, a review, an error, a stalled notice (a hung-turn card's dismissal is `wait`). A permission or question from an agent can't be dismissed, because the agent is waiting for the answer. Answer it or stop the thread.

### Changed questions

An agent can re-send a permission with changed input while it waits. twr never lets an answer to the old question reach the new one. The adapter closes the old item and raises the new request as a new item, with a new id ending in `~1`. An answer that was on its way to the old id is dropped, and the agent keeps waiting on the new card.

Each answer also names the revision it was given for: the card on screen, or the `@revision` part of the id. If the item was raised again since, the hub refuses the answer with "the question changed; look again". A bare attn_id only answers an item that was never raised again. In the TUI the action popup only answers an item that has been on screen in it for 600 ms, since a key pressed just as the item changed was meant for the one before.

## Reports

When a worker's turn finishes, the hub writes a short report from the agent's last message (its own summary section, such as `## Summary` or `TL;DR: ...`, when it has one) and grades the turn `success`, `partial`, `gave_up` or `needs_verification`. The orchestrator can replace it with a better one. A `gave_up` or `needs_verification` grade raises a review card. A report is the summary alone; to open the agent's own CLI on the thread, run `twr attach <name>` ([below](#attach)), or in the TUI `/attach` or the `attach` row of the card's action popup, which show that command. See [orchestrator.md](orchestrator.md#reports).

A turn that fails with a rate limit, an overload or a network error gets one automatic retry. Any other failure, or a failed retry, raises an error card.

A running turn or background task with no events for `stall_after_secs` (10 minutes by default) gets a stalled card. It closes by itself when the thread does anything. A background task's heartbeat, which only says it is still alive, doesn't count. While one of the thread's own jobs runs there is no stall: its turn, or the watcher its runtime left running, waits on the job. Once the job ends, the silence counts again.

A turn that stays open and silent while something waits to start the next one (a job's end, a message you sent) gets a hung-turn card instead, after `stall_after_secs` or 5 minutes, whichever is shorter: "api's turn 35 seems hung: no output from its runtime for 5 min while the end of job 01M5... waits to start the next turn". `interrupt` interrupts the turn, as sending the waiting message now does, and what waits runs next; `wait` closes the card, and no other hung-turn card comes for that turn. It notifies as `hung`, which is on by default, unlike `stalled`.

### When background work ends

A worker's turn can end while work it started still runs: its runtime's background commands and watchers, or its jobs. That turn's report is interim, and the result comes in the next turn, which the runtime normally starts itself once the work ends, or which a job's prompt starts. If the work ends and no turn starts within `wake_after_secs` (30 s by default; `0` turns it off), the hub wakes the worker with a prompt of its own: `[twr] Your background work finished after your turn 3 ended, and no turn started after it, so twr is waking you (once).`, the tasks that ended (each job with its result and the path of its log), and that it should continue. The thread's conversation says twr woke it, and so does the orchestrator's.

The hub wakes a worker once per interim report, and at most 5 times in a row without a message from you or the orchestrator in between; past that it says it did not wake it, and the worker waits for a message. It doesn't wake a thread you stopped or parked, one attached, failed or waiting on you, or one that already has a message queued.

With Claude Code, the end of a command the agent ran in the background opens the turn Claude starts for it right away, before its first output, which can take a minute. Nothing is written into that run meanwhile: since Claude Code 2.1.295, a message folded into a run Claude started itself ends without the `result` that closes the turn. If a turn's last message stopped and no `result` comes within 2 minutes anyway, the adapter ends the turn itself.

## Attach

```sh
twr attach api
```

This pauses the thread and opens the agent's own interactive CLI on the same session, in your terminal. When you exit, the thread picks up where you left it. Prompts sent meanwhile wait. If a turn or background work is running, `twr attach` refuses unless you pass `--force`, which interrupts the turn and stops the background work. If the thread's mode, model or effort changes while `twr attach` waits for the agent to stop, it refuses with "changed while attaching; try again": run it again to get the CLI with the new settings. An ACP agent whose preset's `attach` can't pass a new model or effort is refused until the thread has run with it: send the thread a prompt first ([runtimes.md](runtimes.md#models-and-reasoning-effort)).

If `twr attach` itself is killed or its terminal goes away, the thread stays attached until the agent's CLI has exited, so nothing else runs on that session meanwhile.

The first time you attach to a Claude Code thread's new worktree, Claude asks whether you trust the folder. Choose Yes. The default, No, exits and hands the thread straight back. `twr attach` reminds you on the first attach. twr doesn't answer that question for you or edit Claude's settings.

The TUI can't hand its terminal over, so `/attach` in the TUI prints the command to run in another terminal. Threads on a remote box attach over `ssh -t` ([runners.md](runners.md)). The `pty` runtime has no attach, and an ACP agent needs an `attach` command in its preset ([runtimes.md](runtimes.md)). The orchestrator can be attached only on Claude Code ([orchestrator.md](orchestrator.md#memory)). A thread whose session id isn't one twr passes to a command line (letters, digits, `.`, `_`, `:` and `-`, not starting with `-`, at most 256) isn't attached: a runner box reports its threads' session ids, and an id such as `--dangerously-skip-permissions` would be read as an option of the agent's CLI.

## Park, resume, stop

```sh
twr park api          # stop it, remove its worktree if clean (the branch stays)
twr trim api          # delete its build output (target/, ...); it keeps working
twr park api --keep   # stop it, keep the worktree
twr resume api        # recreate the worktree at the same path, same session
twr compact api       # compact its context, as its agent's own /compact does
twr compact api --offers off   # no more offers to compact it once its prompt cache expired
twr stop api          # stop the runtime and drop its queued prompts
```

`twr compact` runs a compaction of the thread's context as a turn of its own, after the turn that runs and the messages queued before it (`/compact-context` in the TUI). Claude Code, Codex and Grok Build threads on this machine can; [runtimes.md](runtimes.md#context-compaction) says what each does on its own, how to tune it, and what twr shows of it.

When a Claude Code worker's prompt cache has expired and its context is large, the hub holds the next message to it, yours or the orchestrator's, and asks you on a card whether to compact first (`[threads] idle_compaction`, [runtimes.md](runtimes.md#the-prompt-cache)). `twr compact <thread> --offers off` turns those offers off for that thread without compacting anything, as the card's `Don't ask for <thread>` does (with `before_expiry`, the hub's own compaction of it too), and `--offers on` turns them back on: `api: offers to compact its context once its prompt cache expired are off`. Only you can turn them on or off, and only you answer the card.

`park` refuses while a turn or background work runs, unless you pass `--force`. Prompts sent to a parked thread wait for `resume`. [worktrees.md](worktrees.md) has the rules for what parking removes.

## Delete threads

```sh
twr delete api           # asks y/N, then stops it and moves its worktree and twr/api to the trash
twr delete old older -y  # several at once, without asking
```

Deleting stops the thread's runtime, moves its worktree (even with uncommitted changes and ignored files, but on Linux and macOS not its build output, see below) and its branch (even with commits that are on no other branch, tag or remote) to the trash, and drops its queued prompts. The thread leaves every listing at once and its name is free again, but nothing is gone yet: the trash keeps it for 7 days (`[trash] keep_days`), and `twr trash restore` brings it back until then, as a copy of what the trash kept (see [Trash](#trash)). Parked and stopped threads can be deleted too; a parked thread's worktree may be gone already, and then only its record and branch go to the trash. A thread without a worktree of its own (`--no-worktree`, or a project that isn't a git repository) keeps its directory, puts nothing in the trash and cannot be restored.

Nothing is deleted until you confirm. The hub raises one delete card for all the threads named, on the `hub` thread, saying what goes with each one, such as `Delete thread api (project api)? This stops its runtime, moves its worktree ... (240 MB) to the trash with 3 uncommitted files it has, moves its branch twr/api to the trash, with 1 commit on no other branch, tag or remote (not pushed), and drops 2 queued prompts.`, followed by `The trash keeps it for 7 days, then purges it: ...`. A worktree past 1 GB in all adds how much disk the trash holds until then. `twr delete` prints that and asks `Delete api? [y/N]`. `y` deletes, `n` answers Cancel, and an empty reply leaves the card open for `twr respond <id>@<rev> Delete` or the TUI. Without a terminal it only leaves the card open, unless you pass `-y`.

The card still lists everything that is lost once the trash is purged, including what `git status` doesn't show: tracked files whose edits are hidden by `git update-index --assume-unchanged` or `--skip-worktree` (`with it 1 tracked file with edits git status does not show (...: config.local)`), and ignored files, by top-level entry with a file count, a few named and the rest counted (`with it its ignored files (.env, tools/ (12 files), +3 more)`). Every untracked file counts as an uncommitted file.

Build output is the exception, on Linux and macOS: it doesn't go to the trash. Just before the move, the box deletes the worktree's ignored entries that match `[threads] park_disposable` (`target/`, `node_modules/`, `.venv/` and the rest of the default list, the same ones a park deletes), unless one holds a git repository or might. A bare repository whose `objects/` is a symlink counts, and so does a directory that can't be read through. They're regenerated by the next build, and a Rust `target/` alone can be tens of GB, which the trash would otherwise keep for `keep_days`. Each entry is checked again as it goes. It's searched for repositories where it is, and one that holds a repository stays. Otherwise it's written down in the worktree's git record, moved aside inside the worktree, and deleted without following any symlink. If it turns out to hold a repository, or can't be deleted whole, it goes back where it was, and what's left of it goes to the trash with the rest. It never goes back over something that took its place meanwhile. Then it stays in the trash item apart, `twr delete` says so, and `twr trash restore` puts it back where it was. A restore puts back only what the trash item recorded, never through a symlink, and says where an entry stays if its place is taken. If the hub or runner stops in the middle of a deletion, the next `twr delete` first puts back whatever was moved aside, using what the git record says. If its place is taken, it goes into the new trash item instead, and a restore puts it back. The card says so with their size (`deletes its build output (target/, 14 GB) instead of moving it to the trash`), the worktree's size it shows leaves them out, and the trash item's manifest names them. A restored thread rebuilds them. To keep build output in the trash too, set `park_disposable = []` (which also makes parking keep worktrees that have any). When a worktree can't be inspected, the card says so (`which could not be inspected (whatever it holds goes with it)`) and never calls it clean. It also says when the branch existed before the thread (`moves its branch twr/api, which existed before the thread, to the trash ...`).

A worktree with a file system mounted inside it isn't deleted at all, since moving or deleting through a mount point would take that file system's files. The card names the mount points (`api's worktree has a file system mounted in it at target/shared: unmount it first`), and confirming it deletes nothing. Unmount them and ask again. On Linux the box reads `/proc/self/mountinfo`, on macOS it asks the system for its mounted file systems (`getfsstat`), and either refuses the deletion if it can't read the list. Other systems have no such list, so build output is deleted only on Linux and macOS. Elsewhere it goes to the trash with the rest of the worktree, the card lists none, and a mount isn't refused: a mount inside the worktree is moved, copied and purged with it. A mount made inside a trash item later isn't protected either.

This protects against symlinks and repositories that are there when the deletion runs: it never follows a symlink it meets. It doesn't protect against another process writing into the worktree during the deletion: swapping a parent directory for a symlink, or creating a repository while it deletes. twr stops the thread's runtime first, so only a process of yours could do that.

Repositories inside the worktree go too. Git keeps the repository of each submodule you initialized in a worktree under that worktree's own git directory (`.git/worktrees/<id>/modules/<name>`), so it goes with the worktree's record, with every commit in it, and the card counts every git repository in there, also when the worktree's directory is already gone. The same goes for a repository nested in the worktree that git doesn't track: an untracked one, or one inside an ignored directory such as `vendor/`, whether it has a `.git` or is a bare repository (`vendor/backup.git`). The card names each such repository that has commits on none of its remote-tracking branches: `with it the repository of its submodule libs/x with 1 commit on none of its remote-tracking branches (not pushed)`, or `with it the git repository nested at tools/x with ...`. This holds when the superproject looks clean, and for a submodule that isn't checked out any more (it's named by its git directory). Repositories inside repositories count the same way. A repository's uncommitted, hidden and ignored files count with the worktree's, under its path. A linked worktree (of another repository, or another worktree of the same one) checked out inside the worktree is different: its git record stays in its own repository, and after the move that record would point at a missing directory, so the next `git worktree prune` there (which `git gc` runs) would drop it, and `git gc` would then drop the commits only its `HEAD` or reflogs reach. So twr refuses to delete such a thread: the card says `It cannot be deleted now: api's worktree contains a linked worktree of another repository at <path> (its record is <record>): move or remove it first (...)` with the `git -C <repo> worktree move` / `remove` commands to run, confirming changes nothing (an error says the same and the thread stays, parked), and the box refuses the move before it touches anything. Move it out with `git worktree move` (or remove it), then delete again. Ignored directories are searched for repositories up to 200,000 entries in all; past that, the card says `could not search vendor/ to the end for git repositories` rather than claiming there are none. If a repository or its files can't be read, or a directory looks like what is left of a git repository that git won't open, the card says `(not checked: ...)` for it instead of reporting nothing. File names that aren't plain text show escaped (`\\`, `\t`, `\xNN`), so two different names never read the same.

Commits are counted against what stays. The branch and its reflog go, and so does the worktree's record, with its `HEAD` and `HEAD` reflog, even when the directory is already gone. Everything else stays: other branches, tags, remote-tracking branches, `refs/stash`, notes, every other reflog (the main checkout's `HEAD` reflog included), and the `HEAD` of every other worktree, detached or not. The trash's own refs don't count as a copy that stays, since they are purged in their time. A commit is lost when only what goes reaches it. That covers a commit on the branch, a commit left only in the branch's reflog after you moved the branch back, and a commit made on a detached `HEAD` in the worktree. When you delete several threads at once, all of them count as gone, so a commit only two of them have is reported as lost, and each line says who shares it (`of its lost commits, shared with b: 1 commit (deleted too)`). Each box counts its own repositories, and threads created from two checkouts of the same repository count together. If a reflog, worktree record or commit can't be read, the card says `Not checked: unpushed commits (...)` for that thread instead of reporting zero. Keep in mind that a commit kept only by a reflog that stays is safe only until git expires that reflog entry (30 days for unreachable commits by default).

A thread may switch its worktree to other branches as it works, as a thread that integrates its own work does when it makes a new branch for each step. Its worktree is still its own, and deleting it works as usual. Its recorded branch (`twr/api`) goes to the trash with the worktree, and the branch the worktree is on stays in the repository, as do the other branches the thread made. The card names that branch and counts its commits that no other branch, tag, remote, stash or reflog has, so you know what isn't merged anywhere: `leaves the branch twr/api-final its worktree is on in the repository, with 3 commits on no other branch, tag or remote (not pushed)`. Uncommitted files and build output are counted from that checkout as usual. If the worktree moves to yet another branch before you confirm, nothing is deleted and a new card says so. twr still refuses when the worktree holds another checkout: it's on another thread's recorded branch in the same repository, its git record was made for another path and moved there (`git worktree move`), or git has no record of it leaving the thread's branch (no `checkout: moving from twr/api` in the worktree's `HEAD` reflog, and no restore made it). Then the card says `Not checked: unpushed commits (... holds another thread's checkout ...)`, confirming deletes nothing (an error says why and the thread stays, parked), and the box touches nothing. Switch the worktree back to a branch of its own, or move the other checkout away, and ask again.

Threads keep working while the card is open. Asking again for the same threads checks them again: you get the same card if nothing more would be lost, or a new card that replaces it (`This card replaces <id>: since it was raised, there is more to lose (...)`). A card is for the threads as they were when it was raised: if one of them was deleted by another card and then restored (under any name), confirming the old card deletes nothing and stops nothing. A new card for its other threads replaces it, starting `Nothing was deleted: since card <id> was raised, api was deleted and restored since (now api-2). This card leaves it out (ask again to delete it) ...`; `twr delete` asks again for the threads you name and gets a new card instead of the old one. Nothing stops before you confirm. Once you do, each thread's runtime is stopped (parked) and what goes is checked again. Deletions in the same repository take turns, from that check until their worktrees and branches have moved: if you confirm two cards whose threads share a commit, and each card said the other thread's branch keeps it, the second deletion checks again after the first is done, finds the commit lost, and deletes nothing. If there's anything the card didn't list (a new uncommitted, hidden or ignored file, build output it said would go to the trash, a commit, a commit in a repository inside the worktree, more queued prompts, or a worktree that can no longer be inspected), nothing is deleted: the threads stay parked, and a new card lists everything and starts `Nothing was deleted: since card <id> was confirmed, there is more to lose (...). Their runtimes were stopped and stay stopped.` `twr delete` prints that card and exits with an error, even with `-y`: answer the new card to delete or keep them. Once the check passes, it waits until every thread is gone and prints a line for each: `deleted api; moved worktree ... to the trash; moved branch twr/api to the trash; trash item 7KQ2M9XA (240 MB) (...)`. When part of a worktree couldn't be moved (the trash is on another file system and some files may not be removed after the copy), the thread is deleted all the same and the line says `left part of <dir>: it could not all be moved (no worktree any more), remove it by hand`: twr never deletes what is left blindly.

A deleted thread's earlier events stay in the log. If a step fails, that thread stays with an error card saying what failed (parked if stopping it worked; if its runtime did not stop in time it may still be stopping), what moved stays in an incomplete trash item and the rest where it was, and `twr delete` again finishes the job. Build output deleted before the failing step stays deleted. If everything moved but the hub could not record the deletion (a database error, a crash), the thread stays, takes nothing meanwhile, and an error card says its worktree and branch are in trash item X; the hub records the deletion itself as soon as it can (at its next start or trash round, or when the box's runner reconnects). The orchestrator can ask for a deletion too, but only you can confirm the card.

## Trash

```sh
twr trash list                    # alias ls; --json for the raw result
twr trash restore 7KQ2M9XA        # bring the thread back, copied (its id, a unique end of one, or its name)
twr trash restore api --as api-2  # under another name
twr trash empty 7KQ2M9XA          # purge one item now (asks y/N; -y doesn't)
twr trash empty --all             # purge everything (lists what it purges, asks y/N)
```

Each box keeps its own trash: the hub's under `<data dir>/trash/` (`~/.local/share/twr/trash/`, or `$TWR_HOME/data/trash/`), a runner's under its own data directory. An item holds the worktree's files, without its build output on Linux and macOS (`worktree/`), its git record (`record/`, with the repositories of its submodules), what its index had staged (`index-objects/`, a pack of what no kept commit has), a verbatim copy of the branch's reflog (`reflogs/branch`), and a `manifest.json` saying what it is. In the repository, refs under `refs/twr-trash/<item>/` keep every commit the branch, the worktree's `HEAD`, its reflogs and its own refs reached, so `git gc` can't remove them; the branch's reflog comes along. When the trash is on another file system, everything is copied first and only then removed from where it was; a socket or FIFO isn't copied and stays. Build output the deletion could neither delete nor put back is in `worktree/.twr-build-output.<n>/`, each entry with a `.from` file naming where it was. The manifest lists them, and a restore puts back only those. Deleting build output is safe against symlinks and repositories that are there when it runs, but not against another process writing into the worktree meanwhile (see [Delete threads](#delete-threads)).

`twr trash list` shows each item's short id, thread, project, box, when it was deleted and when it will be purged, its size on disk (ignored files count, and on systems other than Linux and macOS so does the build output; on those two it was deleted), what its delete card said it loses (`3 uncommitted files, 2 unpushed commits`), and whether it was restored already. Items on a box whose runner is offline are shown as recorded at deletion. The orchestrator can list the trash too (`list_trash`), but only you restore or empty it.

The hub purges items older than `[trash] keep_days` (7 by default) when it starts and every hour, on every box that is online. `twr trash empty` purges named items, or `--all`, right away: their files, record and refs go for good. It first lists the items it will purge (names and `--all` are resolved then) and asks, and then purges exactly those: a thread deleted while it asks stays in the trash, and if a listed item was purged meanwhile, nothing is purged (run it again). A purge only ever removes directories under the trash that have a valid item id and a manifest naming it, and refs under that item's `refs/twr-trash/<item>/`. It never takes an item that is being made, restored from or purged, and never an incomplete one (its deletion failed part way) that still holds the worktree's files, its record, or the only copy of any commit (one its refs or saved reflogs reach and no other ref, reflog, `HEAD` or worktree of the repository does; a branch of the same name made again since does not count unless it has those commits): get that back by hand (below). `twr trash list` marks such an item "incomplete, kept: holds ...". An incomplete item that holds nothing goes once it has been left alone for an hour.

`twr trash restore` brings the thread back on its box by copying from the item. It never moves or edits the item. It makes the thread's branch again from the item's ref (the branch it had, or `twr/<name>` for the name `--as` gives it) (with the branch's reflog, from the item's copy), has git add a fresh worktree on it at the old path (or `<name>-2` and so on, when that path is taken: something is there, or git still has a record of a worktree there, even a stale one, which a restore leaves alone; `git worktree prune` clears stale ones), and copies the saved files into it. Uncommitted edits, untracked and ignored files, nested repositories and symlinks come back as they were. On Linux and macOS, build output doesn't come back, since it was deleted: the next build makes it again. Build output the deletion kept apart comes back where it was. On other systems, build output was kept in the item like any other ignored file, and comes back with it. A tracked file you had deleted stays deleted. Symlinks are recreated as symlinks and never followed, and sockets, FIFOs and devices aren't copied (the result names them). The repositories of its submodules are copied into the new worktree's git directory, and each checked-out submodule, nested ones too, is connected to its copy, also when the worktree comes back at another path or under another name. When that can't be done for every submodule, the restore fails and removes what it made. The thread comes back parked, with its name, project, runtime, mode, model, history and session: `twr resume api` continues the same session if the runtime still has it. If another thread took its name meanwhile, or its branch exists again, the restore refuses and asks for `--as <name>`.

A restore costs you three things. Staged changes come back as unstaged edits: the index starts fresh from the branch, and the staged versions stay in the item's `index-objects/` pack until it is purged. If the worktree was on another branch than the thread's when it was deleted, the restore makes the thread's branch again and puts the worktree back on the branch it was on, as long as that branch still points where the worktree left it, isn't checked out anywhere and isn't another thread's recorded branch: `api is back (parked) ..., its worktree at ... on branch twr/api-final as it was when it was deleted (its own branch twr/api is back too)`. Otherwise (that branch is gone, has moved, is checked out elsewhere or is another thread's, or the `HEAD` was detached) the worktree comes back on the thread's branch with the saved files over it, and the result says so and why (`its worktree was on the branch feature at 1a2b3c4d5e6f; it is on twr/api now (the branch feature has moved since (to ...)), and that commit stays in the trash at refs/twr-trash/<item>/head ...`). Commits only that `HEAD` or its reflog reached stay in the trash's refs until the purge. And the item stays in the trash after the restore, so its disk is freed only when it is purged. `twr trash list` marks it `(restored as api on ...)`; it can't be restored a second time, and `twr trash empty` or the purge removes it as any other item, leaving the restored worktree and branch alone.

If a step fails, the restore removes only what it made (the new worktree, its record, the branch, and directories it made for the worktree while they are empty), never anything another program put in its way, and the item stays as it was, so you can try again. If the hub can't record the restore, it removes the new worktree and branch the same way. If the hub or the runner stops in between, the hub finds the restored worktree by a mark in its git record (`twr-restore`) and removes it when it starts or the runner reconnects, unless the restore was recorded.

Without the hub, you can recover an item by hand: `git branch <name> refs/twr-trash/<item>/branch` in the repository brings the branch back, and the files are in `<data dir>/trash/<item>/worktree`. Its `.git` file still names the record's old path; copy the pack files of `<item>/index-objects/` (if any) into `<repo>/.git/objects/pack/`, move `<item>/record` back to `<repo>/.git/worktrees/<name>` and run `git worktree repair <path>`, or just copy the files out.

## The log

The hub records everything in `twr.db` in its data directory. What agents said, thought and did in old turns leaves it for `archive.db` beside it once nothing reads it any more: a deleted thread's once its trash item is purged (every purge is recorded, and such a thread can't be restored), and of the other threads each turn's that ended more than `[log] retention_days` (30) ago and has its report, except its final reply and the thread's last turn ([configuration.md](configuration.md)). The hub does that every hour by itself.

```sh
twr log stats                     # sizes, rows per kind, checkpoints, what would move; --json
twr log compact                   # move it now
twr log compact --vacuum          # then rewrite twr.db to give the space back; the hub waits
                                  # (3 to 18 s for a 475 MB twr.db)
twr log checkpoint                # this build's checkpoint of the hub's state; --all, --json
twr events <thread>               # reads archived events too; a deleted thread's by its id
```

A hub starts from a checkpoint of its state, written by its own build, and folds only the events after it. `twr log checkpoint` writes one for the `twr` you run, with or without a hub running (it never starts one), so that a new build's first start does not fold the whole log: the development installer and `twr update` run it for every workspace after installing, and `twr hub stop` does before stopping a hub that runs another build (it prints a line saying what it wrote, and its `--timeout` counts from after it).

Transcripts and the TUI's dive show an old turn's final reply but not its tool calls any more; `twr events` has them.

## Standing context

Standing context for a project lives in `<repo>/.twr/context.md`. The hub prepends it to every prompt it sends to that project's worker threads, never to the orchestrator.

```sh
twr context show api
twr context edit api    # opens a copy in $VISUAL, else $EDITOR, else vi
```

If the context changed while you were editing, or the save fails, nothing is overwritten and your edit is kept in a temporary file whose path is printed. The hub refuses a symlinked `.twr/` or `context.md`, and writes the file inside the `.twr/` directory it checked (held open), so one swapped for a symlink meanwhile is never written through; reads at most 1 MiB of the file and injects at most 64 KiB. Threads on a remote box get the project's context from the hub's log, the last version set or captured, because the hub never reads files on another machine.

## The orchestrator's memory

The orchestrator keeps notes across its sessions and runtimes, one memory per workspace ([orchestrator.md](orchestrator.md#memory)). You can read, change and remove them:

```sh
twr memory list
twr memory show short-replies
twr memory edit short-replies   # or a new name for a new note
twr memory rm short-replies
twr memory path
```

## Subscription usage

```sh
twr usage          # how much is left in each subscription's usage windows
twr usage --json   # the hub's usage.get answer
```

```text
Claude (max), as of 40s ago
  5h   78% left, resets 15:20 (in 3h)
  wk   56% left, resets Wed 05:00 (in 4d)
GPT (prolite), as of 12m ago
  wk   58% left, resets Wed 07:20 (in 4d)
```

Percentages show what is **left** (`100 − used`, clamped to 0–100). The orchestrator's MCP `get_usage` uses the same meaning. The `usage.get` JSON (`--json`) includes `remaining_pct` and keeps the runtime's original `used_pct` for compatibility; neither is rounded.

It lists what the runtimes' sessions last reported: Claude Code's 5-hour and weekly limits, Codex's windows and Grok Build's credit period. A runtime nobody has used through twr since its window reset isn't listed. twr learns the numbers while sessions run and reads them again when a session starts and when a turn ends, at most every ten minutes per session; it never starts a model turn to find out ([runtimes.md](runtimes.md#subscription-usage)). Usage outside twr, such as a `claude` you run yourself, shows up at the next read. The TUI shows the same at the start of its status bar's last row ([tui.md](tui.md#subscription-usage)).

### What threads spent

```sh
twr usage --threads                     # every thread, all time
twr usage --threads --since 7d          # the last week (30m, 24h, 2w, a date, an RFC 3339 time)
twr usage --threads --by project        # or by runtime
```

```text
PROJECT  TURNS  IN    OUT  COST
api      4      1.2M  30k  $3.40
web      1      9.0k  1.0k -
total    5      1.2M  31k  $3.40
5 turn(s) since 2026-10-04T12:00:00Z. IN counts cache reads and writes; ...
```

This sums the tokens the threads' turns used, from the log (deleted threads included), and the cost where the runtime reports one. Claude Code reports a cost on every plan: the API price of the tokens, a measure of size rather than a bill on a subscription. Codex, Grok Build and ACP agents report none (`-`). `twr thread list`'s SPENT column and the TUI's grid tiles show each thread's total, and the orchestrator's `get_usage` ends with the five threads that spent most in the last 7 days. `--json` prints the `usage.threads` answer.

## Follow the log

```sh
twr events api --pretty           # one line per event; a streamed reply is one line
twr events api --follow --hydrate # NDJSON, with payloads moved to the blob store restored
twr control --follow --pretty     # the global control stream; a hub shutdown's stops fold into one dimmed line
```

`twr box list` lists the remote boxes, their state and the twr version they run, and `twr box status <box>` shows one in full; `twr box add <box> <ssh destination>` adds a box the hub reaches over SSH ([runners.md](runners.md#ssh-boxes)), and `twr box project <box> <project>` finds a project's repository on a box, or clones it there, and records where it is ([runners.md](runners.md#projects-on-a-box)).

`twr info` shows what the running hub uses: its boxes and the orchestrator runtime, and how loaded its machine is (`load: busy: CPU pressure 52%, memory pressure 0%, disk pressure 0%, load 5.1 on 4 cores`; see [tui.md](tui.md#machine-load)).

## Security

Workers on the hub's machine run as your OS user. They can read your files and use your credentials like any program you start. twr adds one guard on top. The hub refuses requests that come from a process it started for a thread, or from any child of that process, unless the request only reads or runs a [job](#jobs) for that thread. An agent can list threads, read transcripts, look at what's waiting on you, list jobs and read their output, and start and cancel its own thread's jobs while the thread's mode lets it run commands. It can't answer a permission or question, change a mode, delete threads, empty or restore the trash, or send a prompt to any thread. That holds for an agent working inside `twr attach` too. The refusal names the thread whose agent asked. Run those commands from your own terminal or the TUI.

The guard uses two signals. It follows parent processes all the way up, and every process twr starts for a thread carries a `TWR_AGENT` environment variable that its children inherit. A background job that a worker's shell leaves behind (`nohup cmd &`) loses its parent but keeps the variable, so it's still refused. So is a command that clears the variable while one of its parents still has it, and anything that descends from a twr hub or a `twr attach`: a hub's agents and an attached agent's CLI descend from them, your terminal doesn't. That also keeps the agents of another [workspace](workspaces.md) or `TWR_HOME` instance, whose processes this hub didn't start, from acting on this one. A connection the hub can't identify, or whose parent chain or environment it can't read to the end, counts as an agent too ([details](security-model.md#agents)).

It is not a sandbox. A process that both starts itself again without the variable and detaches from its parent escapes the guard (with `/proc` mounted `hidepid` there are more ways), and so does a command an agent gets a program you already run to start for it: a new window of your tmux server, a `screen` session, `systemd-run --user`, an IDE's or editor's remote server, `at` or `cron`. Unsetting the variable inside a running process isn't enough, because the system shows the environment a program started with. For a repository you don't trust, use the runtime's own sandbox, or run the workers as a separate OS user or in a container. A runner box ([runners.md](runners.md)) can be a container, and its workers can't reach the hub's socket at all.

[security-model.md](security-model.md) has the whole picture: what the orchestrator, jobs, twr's git, the trash and runners guarantee, and what twr doesn't protect against.

## Stop the hub and update twr

```sh
twr hub stop     # stop the hub and wait until it has exited (30 s, or --timeout S)
twr hub stop --all # stop all workspace hubs in this home; timeout applies to each
twr update       # install the latest release over this twr
twr update --version v0.2.0
```

Stopping the hub stops every local thread's runtime first, so a running turn ends. Threads on a remote box keep running under their runner, and the next hub picks them up. With no hub running, `twr hub stop` says so and exits 0. Bare `twr` starts a new hub.

`twr hub stop --all` stops `default` and every named workspace listed by `twr ws`, ignoring `-w` and `TWR_WORKSPACE`. It prints each workspace's result: `stopped`, `not running`, or `failed` with a reason, and exits non-zero if any failed. Stops run concurrently, with a separate 30-second deadline for each (`--timeout S`), including connection, startup and shutdown. A hub that has claimed its workspace lock or bound its socket is waited for, including one already shutting down; a hung hub cannot delay the others. A hub that starts after its workspace was checked is outside this scan. Agents cannot run it.

The command covers the current home: the usual directories, or the instance selected by `TWR_HOME`. It uses `hub.socket` for `default` and the standard sockets for named workspaces. Other `TWR_HOME` instances are separate: run the command under each home you want to stop. A hub started with an explicit `--socket` elsewhere is outside this set; its workspace's held lock causes a timeout and failure instead of `not running`. Stop it with `twr --socket PATH hub stop`. `--all` cannot be combined with `--socket`. Workspace identities are checked before any shutdown request.

An invalid config reports failure for `default` while named workspaces are still processed. If workspaces cannot be listed, that failure is reported and `default` is still processed. A hub already shutting down reports `not running` once its workspace lock is free and its local runtimes are stopped. At the deadline, one final process check gets at most 100 ms to confirm an exit in the last polling interval.

`twr update` picks the release (`--version`, else the latest on GitHub), downloads its `install.sh` and `sha256sums.txt` with their signatures, and checks both signatures with the release key built into twr. Only then does it run that `install.sh`, with the checksums it verified, for the directory this `twr` is in, so it replaces this binary wherever it lives. A missing or bad signature stops it before anything runs, and so do checksums that name another release than the one it picked, or that don't list the `install.sh` it downloaded. It won't touch a `twr` in cargo's bin directory. Update that one with `cargo install --path crates/twr-cli`, or remove it with `cargo uninstall twr-cli` and use the installer. The hub keeps running the old version. If its version differs from the new one, `twr update` tells you, and you restart it with `twr hub stop`, then `twr`. It doesn't restart the hub for you, because that would stop your agents. `TWR_INSTALL_SCRIPT_URL` points it at another copy of the installer, a mirror or a local `file:///` path. Its signature is then `<that URL>.sig`, and `sha256sums.txt`, its signature and the archive come from the installer's directory, checked the same way, so the mirror's release is the one installed.

When several workspaces are running, use `twr hub stop --all`, then reopen the workspaces you need (`twr`, `twr -w NAME`). The development installer (`docs/tools/dev-install.sh`) suggests this too. Pick a moment when stopping their local runtimes is appropriate.

After the restart, a `twr` older than the hub, such as a TUI started before the update, is refused with a message to update it. A new hub can also change the format of its database. An older twr then refuses that database as newer than itself, so you can't go back to it on the same data.

A TUI left open across an update starts the new build when it opens a workspace whose hub isn't running. When the hub it shows stops or restarts, the TUI says `hub closed the control stream`. What you send from it after that goes over a new connection to whichever hub runs there: a message the old hub never got is sent once, and one it may have acted on is not sent again (the status bar says to check). Quit and reopen the TUI to see the new hub. Any twr that says `the hub closed the connection before ...` lost its hub after it connected: the hub stopped, restarted or crashed. `twr info` says whether one runs.
