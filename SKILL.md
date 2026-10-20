---
name: twr
description: Drive twr, the local hub that runs parallel coding agents behind one orchestrator conversation, through its hub's Unix socket (NDJSON) or its MCP tools. Use it to list, create and prompt agent threads, change a thread's model, reasoning effort or runtime, ask to delete threads, follow the control stream, answer or annotate attention items the user decided, write turn reports, and read or set a project's standing context.
---

# twr

twr runs a hub daemon that keeps a registry of **threads**: coding-agent sessions (Claude Code,
Codex, Grok Build, any Agent Client Protocol agent through the `acp` runtime, or any
interactive CLI through the degraded `pty` runtime) each bound to a
project, a repository and a box. Everything is an append-only event log; thread
status, the attention stack and reports are derived from it. One thread may be the
**orchestrator** (the agent the user talks to); the others are **workers**. The orchestrator
is the user's one conversation: worker reports, questions and permission requests reach it as
hub notices, and it may relay an answer the user gave but never decide one.

Two ways in:

- the hub's **Unix socket** (newline-delimited JSON), for any program or agent with a shell;
- the **MCP server** `twr mcp --socket <path>` (stdio), which the hub gives the orchestrator.
  MCP tools use a bounded connection pool; creation and polling may make several socket calls.

## Socket location

First match wins:

1. `--socket <path>` on the command line, else `hub.socket` in the config file (`default`
   workspace only);
2. `TWR_HOME=X` set: `X/hub.sock` (config `X/config.toml`, data `X/data/`);
3. `$XDG_RUNTIME_DIR/twr/hub.sock`;
4. `$XDG_STATE_HOME/twr/hub.sock`, else `~/.local/state/twr/hub.sock`.

That is the `default` workspace. A named workspace `W` (`-w W`, else `$TWR_WORKSPACE`, which a
workspace's hub sets for what it runs) is a hub of its own, at `ws/W.sock` beside where
`default`'s would be (`X/ws/W.sock`, `$XDG_RUNTIME_DIR/twr/ws/W.sock`, ...), with its data in
`<data>/workspaces/W/`. `hub.info` reports it as `workspace`.

The config file is `$XDG_CONFIG_HOME/twr/config.toml` (default `~/.config/twr/config.toml`).
The socket is mode 0600 in a 0700 directory: only its owner can connect. Clients check the
other way too: right after connecting they compare the listener's uid (the socket's peer
credentials) with their own effective uid and refuse a socket of another user ("the hub
socket <path> belongs to uid N, not to you (uid M): refusing to use it"), or one whose
credentials cannot be read, before sending anything. Paths are limited to
107 bytes on Linux and 103 on macOS. `twr hub` runs the hub in the foreground; bare `twr` starts one if none answers; `twr hub stop` stops it.

For the user, `twr hub stop --all` stops `default` plus every named workspace in the current
home (`TWR_HOME` when set), ignoring `-w`/`TWR_WORKSPACE`. It uses the configured default
socket and standard named sockets, checks workspace identities, and stops local runtimes
first, just like a single stop. Stops run concurrently with a timeout per workspace
(`--timeout S`, default 30); each prints stopped, not running, or failed with a reason, and
any failure makes the exit non-zero. A hub that has claimed its workspace lock or bound its
socket is waited for, including one already shutting down; a later startup after its workspace
has been checked is outside this scan. A hub on another explicit
`--socket` is outside the set: its held workspace lock causes a failure, and the user must
stop it with `twr --socket PATH hub stop`. `--all` conflicts with `--socket`; other homes
need their own command. **Agents cannot stop hubs, including with `--all`, or create or
remove workspaces.** Bulk stop refuses agents locally before contacting any hub; the
existing shutdown RPC guard also applies.

## Framing

One JSON object per line, UTF-8, `\n`-terminated, at most 16 MiB per line.

- Request: `{"id": 1, "method": "thread.list", "params": {...}}` (`params` may be omitted or
  `null` when a method takes none). `id` is any u64 you choose; it is echoed back.
- Response: `{"id": 1, "result": ...}` or `{"id": 1, "error": {"code": "...", "message": "..."}}`.
- Stream item (after a subscribe ack, on the same connection):
  `{"stream": "control", "gseq": 42, "event": {...}}` or `{"stream": "thread", "event": {...}}`.

A connection may interleave requests and several subscriptions (at most 16 at once; the hub
serves at most 1024 in all, at most 256 of them to agents' connections; one more is
`conflict`); match responses by `id` and recognize stream items by the `stream` key. A malformed or oversized line gets
`{"id": 0, "error": {"code": "bad_request", ...}}` and the connection is closed. Params are
strict: unknown keys are `bad_request`.

Error codes: `bad_request`, `method_not_found`, `not_found`, `ambiguous`, `conflict`,
`box_offline`, `unsupported`, `unauthorized`, `relay_forbidden`, `internal`.

## Identity

Say `hello` first. Every connection is a **user client** `user:<client>` (turns it starts are
`from: user`) unless its `hello` carries the current orchestrator token (`TWR_ORCH_TOKEN`, made
by the hub for each orchestrator process and passed only to its MCP server): then it is the
**orchestrator** (source `orchestrator`, turns `from: orchestrator`), re-checked on every
request. A wrong or stale token, or the client name `orchestrator` without a token, is
`unauthorized`, and so is a `hello` without a token on a connection that already presented one
(it cannot turn itself into a user client). Without `hello` a connection is `user:cli-<n>`.
Client names are 1-64 of `[A-Za-z0-9._-]`. `protocol` is the event format the client reads
(currently 1, which added `turn.started` `from: runtime`). A `hello` with an older `protocol` is
`unsupported`, and so is one from twr's own clients (`cli`, `tui`, `orchestrator`) without it:
those are a twr older than the hub, told to update. Other clients may leave it out; they should
ignore enum values and kinds they don't know. The token reaches the MCP server through a 0600
config file in the data directory (never a command line), removed when the orchestrator's
runtime exits. A token is valid only while the runtime it was made for is the orchestrator's
live runtime: it dies when that runtime exits, and one made for a runtime that failed to start
is never valid. `twr mcp` refuses to start without `TWR_ORCH_TOKEN` (it exists only for the
orchestrator).

The orchestrator may call only: `hello`, `hub.info`, `box.agents` (no `refresh`), `config.show`, `thread.new`, `thread.list`, `thread.activity`, `usage.get`, `thread.get`,
`thread.send`, `thread.stop`, `thread.set_mode`, `thread.set_model`, `thread.delete`,
`trash.list`, `thread.park`, `thread.resume`, `thread.events`, `thread.transcript`, `thread.spawn`, `thread.spawn_status`,
`thread.subscribe`, `attention.list`, `attention.respond`, `attention.annotate`, `control.read`,
`control.subscribe`, `blob.get`, `report.write`, `context.read`, `context.set`,
`project.files`, `project.read`, `project.search`,
`orchestrator.watch`, `orchestrator.unwatch`, `orchestrator.watches` (not `say`, `attach.*`,
any other `orchestrator.*`, `trash.empty`, `trash.restore`, `box.add`, `box.install`,
`box.restart`, `box.projects`, `box.token` or `hub.shutdown`); anything else
is `unauthorized`.

**Agents**. Runtimes on the hub's machine run as the user's OS user, so the
hub also checks who connects. When a connection opens, it reads the peer's pid (`SO_PEERCRED` on
Linux, `LOCAL_PEEREPID` on macOS) and walks its parent chain to init. A connection from a process
twr started for a thread, or from any descendant of one, is that thread's **agent**. Those
processes are a runtime on the hub's machine (a worker's or the orchestrator's) and the
interactive CLI of `twr attach` once its client reported it, matched by pid and start time so a
reused pid matches nothing (a runtime whose start time could not be read is matched by pid
while it runs). A runtime counts from the moment its adapter has started its process, by that
pid, before the runtime reports itself (`thread.started`). Each of them also carries
`TWR_AGENT=<thread id>:<generation>` (an attach: `<thread id>:attach.<lease>`) in its
environment, and a peer whose parent chain shows no such process but which, or any process of
whose chain, has `TWR_AGENT` (any value; `/proc/<pid>/environ`, or `ps -E` on macOS) is an
agent as well: a background job its shell left behind, reparented to init, or a child that
cleared the variable under a parent that has it. A peer
whose chain cannot be read to the end (a loop, deeper than 4096, a parent that keeps exiting
mid-walk, the peer gone before it is read, or the peer's own record cannot be read or parsed
while the process table can; a command name that is not UTF-8 is read like any other) is an
agent of no known thread, unless its marker names one. A parent the process table does not show
(`/proc` mounted with `hidepid`, which hides root's `sshd` and `login` and `sshd`'s
per-connection process from the user: its record cannot be read, or it is missing while its
child still names it) ends the chain like init: the processes read below it decide, so the
user's SSH session stays the user. If the hidden parent has the pid of a process twr started,
the peer is an agent of no known thread. So is a peer whose chain is whole but
whose own environment cannot be read (it is not gone), or that of an ancestor of the hub's
user that started after the hub (a process can hide it from its own user with
`PR_SET_DUMPABLE`), when no readable one has the marker; another user's ancestor (root's
`sshd` or `login`, a setuid or setgid program), one older than the hub (`systemd --user`) or
one whose parent is another user's (`sshd`'s per-connection process) does not count. A peer
nothing else made an agent is one of no known thread when a twr hub or attach (a `twr ... hub`
or `twr ... attach` command line, this hub's or another workspace's or instance's) is among its
ancestors: the user's terminal never descends from one. (On macOS the arguments come from
`ps`, without boundaries: words with a `/` are taken for paths.) On macOS (`ps`) start times are wall-clock
times, so a parent that looks younger than its child is still its parent; the chain is read a
second time instead and must show the same parents. `hello` and `hub.info` say `identity: agent:<thread name>`. An agent may call
only `hello`, `hub.info`, `box.agents` (no `refresh`), `thread.list`, `thread.activity`, `usage.get`, `usage.threads`, `thread.get`, `thread.events`, `thread.transcript`,
`thread.subscribe`, `attention.list`, `control.read`, `control.subscribe`, `blob.get`,
`context.read`, `trash.list`, `orchestrator.show`, `job.list` and `job.log` (every thread's
jobs), and, for its own thread only and from a process twr started for it (its runtime or
attached CLI, by ancestry; a marker alone is not enough, so a job's own processes start none),
`job.start` and `job.cancel`, the ancestry walked again at each such request (the process that
opened the connection must still be there and still run under the thread's live runtime or
attach: one that left them or outlived them gets `relay_forbidden`, "... it no longer runs
under the runtime or attached CLI twr started for <thread> ..."); `job.start` only while the
thread's mode may run commands (`manual` or wider). Anything else is `unauthorized`, with "this request comes from
<thread>'s agent (pid N, a process twr started for it): agents cannot answer, change modes,
delete, empty or restore the trash, or send prompts ..." (found by the marker: "(pid N, which
carries TWR_AGENT ...)", N the peer). The orchestrator token is accepted only from a peer
whose parent chain contains the current orchestrator runtime's process as the hub recorded it
(its MCP server); a marker never grants it, so another thread's agent, one of no known thread,
one found only by its marker (whatever thread the marker names) or a peer that counts as a user
client is `unauthorized` (a chain that does not show the runtime yet is walked once more after
200 ms). An orchestrator's background job that left its parent cannot present it either. The
chain is walked again at every `hello` with the token and at every request after it, against
the runtime the token belongs to: a process of an earlier orchestrator runtime that outlived a
restart cannot present the new token on the connection it opened then, and a connection whose
peer no longer descends from the runtime is `unauthorized` ("this connection no longer comes
from the orchestrator's current runtime ..."). The token also needs the process that opened the
connection: its pid and start time are read when it connects, and once that pid no longer names
that process (it exited and a process that inherited the connection uses it), the token is
refused on that connection.
Limits: a process that both clears its environment (execs again without
`TWR_AGENT`; unsetting it in a running process is not enough) and detaches from its parent (a
double fork, a background job whose shell exited) escapes the check; so does a command an agent
has a daemon of the user's that already runs start for it (`tmux new-window 'twr ...'` against
the user's tmux server, `screen -X`, `systemd-run --user`, an IDE's remote server, `at` or
`cron`): its ancestry and environment are the daemon's, the user's, which classification by
process ancestry cannot tell apart (for an untrusted repository use the runtime's sandbox or a
separate OS user); with `hidepid`, a process that clears its environment below a process the
agent made not dumpable (hidden from the user) escapes too, without leaving its parent; a peer
whose process table cannot be read at all (`ps` failing, no `/proc`; a missing record is a
gone process only while the hub's own can be read), and whose environment shows no marker,
counts as a user client. A peer whose credentials cannot be read, or name no pid the hub can
see (none, or 0: a process of another pid namespace), is an agent of no known thread
(`identity: agent:unknown`; "a process twr cannot identify"), except the hub's own process. Workers on remote boxes cannot reach the socket at all.

## Relay rule

twr relays; it never answers an agent's permission prompt or question on the user's behalf.
A worker's own shell is no user client: the hub refuses it everything but reads (see
Identity, **Agents**). Only an explicit `attention.respond` from a user client answers one, plus one guarded case: the
orchestrator may answer a permission or question only while its current turn was started by a
user message (not a hub notice), that message arrived (was queued) after the item was raised,
the message is a short command that gives that answer (below; otherwise it is a proposal for
the user's confirm card), only once per item per turn, and only while no newer user message to the orchestrator has arrived
(see below). A message routed by
`say` (or sent with `say {to: "orchestrator"}`) counts as arriving when the user last saw the
screen (`seen_gseq`; never, for `twr say`), so it cannot answer an item the user had not seen.
A newer user message supersedes an older one: every path that queues a user prompt for the
orchestrator (`say` delegated by the ladder, `say {to: "orchestrator"}`, a hub question answered
with it, `thread.send` to it; `Core::push_prompt`) records its arrival with it, as the
`decision.made{site: route}` that routes it, tagged `ext.twr.user_prompt_for` (`thread.send`
adds one, `provider: user, chosen: orchestrator`), and that decision is the turn's `routed_by`.
Once one arrived after the message of the turn in progress, that turn may neither relay nor
change a mode, whatever the newer message says (fail-closed: "thanks" ends it too):
`relay_forbidden` "the user sent a newer message or command; read the conversation before acting". The orchestrator
reads it in its next turn. Checked under the lock that records; rebuilt from the log on
replay. Hub notices are no user messages. A turn whose arrival is unknown (a log from before
it, a retry) relays nothing and changes no mode. A turn whose message the user took back
(`message.withdraw`, `turn.withdrawn`) has no authority left: its relays and mode changes are
`relay_forbidden`, and so is every other call of the orchestrator's that changes something
(`thread.new`, `thread.spawn`, `thread.send`, `thread.stop`, `thread.set_mode`,
`thread.set_model`, `thread.rename`, `thread.delete`, `thread.park`, `thread.resume`,
`attention.respond`, `attention.annotate`, `report.write`, `context.set`,
`orchestrator.watch`, `orchestrator.unwatch`): a call received in that turn stays refused when
the hub gets to it after the turn ended, and so does one received after it until the next
turn starts, even when the hub gets to it once that turn runs. A change asked by an
orchestrator that was replaced or restarted since the request came in is `unauthorized`. The first such call of a turn, like its first tool call, makes its message too
late to take back.

**Short commands** (`twr_decisions::code::command`). The orchestrator reads the user's message freely and calls
`attention.respond` or `thread.set_mode` once per action. It passes no evidence. Once
provenance holds, the hub reads the message itself and applies the call directly only when the
WHOLE message is one short command of the grammar below that covers the call. Anything else is
a proposal for the confirm card, which costs the user one key. The grammar is kept small on
purpose: free-text reading kept turning up corner cases.

The message. It is trimmed and may end with one `.` or `!`. Any other `.`, `!`, `?`, `¿`, `¡`,
`;`, `:`, `…` (full-width forms too), a line break or a control character makes it no command:
a command is exactly one sentence. At most 4096 bytes and 48 tokens. It is lowercased with
curly quotes and apostrophes straightened and split at whitespace; each `,` is a token of its
own. Nothing else is trimmed ("'api'", "(api)" and "*api*" are no vocabulary). Every token must
be consumed:

```
MESSAGE := CLAUSE (JOIN CLAUSE)*                at most 8 clauses
JOIN    := , | and | y | , and | , y
CLAUSE  := ANSWER | MODE
ANSWER  := AW [on | for | to | a] TARGETS
         | AW | AW it                           only as the whole message
MODE    := [VERB] TARGETS to [the] MODENAME (mode | modo)
         | [VERB] TARGETS to [the] STRONG
         | (pon | pasa | cambia) TARGETS (a | en) (modo MODENAME | MODENAME (mode | modo) | STRONG)
VERB    := set | switch | change | put | turn
TARGETS := GROUP (TJOIN GROUP)*                 at most 16 groups
TJOIN   := , | and | y | & | , and | , y | , &
GROUP   := [the | el | la] REF [one]
         | both | ambos | ambas | them both     exactly two candidates
         | all | todos | todas | every | everything | them all
```

- AW: one approve word or phrase (allow, approve, accept, yes, ok, okay, sure, go ahead,
  proceed, confirm, lgtm, sí, dale, adelante, aprueba, apruebo, permite, acepta, yep, yeah, yup)
  or deny word (deny, reject, decline, no, nope, block, rechaza, deniega). "no" and "nope"
  answer only as the whole message. The unaccented "si" is no answer word (it is also "if").
- MODENAME: auto, plan, manual, default, read-only or "read only", dontAsk or "don't ask",
  acceptEdits or "accept edits", bypassPermissions, "bypass permissions" or bypass; words apart
  or joined by nothing, `-` or `_`; or a one-edit typo of a mode of 4+ letters directly before
  "mode" / "modo" ("audo mode"; never auth, plain and other real words). STRONG is a mode
  spelled exactly that is not weak: a weak mode (auto, plan, manual, default) always needs
  "mode" / "modo" next to it.
- REF: a token spelled as a plain identifier (letters or digits at both ends, `-`, `_`, `.`
  inside), or `@` and one, that is exactly a thread's name, alias or id (any case), a name it
  had before a rename, or a whole `-` / `_` part of 3+ characters of its name ("webapp" for
  `orbit-webapp-1`, not "app" nor "orbit-webapp"). No typos, no projects. A word two threads
  match in any of these ways is no reference (`api` beside threads `api` and `x-api-y`, or
  beside a thread once named `api`), and so is a word an orchestrator thread (current or
  retired) or the word "orchestrator" matches in any of them. Vocabulary words, common English and Spanish function
  words (`command::STOPLIST`), negative pronouns and quantity words (`command::RESERVED`) and
  nouns such as agents, requests, permissions, worker, each (`command::NOT_NAMES`) and courtesy
  words and closing remarks such as please, thanks, thx, ty, cheers, gracias, cool, done
  (`command::COURTESY`, so "allow api, cheers" never approves a worker named `cheers`) and
  hedging, conditional and temporal qualifiers and indefinite choices such as conditionally,
  tentatively, maybe, later, either, whichever, quizás, luego, cualquiera
  (`command::QUALIFIERS`, so "allow api, conditionally" never approves `conditionally-1`) name a
  thread only with `@`, and so does a word holding one of them as a `-` / `_` / `.` part
  ("no-one", "never-1", "thank-you", "either-1") or made only of digits ("2", "1-2"). A thread
  name holding a `.` can be named only by a part. Accepted residual: a thread named (or
  aliased, or with a name part) after an ordinary word outside these lists can still be named
  bare; `@name` is the way to be exact, and adding words to the lists is maintenance.

Meaning. For answers the candidates are the items waiting and seen at `said_after`, never an
orchestrator's item or a confirm card. For modes they are the workers then, not stopped, never
the orchestrator or the hub thread. Every group must pick something out, else the whole message
is no command: for an answer a REF covers its thread's waiting item when it has exactly one (a
thread with none or several is no command); for a mode a REF covers its worker, even stopped;
"both" covers every candidate when there are exactly two; "all" every candidate, at least one;
a lone AW ("yes", "allow it") the only answer candidate when there is exactly one. A message
that parses two ways is no command.

The call is applied directly when an action of the command covers its target and every action
covering it gives the same answer or mode, but never for a lone AW: it names nothing, and in the
orchestrator's conversation the user may be answering the orchestrator's own question, so its
call goes on the card (`command::Coverage::Lone`), which names the item, and the hub's own fast
path (`hub::commands`) leaves it to the orchestrator. Nor when a thread a word of the message
could name (`command::named_threads`: any match, unique or not) was made or renamed after
`said_after` (by the arrival, or unknown because that history was dropped): the user wrote
against the names they saw, and the orchestrator makes and renames threads without a card. Only a permission takes an answer directly: approve
picks its allow option and deny its deny option (a label spelled as the answer word, else the
only option of that polarity; a permission listing no options takes `allow` / `deny`). A
question is never answered directly, whatever the wording (its options are labels: the user
answers on the card or with their own keys), and a deny-only permission is never allowed. A
`set_mode` call's mode is compared as the hub canonicalizes it for the thread's runtime,
exactly, with the mode the command names (twr's own id): a generic ACP agent's "p'lan", kept as
written, is never "plan".

Direct: "allow both", "approve all", "allow api and web", "allow on the webapp", "approve atlas,
deny the webapp one", "dale a ambos", "Allow both, and set them both to audo mode", "set api to
plan mode", "set both to audo mode". On the card: "yes" / "allow it" / "yep" (also with exactly
one item waiting), "Set both agents to auto mode. What were the latest changes?" (a second
sentence; the question still reaches the orchestrator), "approve both requests", "allow api.
Only if tests pass?", "please allow api", "si", "option 2", a label, "allow teh webapp", "allow
webapp" after a thread was renamed or made `webapp` since the user's frame.
Applied answers are forwarded as the exact option label, with no message or extra fields.

**Confirm card** (`hub::proposal`). Every other call that passes provenance becomes a proposal:
`attention.respond` and `thread.set_mode` return `{status: "needs_confirmation", ..., message}`
and nothing changes; so does `thread.set_model`'s move to another runtime unless the thread's
mode is restricted there (see its row). A provenance failure is still `relay_forbidden`. When the orchestrator's
turn ends (or its runtime stops), the turn's proposals go on one confirm card: a `question` the
hub raises on the hub thread with options `Confirm` and `Cancel`, a summary listing every
action ("From your message: allow api (Bash: git fetch ...) · set atlas → auto") and
`ext.twr.proposal = {orchestrator, turn, said_after, actions: [{kind: "answer", attn_id,
revision, thread, answer} | {kind: "mode", thread, mode} | {kind: "move", thread, runtime,
model?, effort?}]}` (a move reads `move api → codex, effort "high"`; Confirm makes it as the
user's `thread.set_model`, checked again, its `thread.model_changed{by: user}` with
`ext.twr.proposal`, and the new runtime's settings applied after the answers and modes). The summary renders exactly those
actions, losslessly (`twr_decisions::code::card`): thread names, labels, mode ids and item
summaries have control, format and invisible characters written as `\u{200B}` escapes (and
`\`, `"` and `·` escaped), a mode id the thread's runtime does not know is marked
`custom mode "plan\u{200B}"`, and a summary that does not read back as its actions raises no
card. `attention.list` marks it with
`proposal: true`. Actions that no longer hold when it is raised are left off; with none left
there is no card. Only a user answers it, with `attention.respond` or `attention.dismiss`; the
orchestrator gets `relay_forbidden` for respond and annotate. Confirm applies each action at
that moment as the user's, checked again (the item open at the same revision, its runtime
running, the thread a worker in the life it was proposed in: a mode change for a thread
deleted and restored since is skipped, `<name> was deleted and restored since the card was
raised`; `ext.twr.proposal`'s mode actions carry `lifecycle`, the seq of the thread's latest
`thread.created` or `thread.restored` when proposed): `attention.resolved{by: user}` and `thread.mode_changed{by:
user}`, both with `ext.twr.proposal` = the card's id. Actions that no longer hold are skipped.
The card's `attention.resolved` lists both in `ext.twr.confirmed: {applied, skipped}`, and it
and every applied action's events are one batch: a crash or write failure leaves all of
it or none of it, the card still open. Once it is recorded, every applied answer goes to its
runtime first, whatever the actions' order, and only then do restarts and dispatches run (the
answers', then the mode changes'), so a mode change never stops a runtime before it got the
answers to the items it raised between turns. The orchestrator gets a notice of it. Cancel or a
dismissal applies nothing. Answering a resolved card again is `conflict`, stating how it ended. A newer user message
to the orchestrator drops the proposals not on a card yet and resolves an open card in the
same batch (`by: user`, answer `superseded`). Open cards are rebuilt from the log on restart;
proposals not on a card yet live in memory and are dropped by a restart. The accepted cost:
anything but a short command, a command beside a question included, costs the user one
keypress on the card.

When an agent re-sends a pending request with new input, its
adapter withdraws the old item (`attention.resolved{by: timeout}`, `ext.<runtime>.superseded`)
and raises the new request under a new attn_id (`<id>~<n>`); an answer to the old id never
reaches the new request. The hub also handles an item raised again while open (a replay): that
is a new *revision* (the item's `revision`, the control gseq of its latest
`attention.raised`). A user's `attention.respond` names the revision it answers (`revision`,
what the client displayed; `twr respond <attn_id>@<revision>`); once the item was raised again
the answer is refused with `conflict` ("the question changed; look again"). Without a revision
it is accepted only while the item was never raised again. The orchestrator's answer must
postdate the current revision (above). All of this is checked under the lock that records the
answer. The answer is forwarded as the exact option label, without a
message or other fields. It may never resolve `error`, `review` or `stalled` items (it can
annotate them).
Everything else is `relay_forbidden`. MCP tool text:

> RELAY RULE: never answer a permission or question on your own judgment. Answer an item only
> when the user's latest message answers it: call respond_attention once per item with that
> answer (allow, deny, or the option's label or option_index). The hub reads the message itself
> and applies the answer directly only when the whole message is one short command: one sentence
> of answer words (allow, approve, yes, ok, go ahead, dale, deny, reject, no, rechaza, ...) and
> threads (a name, a part of one such as "webapp", @name, both, all), like "approve atlas, deny
> the webapp one", for permissions only. Anything else, and every answer to a question, returns
> status needs_confirmation: nothing is applied yet, and everything you proposed in this turn
> goes on one confirm card that only the user can confirm; then tell the user "I've put that on
> a card for you to confirm." Relay what the user asked as asked; never retry with other
> wording. Read the whole message first: if it takes an answer back, makes it conditional or
> asks a question instead ("allow it if the tests pass"), do not answer; ask the user. The hub
> refuses with relay_forbidden an item raised after the message or that the user had not seen,
> an item of your own, a confirm card, a second answer to one item, and anything once the user
> sent a newer message (read it first). Then tell the user what is pending.

**Mode rule** (`thread.set_mode` from the orchestrator): provenance as above, plus the thread
must have existed when the message arrived (`said_after`; unknown history is refused). Each
thread's change is used once per orchestrator turn, a proposal included (a retry that sets
the mode the thread already has passes as a no-op). A `thread.mode_changed` by the user after
the message arrived ends the message's authority for that thread, including one that keeps
the mode (`mode == previous`). A newer user message to the orchestrator ends it for every
thread (see above). Then a short command applies it, and anything else is a proposal.
MCP tool text:

> MODE RULE: a thread's permission mode is the user's setting. Call set_mode only when the
> user's latest message asks for that mode for that thread, once per thread. When the whole
> message is one short command ("set both agents to auto mode" is not one, "set both to auto
> mode", "set webapp to plan mode" and "pon atlas en modo plan" are) the change is applied;
> anything else returns needs_confirmation and goes on the confirm card, like an answer. Relay
> it anyway, and never retry with other wording. A mode named in passing ("tell api to plan the
> migration") is no request. Other requests in the same message ("set both to auto mode. What
> changed on staging?") go to the workers with send; the mode change then goes on the card. The
> orchestrator's own mode cannot be changed.

## Methods

`thread` params accept an id, name, alias or unique id prefix (`not_found` / `ambiguous`).
`since*` params are exclusive.

| method | params | result | notable errors |
|---|---|---|---|
| `hello` | `{client, token?, protocol?, user_token?}` (`user_token`: tests only, see docs/development.md) | `{client, identity, workspace}` (`"user:<client>"` or `"orchestrator"`; the workspace the hub serves) | `bad_request`, `unauthorized`, `unsupported` (a twr older than the hub) |
| `hub.info` | - | `{version, build, runner_protocol, drainable_protocols, listening, pid, db, instance, workspace, started_at, threads, control_head, open_attention, dropped_events, persist_retries, orchestrator: {as orchestrator.show, without installed}, boxes: [{name, transport, state, online, version, refused, last_error, notes, os, arch, build, running_build, switch_pending, restart_pending, epoch, launch, drain, agents}], agents, client, identity, load, disk, notify_hook, warnings}`. `build`: the hub's own build id (the first 16 hex digits of the SHA-256 of its executable; null when it cannot read it). `runner_protocol`: the runner protocol the hub speaks, which its boxes' runners must speak too (`twr box doctor` compares a box daemon's with it). `drainable_protocols`: the older runner protocols the hub still serves drain-only (`[6]` with protocol 7): a box whose runner speaks one is drained (`drain`, below) instead of refused. `listening`: the hub's runner listener is up (`[hub] listen`), so a token box's runner can connect (`twr box doctor` checks it for a dial-in box). `boxes`: each remote box the hub has (configured, or added since with `box.add`), by name; `transport`: `token` (its runner connects to the listener), `ssh` (the hub connects to it over ssh) or `command` (the hub connects to it through the box's `command`, a container's `docker exec -i` say); `state`: `online` (connected and ready), `offline`, or for a box the hub connects to (`ssh`, `command`) also `connecting` (probing, installing twr, opening the connection), `needs_install` (`twr box install <box>`: the box's OS or CPU is not the hub's, or an upload failed its checks), `switching` (the hub is switching the box to the build it launched there: its runtimes were stopped once it was quiet, their threads keep their sessions and their prompts wait, and the box's daemon is replaced; its notes say `switching to build <id>`, then `now on build <id>`; a daemon of another runner protocol is replaced at once instead, its runtimes recorded as stopped the same way (their threads' `thread.stopped` has `ext.twr.reason` `box_upgrade`, never a crash; a running turn ends `interrupted`), the notes saying `the box's daemon speaks runner protocol <p>, which this hub (protocol <n>) cannot serve: replacing it; its <k> agent(s) stop, and each thread's next prompt resumes its session (<t> running turn(s) cut)`; meanwhile a new thread, park, resume, deletion, trash restore, attach or answer on the box is refused with `box_offline`, saying the box is switching to the hub's twr build and to try again in a minute) or `error` (the hub's box key cannot be loaded); `switching` also covers a drain (`drain`, below), except that a box the hub connects to shows `needs_install` or `error` while it is offline in that state; `version` is the twr version its runner last reported (`0.2.0`, or `0.2.0+dev.<commit>` for a development build; null before any connection); `refused`: while the box is offline, the latest connection naming it that the hub refused before it authenticated, `{at, reason}` (a runner of another protocol, one that did not accept the hub's proof, a bad proof), or null. Anyone who reaches the listener can name a box, so `refused` proves nothing about who connected. Boxes the hub connects to (`ssh`, `command`) only (null for a token box): `last_error` (why the latest attempt failed, one line: a refused ssh key says to load it into an ssh-agent the hub can reach, since the hub's ssh never prompts), `notes` (the box's latest status lines, such as a warning that logind will end its agents with the ssh session), `os` / `arch` (`uname -s` / `-m` of the box), `build` (the twr build id the hub launched there), `running_build` (the build the box's daemon runs, as it said when its connection was welcomed; null while it is not connected) and `switch_pending` (the box runs another build, of the same protocol, than the one launched, or than the one `twr box install` installed since for a box of another OS or CPU: the hub switches it once it has been quiet for 2 minutes), `restart_pending` (a `box.restart` was asked for and no connection has carried it out yet) and `launch` (how the box's daemon says it was started, from its inventory: `launchd <label>`, a launchd job in the GUI session of the user logged in at a Mac's screen, label `tools.twr.box.<hash>.<random>`; `systemd <unit>`, a transient systemd user service `twr-box-<hash>-<random>.service`; `detached`, in a session of its own from ssh, which on a Mac means Claude Code can't read its keychain login; null before any connection or from an older twr; kept while the box is offline). `epoch` (every box): the hub's number of the box's current connection, larger for each new one (it starts over when the hub restarts), null while the box has none. `drain` (users only: the orchestrator and agents see `state` alone; null otherwise): the box's runner speaks an older runner protocol that the hub drains (after a hub upgrade across a protocol bump), `{protocol, since, until, stage}`. `stage`: `draining` (its running turns may finish and stream until `until`, 30 minutes after `since`, and answers to them go through; prompts to its threads wait; a new thread, park, resume, deletion, trash restore, attach or trim on the box is refused with `box_offline`, saying why; the hub sends the runner nothing but acks, pings and the `send`, `respond` and `stop` of sessions already running, so its git snapshots, disk readings and agent probes are skipped), `stopping` (once the box is idle, or at `until`, or on `box.restart`, its agents were stopped for the upgrade: their threads' `thread.stopped` has `ext.twr.reason` `box_upgrade`, never a crash; a turn still running then ends `interrupted`), `drained` (a token box once its agents have ended: its runner stays connected and the box gated until the runner is updated and restarted at the hub's protocol; restarted earlier, its agents end as crashes) or `needs_install` (a box of another OS or CPU whose drained daemon runs its configured build: `twr box install`). A box the hub connects to has its daemon replaced once its agents have ended (`--replace`); one whose connections do not get ready (none syncs within 2 minutes, three times, or `until` passes first) is replaced as a daemon of a protocol the hub cannot serve, its agents recorded as stopped for the upgrade first. The reconciliation of a connection of the hub's protocol ends the drain and runs the queued prompts, each thread's next prompt resuming its session. The notes say each step (`draining: the box's daemon runs twr <v> (runner protocol <p>; the hub speaks <n>): running turns may finish until <HH:MM> UTC; new prompts wait`, `drained: stopping <k> idle agent(s)`, `drain time is up: stopping <k> agent(s), cutting <t> running turn(s)`, `a restart cuts the drain short: ...`, then `switching to build <id>` and `now on build <id>`). A hub restart forgets the drain: the next hub drains again, with a new deadline. A box of another OS or CPU whose configured build speaks another runner protocol than the hub (after a twr upgrade) is `needs_install`, `last_error` "box <b> runs a twr build that speaks protocol <n>, the hub <m>: install this hub's build there: `twr box install <b>`": that build is not launched again until `box.install` (or 5 minutes later). `agents` (each box's; top-level `agents`: the hub's own box, `local`): the box's agent CLIs as it last reported them, one per built-in agent runtime, `[{runtime, found, path?, version?, login: logged_in \| logged_out \| unknown, method?, note?}]` (`path`: the executable the box would start, resolved as its runtimes are, `TWR_*_BIN` first, else on the login shell's `PATH` before the runner's; `version`: the first line of its `--version`; `login` from `claude auth status` / `codex login status`, `unknown` for Grok Build, which has none, or when the status could not be read; `method`: Claude Code's sign-in kind, `claude.ai`, `console`, `api-key`, `bedrock`, `vertex` or `other`; `note`: why something is not known, a fixed phrase; never an account's email, organization or plan). The hub applies the probe's filters again to what a box reports, whatever its twr: built-in agent runtimes only, once each; `method` for Claude Code only, any word but those as `other`; a `note` that is none of the probe's phrases becomes "the box reported something else"; strings printable and short. A remote box reports them in its inventory (empty when its runner has not finished its first probe), then the hub asks it again (`Op::Agents`) 30 s after each connection and every 10 minutes, and on `box.agents` `refresh`; the hub probes its own in the background as it starts and every 10 minutes, with its own environment (no login shell). Empty: nothing reported yet. The orchestrator and threads' agents get of each box only `name, state, online, version, agents`, and of each agent only `runtime, found, login`, top-level `agents` included (the rest of `hub.info` as users get it). `notify_hook`: whether `[notify] command` is set ([notifications](docs/notifications.md)). `warnings`: what is wrong with the configuration the hub started with but did not stop it, one sentence each (also in its log): a box token shorter than 32 characters, a `config.toml` or `boxes.toml` holding a box token (or a `config.toml` a `[notify]` command) that other users can access, naming that file (`chmod 600` it). `disk`: each box's free space where it keeps its worktrees, as the hub last read it (every 5 minutes), `{<box>: {free, total, at, low}}` (`low`: under `[threads] disk_warn_gb`). `load`: how loaded the hub's machine is, `{level: calm\|busy\|saturated, summary, cpu_pressure, memory_pressure, io_pressure, load1, cores}` (pressures are the kernel's 10-second pressure stall averages in %, `io_pressure` the `full` line; null when unknown, and then the load average per core decides), or null | |
| `config.show` | - | the configuration the hub loaded when it started, built from an allowlist: `default_runtime`, `project_roots` (+ `project_roots_default`), `stall_after_secs`, `background_timeout_secs`, `orchestrator`, `threads` (`env_names`, not values), `trash`, `projects` (`repo, runtime, box, worktree, base, trim_idle_after, idle_trim_disposable, env_names`), `runtimes` (preset names only), `boxes` (every box the hub has, those `box.add` added included: `transport`, `has_token`, `projects`), `hub` (`socket, listen, tls: bool, allow_insecure_remote`), `tui`. Never a token, an `env` value, a preset's command line, a box's `ssh` or `command`, or a TLS file. Not for agents | |
| `hub.shutdown` | - | `{ok: true}`, then the hub stops its runtimes and exits (each local one's `thread.stopped` carries `ext.twr.reason: "hub_shutdown"`, idle ones included: the log must show the process gone) | |
| `thread.new` | `{project, runtime?, name?, mode?, model?, effort?, aliases?, box?, worktree?, no_worktree?, base?}` (`effort`: as `thread.set_model` takes it, recorded as `thread.created` `ext.twr.effort`; blank or `default`: the runtime's default) | thread object (+ `project_found: {name, repo}` when `project` was a repository folder name found under `project_roots`; + `warning` when the thread's box last reported its built-in agent runtime's CLI not found: "box <b> reports no `<cli>` on its PATH; the thread may fail to start: run `twr box doctor <b>`", for `local` without the doctor; advisory only, the thread is made: a thread may configure a `PATH` of its own) | `not_found` (project neither configured, an absolute directory, nor (hub's own box only) the folder name of a git repository at `<root>/<name>` or `<root>/*/<name>` under `project_roots`; the message lists the folders searched), `ambiguous` (that folder name matches several git repositories, all listed, or the search was cut short by its limits), `bad_request` (`worktree` with `no_worktree`, unknown runtime, a `mode` that is not a shared id (`manual`, `default`, `plan`, `read-only`, `dontAsk`, `acceptEdits`, `auto`, `bypassPermissions`) or the runtime's own (generic ACP: any its agent takes), a bare `pty` / `acp` or a `pty:` / `acp:` command (or preset) that does not split, bad name, a `base` that is not a commit or given with `no_worktree` / outside a git repository, the orchestrator giving a `mode` other than `threads.default_mode`, or a `runtime` other than the project's (else `default_runtime`) that is neither a built-in agent runtime (`claude-code`, `codex`, `grok-build`) nor a `[runtimes]` preset, or, with none, the project's runtime being `fake` or `pty:*`, an `effort` the runtime does not take, as for `thread.set_model`, or a `project` that is not a configured project and not inside a git repository, or whose repository is or holds the home folder, or holds or is in twr's data directory, `TWR_HOME` or a credentials folder of the home folder (on a remote box: `/`, a home folder by its shape or a path through such a folder)), `conflict` (name/alias taken, in any case: `API` is `api`; the worktree cannot be created, the project directory is in another thread's hub-made worktree), `box_offline` (the box is not configured, or its runner is not connected, or the SSH box is switching to the hub's build: nothing is created) |
| `thread.list` | - | `[thread object]`, oldest first (deleted threads are not listed) | |
| `thread.activity` | - | `{<thread id>: {activity, doing, said, task, at}}` for listed threads: `activity` is the thread object's (below); `doing` is one line on its latest action as reported (`thinking`, `<tool>: <input>`, the line it is writing, `waiting for you: <summary>`, `idle · <what it said>`, `stopped`, ...), `said` the end of what its agent last said in the open or last turn (600 characters at most), `task` the first line of that turn's prompt, `at` when the thread's agent or turn last did something (null before its first turn; a status change such as a raised request does not move it). Folded from the log, not recorded; a worker's own permission or question is recorded with `ext.twr.context: {said?, task?}` as they were when it was raised | |
| `usage.threads` | `{since? (RFC 3339), by? (thread\|project\|runtime, default thread)}` | `{since, by, rows: [{key, label, input, output, cost, turns, costed_turns}], total}`: what the log's `turn.ended` usage adds up to since `since` (all time without it), deleted threads included (labelled `name (deleted)`), grouped by thread, project or runtime (the adapter that reported the turn), by cost then tokens, the most first. `input` counts cache reads and writes; `cost` is null when no turn of the row reported one (Claude Code reports the API price on every plan; Codex, Grok Build and ACP agents none). Read only | |
| `usage.get` | - | `{now, runtimes: [{runtime, account, label, plan, windows: [{window, label, used_pct, remaining_pct, resets_at, minutes}], observed_at, age_secs, stale, source}]}`: per runtime (and account: null for the runtime's own login), the usage windows its sessions last reported that have not reset yet, shortest first. `used_pct` is the share USED (0 to 100, more when a runtime lets usage run past its cap); `remaining_pct` is `100 − used_pct`, clamped to 0–100, without rounding. Human-facing text (`twr usage`, the TUI, MCP `get_usage` and readable control events) shows the share LEFT; `window` is `five_hour`, `seven_day`, `month` or `<minutes>m`, `label` `5h`, `wk`, `mo`, ...; `observed_at` is the oldest report of the windows shown, `stale` once it is 10 minutes old; `source` is `event` (the runtime's own report) or `read` (twr asked). `label` is `Claude`, `GPT` (Codex), `Grok` or the runtime's name, with `:<account>` when a runtime has more than one entry. Runtimes with no data are left out. Folded from `account.usage` | |
| `thread.get` | `{thread}` | thread object (with `pending`, `live`, `box_online`: its box connected and reconciled, so what it shows is current) + `attention: [item]`, and `queued_after: [{id, after: [names], prompt, card}]` while prompts for it wait for other threads | `not_found`, `ambiguous` |
| `thread.send` | `{thread, prompt}` (`from` accepted and ignored); a user's prompt to the orchestrator is logged with `decision.made{site: route, provider: user, chosen: "orchestrator", ext.twr.user_prompt_for}` on the hub thread (its arrival, which ends the authority of the orchestrator's turn in progress) | `{thread, thread_name, queued, ref, turn, pending, dispatch_error?, parked?, warning?}` (`warning`: no runtime of the thread runs, so this prompt starts or resumes one, and the thread's box last reported its built-in agent runtime's CLI not found, as `thread.new`'s; the prompt is queued all the same) (`ref`: the message's, its queue row id like `queued`, for `message.withdraw`; the turn it starts records it as `ext.twr.ref`) | `bad_request` (empty; the orchestrator sending to itself), `conflict` (shutting down) |
| `thread.send` with `after` | `{thread, prompt, after: [thread...]}` (1 to 8 threads, not the orchestrator nor the thread itself): held until each of them has finished a turn that ended after this call and settled (no turn, background work, job or queued prompt, its report written, 5 s quiet). It is recorded as a `conversation.update` on the thread with `ext.twr.send_after = {id, prompt, after: [{thread, turn}], source, from}`, then queued as the caller sent it, `{report:<thread>}` replaced by that thread's report (at most 8000 bytes), with `ext.twr.send_after_end = {id, outcome: sent}`. When one did not finish well (failed, interrupted, gave up, needs verification), a hub card (`attention.raised` on `hub`, `ext.twr.after_card = {id, thread}`, options `Send`/`Drop`, user only) asks; a later good finish sends it and closes the card. Deleting either thread drops it (`outcome: dropped`). At most 64 at once | `{thread, thread_name, queued_after, after: [names]}` | `bad_request` (empty, more than 8, itself, waiting for the orchestrator or held for it, a circle through other held prompts, a `{report:X}` naming another thread), `not_found`, `conflict` (64 waiting; shutting down) |
| `thread.send_cancel` | `{id}` (a unique prefix of 4 or more): drops a prompt held by `after` (`outcome: cancelled`) | `{cancelled, thread, thread_name}` | `not_found`, `ambiguous` |
| `orchestrator.show` | - | `{available, runtime, model, model_actual, thread, name, status, live, configured: {runtime, model, explicit}, retired, runtimes, installed}` (`runtime`/`model`: the current orchestrator's, else the configured ones; `model_actual`: the model the current orchestrator's runtime reports it runs, as the thread object's, null when there is none or nothing was reported; `explicit`: the config sets `[orchestrator] runtime`; `runtimes`: local runtimes that can host it; `installed`: `{runtime: bool}` per entry of `runtimes`, whether its program is on PATH, true when twr doesn't know its program) | |
| `orchestrator.watch` | `{thread, note, repeat? (default false)}`: `thread` a worker (id, name, alias or prefix) or `"*"` for every worker, present and future; `note` (at most 500 bytes) is what the orchestrator should do when it fires | `{watch, thread, thread_name, status?, background_task_count?, last_report?: {turn, grade, summary, interim, follow_up}, existing, watches}`; appends a `conversation.update` on the orchestrator thread ("Watching api: ...", `ext.twr.watch: {id, thread, note, repeat}`). The same thread, note and repeat again returns that watch (`existing: true`). It fires on the worker's next final report (see Watches below) | `bad_request` (empty or long note, the orchestrator or hub thread), `not_found` / `ambiguous` (thread), `conflict` (no orchestrator, 64 watches set, shutting down) |
| `orchestrator.unwatch` | `{watch}`: a watch id (`w3`) or `"all"` | `{removed, watches}`; appends a `conversation.update` ("Stopped watching api (w3).", `ext.twr.unwatch: {ids}`); a firing still queued for the orchestrator goes too | `not_found` (no such watch) |
| `orchestrator.watches` | - | `{watches: [{watch, thread, thread_name, note, repeat}]}` (`thread`: `"*"` for every worker) | |
| `orchestrator.use` | `{runtime, model?, summary? (default true), wait_secs? (default 60, max 600)}` | `{thread, name, runtime, model, model_actual, started, start_error?, previous: {thread, name, runtime, model, model_actual, stopping, dropped_prompts} or null, handoff: {turn, bytes, summary, summary_note, exchanges, dropped_exchanges} or null, warning?}` (`warning`: the runtime's program is not on PATH, for any runtime: a built-in one's CLI or its `TWR_*_BIN` override, an `acp:` or preset agent's command; the switch still happens) | users only (`unauthorized` for the orchestrator); `bad_request` (unknown runtime, a bare `acp`, one that cannot attach MCP: `pty`, `pty:*`, `fake`; model over 128 bytes; `wait_secs` over 600); `acp:<command>` and `[runtimes]` presets can host it, `conflict` (another switch is in progress; the current orchestrator is attached or being parked/resumed; shutting down). Can take minutes: it waits for the handoff summary |
| `say` | `{message, focused?, seen_gseq?, to?}`; `seen_gseq`: the control gseq the client had rendered. `to: "orchestrator"` skips routing: the message is delegated to the orchestrator exactly like the ladder's `delegate` outcome (same result, same `said_after` / `seen_gseq` handling for the relay guard), with the user's choice logged as `decision.made{provider: user, chosen: "orchestrator"}`; no orchestrator available: `conflict`, nothing recorded; any other `to`: `bad_request`. A reply answers a thread's permission/question only if it was raised at or before it; without it (`twr say`) `say` never answers an item, and a message that looks like an answer to one (a strict answer or answer-shaped: "yes", "allow", "2", "yes?", `@api yes`) is not routed at all: `action: "pending"` with `items: [{attn_id, revision, thread, thread_name, reason, summary, options}]` (the open permissions/questions; only the addressed thread's when addressed; compound answers such as "allow both" or "@api yes, @web no" count too, whatever their length), nothing recorded | `{action, thread?, thread_name?, ref?, turn?, pending?, attn_id?, options?, answer?, items?, decision_ids}` (`ref`: the queued message's, for `send`, `delegate` and `new_thread`; a short command, `action: "command"`, is applied at once and has none) | user clients only; `bad_request` (empty, > 256 KiB), `unsupported` (delegate with no orchestrator runtime) |
| `message.withdraw` | `{thread, ref}`: takes back a message `say` or `thread.send` returned `ref` for. Still queued: it leaves the queue and `prompt.withdrawn{ref, prompt}` is recorded with that. Its turn running and not acted yet (no `agent.tool_call`, no permission or question raised in it, and for the orchestrator no call that changes something): `turn.withdrawn{ref, turn}` is recorded in that turn, then the runtime is told to interrupt it (the session stays). Under the hub's lock: whichever comes first, the withdrawal or the turn's first action, wins | `{withdrawn: true, state: "queued" \| "running", thread, thread_name, ref, prompt, turn?}`, or `{withdrawn: false, reason, message, thread, thread_name, ref, turn?}` with `reason` `acted`, `ended`, `uninterruptible` (its turn started on a `pty:` runtime, a remote box or a scripted fake: they cannot interrupt a turn), `withdrawn` (taken back already) or `unknown` | user clients only (`unauthorized` for the orchestrator and agents); `bad_request` (no `ref`), `not_found` |
| `message.send_now` | `{thread, ref}`: runs queued message `ref`, the next one queued for the thread, now. A turn running: `turn.interrupted{ref, turn}` is recorded in it (source: the user's client), then its runtime is told to interrupt it; it keeps what it did and its authority, ends `interrupted` whatever the runtime reports (no grade, report, review or retry; a crash after it makes it no failed turn), is interrupted again on the first sign of each kind that its runtime still runs it, as a taken-back turn is, and the message is the thread's next prompt. No turn running: it starts now if the thread can take a turn. A failure that held the queue is released, as the user's own send releases it | `{sent_now: true, state: "interrupting" \| "started" \| "waiting", thread, thread_name, ref, turn?, message?}` (`waiting`: the thread can't start a turn now, parked, attached, its box offline or its runtime stopping, and the message is next), or `{sent_now: false, reason, message, thread, thread_name, ref, turn?}` with `reason` `unknown` (not queued: it started, or was taken back), `not_next` (other messages are queued ahead of it) or `uninterruptible` (the running turn is on a runtime that can't interrupt one: the message runs when it ends) | user clients only (`unauthorized` for the orchestrator and agents); `bad_request` (no `ref`), `not_found` |
| `thread.queue` | `{thread}`: the messages queued for the thread, in the order they run (the turn running is not queued) | `{thread, thread_name, total, held, items: [{ref, prompt, truncated, from, mine, queued_at}]}`: the first 20 of `total`, each `prompt` cut to 2048 bytes (`truncated`; `message.withdraw` returns the whole text); `from` `user`, `orchestrator` or `runtime` (a job's wake), `mine`: the user sent it; `held`: a failure holds the queue until the user's next send. Editing a queued message is taking it back (`message.withdraw`) and sending the new text: a new message, at the end of the queue | user clients only (`unauthorized` for the orchestrator and agents); `not_found` |
| `thread.set_mode` | `{thread, mode}` | `{thread, thread_name, mode, previous, changed, applies, stops_background_tasks?, message?, dispatch_error?, warning?}` (`warning`: as `thread.new`'s, when the change (re)starts a runtime whose CLI the thread's box last reported not found, also on a `needs_confirmation` result): `changed: false` (and `message`) when already in that mode: from a user, `thread.mode_changed{mode, previous: mode, by: user}` is still appended (the user's explicit choice, which ends an earlier message's authority over that thread's mode; the reducer keeps the mode, nothing restarts), from the orchestrator nothing is recorded; else `thread.mode_changed{mode, previous, by: user \| orchestrator}` is appended and `applies` says when the runtime takes it: `next_start` (no live runtime: the next start or resume uses it), `now` (an idle runtime is stopped and started again on its session: `thread.stopped{ext.twr.reason: "mode_change"}`, which does not make the thread stopped, then `thread.started`), `after_turn` (a running turn, and the items it waits on, finish in the old process; the restart comes before the next queued prompt or retry; an item the runtime raised between turns, of any reason, defers it until answered, and nothing is dispatched to the old process meanwhile), `never` (`pty` applies no mode). The runtime's own background tasks (`ThreadView::runtime_tasks`) defer a restart that keeps or widens the mode until they end; a narrowing one (`twr_adapters::mode_narrows`: any capability lost, compared on the runtimes' effective policies, e.g. Codex approval policy, sandbox, network and roots; a lower or, across modes or runtimes, an equal level; anything unknown) waits neither for them nor for items raised between turns (its stop resolves those `thread_stopped`), the tasks end with the runtime, and the result says `stops_background_tasks: true`. A `thread.stop` during a pending restart wins. Remote-box threads restart through their runner the same way; a runner stamps `ext.twr.mode` (the mode it started the session in) on its start event, and a start event with no known mode is recorded with `ext.twr.mode: "unknown"`, which a restart replaces | `bad_request` (a mode `thread.new` would refuse, canonicalized the same way; the orchestrator thread: its mode is clamped, switch it with `twr orchestrator use`), `conflict` (a retired orchestrator, shutting down), `relay_forbidden` (the orchestrator, outside the mode rule's provenance); the orchestrator's change no short command covers returns `{status: "needs_confirmation", thread, thread_name, mode, message}` instead, recorded for the confirm card |
| `thread.set_model` | `{thread, runtime?, model?, effort?}`: at least one; one left out keeps the thread's (the thread's own runtime named alone changes nothing: `changed: false`), one that is blank or `default` (any case) returns it to the runtime's default, except, without a move, once the thread's next start resumes a session where its runtime keeps that setting on resume (`twr_adapters::resume_keeps`: the model on Claude Code, Codex, Grok Build and ACP agents; the effort on Codex, Grok Build and ACP agents; Claude Code resumes on the default effort), since twr would resume without it and the session would go on with the old value: once the thread has a session, and while a live runtime of its runtime started with a value has not reported its session yet (a live runtime the thread moved away from does not count: it is replaced, not resumed). `runtime` (blank: none) moves the thread to another runtime (see Moving a thread to another runtime below): any runtime `thread.new` takes but `pty` / `pty:*`; from the orchestrator only a built-in agent runtime (`claude-code`, `codex`, `grok-build`) or a `[runtimes]` preset. With a move, a model or effort left out is the new runtime's default, both are checked against the new runtime, and going back to its default is never refused (its first session is a new one). `model` is the runtime's own id or alias, passed as given (Claude Code: `fable`, `opus`, `sonnet` or a full id such as `claude-fable-5-1`); `effort` is canonicalized for the (new) runtime: Claude Code `low`, `medium`, `high`, `xhigh`, `max` (`--effort`); Codex `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra` (`config.model_reasoning_effort`); Grok Build `low`, `medium`, `high`, `xhigh` (its `reasoning_effort` config option); a generic ACP agent its own value, as given (set on its `thought_level` config option; an agent without one fails the session) | `{thread, thread_name, runtime, previous_runtime, runtime_changed, model, effort, previous_model, previous_effort, changed, applies?, stops_background_tasks?, message?, dispatch_error?, warning?}` (`model` / `effort` null: the runtime's default; `warning`: a move to a built-in agent runtime whose CLI the thread's box last reported not found, as `thread.new`'s, also on a `needs_confirmation` result; the move is made all the same): `changed: false` (and `message`) when the thread does not move and both are the thread's already, and nothing is recorded or restarted; else `thread.model_changed{model, effort, previous_model, previous_effort, runtime?, previous_runtime?, by: user \| orchestrator}` is appended and `applies` is `next_start`, `now` or `after_turn`, with the same restart as `thread.set_mode` (the restart's stop says `ext.twr.reason: "model_change"`, or `"mode_change"` when a new mode goes with it). With a move, `runtime_changed: true`, the stop says `"runtime_change"`, nothing starts again until the thread's next turn, and `message` says so. The orchestrator's call is applied directly (no relay gate, no confirm card: a model choice answers no permission), except a move: unless the thread's mode is a restricted one on the new runtime (`plan`, `read-only`, `dontAsk`), the move goes through the `set_mode` gate's provenance (`relay_forbidden` outside a user turn whose message is the user's latest, for a thread made after it arrived, a second move of the thread in the turn, or after the user changed its mode since) and returns `{status: "needs_confirmation", thread, thread_name, runtime, message}`: it goes on the turn's confirm card (`move api → codex, effort "high"`), and Confirm makes it as the user's (runtimes differ in what one mode lets through) | `bad_request` (none given; an effort the runtime does not take, listing the ones it does; a model over 128 bytes or with control characters; a local Claude Code whose `--help` does not list `--effort`, as far as an earlier spawn read it (the `claude` the thread would run: `TWR_CLAUDE_BIN`, else on the `PATH` its `env` gives, from its directory): "run `claude update`"; a clear back to the runtime's default that a resumed session would not take: "<runtime> keeps a session's <model\|effort> on resume, ... pick a <model\|effort> instead"; a `pty` thread without a new runtime; a runtime that is unknown, missing on the thread's box, `pty`, or does not take the thread's permission mode (set one it takes first); the orchestrator naming a command line or anything but an agent runtime or preset; the orchestrator thread: switch it with `twr orchestrator use`), `conflict` (a retired orchestrator, a thread being deleted, an attached thread moving to another runtime, shutting down) |
| `thread.rename` | `{thread, name}` (a worker thread; `name` as `thread.new` takes it) | `{thread, thread_name, previous, changed, worktree, branch, message}`: `changed: false` when it has that name already; else `thread.renamed{name, previous, by: user \| orchestrator}` is appended. The worktree path and branch keep the name they were made with; earlier names stay the thread's (`former_names` in the thread object): they still resolve to it, after names and aliases, and no thread takes them. The orchestrator's call is applied directly | `bad_request` (an invalid name, an orchestrator thread), `conflict` (the name is another thread's id, name, alias or former name, or its own alias, in any case; a retired orchestrator; a thread being deleted; shutting down) |
| `thread.delete` | `{thread}` or `{threads: [string]}` (one to 32; the same thread twice counts once) | `{status: "needs_confirmation", threads: [{thread, thread_name, pending, worktree?, branch?, dirty_files?, hidden_changes?, ignored?, ignored_entries?, build_output?, build_output_bytes?, repositories?, unsearched?, linked?, mounts?, mounts_unknown?, refused?, head_only_commits?, branch_only_commits?, shared?, existing_branch?, worktree_missing?, worktree_unchecked?}], thread, thread_name, attn_id, revision, summary, options, existing, replaces?, message}`: nothing is deleted; a delete card is raised for all the threads (see Deleting threads below), or the open one naming exactly them is returned when they have nothing more to lose than it says (`existing: true`, with what it says), else a new card replaces it (`replaces`: the old card's id). `thread` / `thread_name`: the first thread's. `dirty_files`: uncommitted files (each untracked file counts); `hidden_changes`: tracked files whose edits `git status` hides (assume-unchanged, skip-worktree); `ignored`: the first five ignored entries by top-level entry (`tools/ (3 files)`, `.env`) of `ignored_entries` in all, build output left out; `build_output`: the ignored entries of `[threads] park_disposable` (`target/`, `node_modules/`, ...) holding no git repository, which the deletion deletes instead of moving them to the trash (they are regenerated), `build_output_bytes` their size on disk; `repositories: [{path, nested?, commits, unchecked?}]`: git repositories that go with the worktree (its submodules' and every other git directory in its record, also with its directory gone, and `nested: true` for another repository nested in it, in a tracked, untracked or ignored directory, bare or not) and lose commits (on none of their remote-tracking branches) or could not be checked (`unchecked`: what and why); `unsearched`: directories of the worktree not searched to the end for repositories (more than 200,000 entries, or unreadable; its top level is `./`); `linked: [{path, record, repo, same_repository?}]`: linked worktrees checked out inside the worktree whose record is elsewhere, and `refused`: why the thread cannot be deleted while they are there (move or remove them with git first); `mounts`: mount points inside the worktree (below its top level, by Linux's `/proc/self/mountinfo` or macOS's `getfsstat`, resolved paths compared by whole components), and `mounts_unknown: true`: that list could not be read; either is in `refused` too (`api's worktree has a file system mounted in it at target/shared: unmount it first`): moving, copying or deleting through a mount point would take that file system's files, so nothing of the thread is made, moved or deleted (build output is deleted only on Linux and macOS, where that list is read: on any other system no build output is listed or deleted, all of it goes to the trash with the worktree, nothing is refused for a mount, and a mount in the worktree is moved, copied and purged with it; a mount made inside a trash item later is not protected either); `worktree_unchecked: true`: the worktree could not be inspected. `branch_only_commits`: lost commits the branch or its reflog reaches, those nothing that stays reaches (the card's other threads count as gone, see Deleting threads); `head_only_commits`: lost commits only the worktree's own `HEAD` or `HEAD` reflog reaches (made on a detached `HEAD`), counted while the worktree has a record, even with its directory gone; both absent when they could not be checked; `shared: [{thread_name, commits}]`: how many of its lost commits each other thread of the card also has; `existing_branch: true`: the branch existed before the thread | `bad_request` (both or neither of `thread` / `threads`, an empty list, more than 32, an orchestrator thread: the current one is switched with `twr orchestrator use`, a retired one is never deleted), `not_found`, `ambiguous`, `conflict` (attached; another thread attached inside its worktree; being parked, resumed or deleted; shutting down), `box_offline`; any refusal names the thread and refuses the whole request |
| `trash.list` | - | `{items: [{item, short, thread, thread_name, project, box, deleted_at, deleted_at_ms, purge_at, purge_at_ms, bytes, size, losses, worktree, branch, dir, incomplete?, box_offline?, restored_as?, restored_at?, restored_at_ms?}], keep_days, bytes, size, unavailable: [{box, error}], message}`, oldest first: every item of the trash of every box that is online, from the box's own manifests, plus the items the log knows of boxes that are not (`box_offline: true`, as recorded at deletion). `short`: the last 8 characters of the item id (what people type); `purge_at`: `deleted_at` plus `[trash] keep_days`; `losses`: what the delete card said the thread loses, in short (`3 uncommitted files, 2 unpushed commits`); `incomplete: true`: its deletion failed part way (the thread is still there); `restored_as`/`restored_at`: the log's `thread.restored` of it (the name it was restored under, and when; the item stays until purged and is not restored again); `unavailable`: online boxes that could not list theirs | |
| `box.add` | `{name}` (users only) | `{box, transport, listening, already}`: box `name` of `config.toml` and `boxes.toml` (read again now) is added to the running hub: a token box can connect at once when `listening` (the hub serves `[hub] listen`), an SSH or command box is dialed by the hub from now on. `already: true`: the hub had that box with the settings the files give it now (its `build` aside), so nothing changed. Changing or removing a box needs a hub restart | `unauthorized` (the orchestrator), `not_found` (in neither file), `bad_request` (the files do not parse, or the box is refused: an invalid name, `local`, an SSH box without `ssh` or with a token, a command box without `command` or with a token or `ssh`, a token under 16 characters), `conflict` (the hub has a box of that name with other settings; nothing is replaced), `unsupported` (a named workspace's hub: boxes are the default workspace's; a hub started without a config file) |
| `box.agents` | `{box, refresh?}` (`refresh`: users only) | `{box, online, agents}`: box `box`'s agent CLIs as `hub.info` gives them (`local`: the hub's own), as last reported, or with `refresh` probed again first (a remote box's runner probes, `Op::Agents{refresh: true}`, at most 30 s; the hub's own box: the hub probes). The orchestrator and agents get `runtime, found, login` of each | `not_found` (no such box), `box_offline` (`refresh` of a box that is offline or does not answer), `unauthorized` (`refresh` from the orchestrator or an agent) |
| `box.install` | `{name}` (users only) | `{box}`: SSH or command box `name`'s hub connection tries again now (after `twr box install` put a build there), with the box's `build` read again from `config.toml` and `boxes.toml` (kept as it was when they cannot be read or no longer have the box; only the build is taken) | `unauthorized` (the orchestrator), `not_found` (no such box), `bad_request` (a token box, or one this hub does not dial), `conflict` (the box's other settings, such as its `ssh`, changed in the files since the hub added it: restart the hub; its projects aside, which `box.projects` takes) |
| `box.restart` | `{box}` (users only) | `{box, epoch}`, at once: SSH or command box `box`'s daemon is restarted by the hub's connection to it: the connection that is up ends (the box is offline meanwhile: operations on it get `box_offline`; the daemon's runtimes end with it and their threads are closed as crashed by the next connection's reconciliation, each one's next prompt resuming its session; during a drain, `drain`, its runtimes are stopped for the upgrade first, `box_upgrade`, no crash, and the connection ends once they have exited) and the hub connects again at once with the relay's `--restart`, which stops whatever daemon of this hub key answers, whatever its build, and starts a new one (on a Mac, in the GUI session when there is one). `epoch`: the connection being replaced, null when none is up; the box is restarted once `hub.info` shows it `online` with a larger `epoch` and `restart_pending` false | `unauthorized` (the orchestrator), `not_found` (no such box), `unsupported` (a named workspace's hub, which does not connect to boxes), `bad_request` (a token box) |
| `box.projects` | `{box}` (users only) | `{box, projects, repos}`: box `box`'s projects (`[boxes.<box>.projects.<project>]`, where each repository is on the box; `twr box project` records them) read again from `config.toml` and `boxes.toml` and put in place of the ones the hub has for that box, nothing else of it or of another box changed; a new thread on the box uses them from now on. `projects`: their names, sorted; `repos`: `{<project>: <repo>}`, each one's `repo` as the hub has it now. One at a time with `box.install`, from the read to the change, so an older read is never applied after a newer one | `unauthorized` (the orchestrator), `not_found` (no such box, or the files no longer have it), `unsupported` (a hub started without a config file), `bad_request` (the files do not parse), `conflict` (the box's other settings, such as its `ssh`, changed in the files since the hub added it: restart the hub; nothing taken) |
| `box.token` | `{box}` (users only) | `{box, changed, accepting}`: token box `box` takes its token from `config.toml` and `boxes.toml` again (`twr box token --rotate` or `--reload`, `twr box remove`): the files' token when they have the box as a token box with one, else none (revoked: no runner of the box is accepted). The hub's token is only ever the files' token, or none; never one a caller passes, and the answer never holds it. `changed`: it differs from the one the hub had, and then the box's runner connection is dropped at once (the box goes offline, `runner.disconnected`; nothing the old connection sent commits afterwards) and a handshake that proved the old token is refused. `accepting`: the box has a token now. Nothing else of the box changes; `box.add`, `box.install` and `box.projects` compare against the new token. A token of 16 to 31 characters is taken with a `warnings` entry. One at a time with `box.install` and `box.projects` | `unauthorized` (the orchestrator), `not_found` (no such box), `unsupported` (a named workspace's hub; a hub started without a config file), `bad_request` (an SSH or command box, which has no token; the files do not parse, said without their text; a token under 16 characters, which is not taken while the old one is revoked all the same) |
| `trash.empty` | `{items?: [string], all?, dry_run?}` (users only): an item is named by its id, a unique end of an id (at least 4 characters, any case) or the name of the thread of exactly one item; `all`: every item of every online box; `dry_run`: purge nothing, say which items it would purge | `{purged: [{item, short, thread_name, box, bytes}], failed: [{item, box, error}], bytes, size, message}`: each item's refs (`refs/twr-trash/<item>/`) and directory are removed on its box. With `dry_run`: `{dry_run: true, items: [item as `trash.list` lists it], bytes, size, message}`; a client that asks the user (`twr trash empty`, `/trash empty`) shows those and then purges exactly their ids (`all: false`), so an item that arrives, or a name that comes to mean another item, while the user is asked is not purged by that answer | `unauthorized` (the orchestrator), `bad_request` (neither given), `not_found`, `ambiguous`, `box_offline` (an item named on an offline box) |
| `trash.restore` | `{item, as?}` (users only; `item` as for `trash.empty`) | `{thread, thread_name, previous_name, item, box, worktree, branch, notes, skipped, session_id, message}`: the thread is back (see The trash below), parked, under its name or `as`, on a worktree and branch copied from the item, which stays; `notes`: what did not come back as it was (staged changes, a `HEAD` elsewhere, special files, submodule repositories); `skipped`: special files not copied | `unauthorized` (the orchestrator), `not_found` (no such item, or its thread is no deleted thread of this hub), `ambiguous`, `box_offline`, `conflict` (the name is taken by another thread now, in any case, or the branch to make exists: pass `as`; the item is incomplete; restored already; being restored or purged already; a step on the box failed, what it made removed and the item as it was), `bad_request` (an invalid `as`), `internal` (`thread.restored` could not be recorded: what the restore made is removed, the item as it was) |
| `thread.stop` | `{thread}` | `{thread, thread_name, stopping, box_offline?, dropped_prompts}` (`box_offline: true`: its runtime runs on a box whose runner is not connected; the recorded stop goes out when it reconnects) | `conflict` (attached, being parked or resumed) |
| `thread.park` | `{thread, keep?, force?}` | `{thread, thread_name, parked: true, worktree?, worktree_removed, kept_dirty, dirty_files?, kept_reason?, kept, worktree_missing?, worktree_error?, pending}` | `conflict` (already parked, attached, being parked/resumed, a turn or background tasks running without `force`), `internal` (the runtime did not exit within 20 s) |
| `job.start` | `{argv, thread? (the user's; an agent's job is its own thread's), name?, slots? (default 1), timeout_secs?, wake? (default true)}` | `{job, thread, thread_name, name, position, log}`: queued on the machine's job slots (`[threads] job_slots`), run in the thread's directory as argv (no shell) with its runtime's environment, output to `log`. Recorded as the thread's background task `job:<id>` (source `hub`, `ext.twr.job: {id, name, argv, slots, log, wake, by}`, `by` who asked: `user:<client>`, `agent:<thread id>` for the thread's runtime, `attach:<thread id>` for the CLI attached to it), renewed every minute; it ends with `task.updated{running: false, ext.twr.job: {id, name, argv, outcome: exited\|cancelled\|timed_out\|failed_to_start\|lost, exit?, signal?, duration_ms, log}}` and, with `wake`, a queued prompt to the thread (`from: runtime`) with how it ended and the last 40 lines of its output. Before its command runs the hub records its process group: `task.updated{running: true, ext.twr.job: {id, pgid, start_time}}` (`job {name}: running`), and while it runs the group's other members when they change (a renewal with `ext.twr.job: {id, members: [[pid, start_time]]}`, at most 8, the oldest; checked every 5 s; a job whose process's start time can't be read ends `failed_to_start` without running); after a crash the next hub kills what is left of that group, whatever its environments say, then records it `lost` (not while that can't be verified: the thread then starts no runtime or job, `conflict`, until a later start verifies it). Callable by a thread's agent for its own thread while the thread's mode may run commands (`manual` or wider), checked when it is queued and again when it starts (one queued before the mode narrowed ends `failed_to_start`; a running job is not stopped when the mode narrows); an agent's job does not wake a stopped thread; not by the orchestrator | `bad_request` (empty argv, slots out of range), `relay_forbidden` (an agent naming another thread, or of no thread of this hub, or not (any longer) from a process twr started for it; an agent's thread in a mode that only plans or reads, `plan`, `dontAsk`, `read-only`, or one twr cannot classify, an ACP agent's own id: "<name> is in plan mode: jobs need a mode that may run commands (manual or wider)"), `conflict` (parked, a remote box, 8 jobs already, shutting down) |
| `job.list` | `{thread?}` | `[{job, thread, thread_name, name, argv, slots, state: queued\|running, position, running_secs, log}]`, running ones first, then the queue in order | |
| `notify.looking` | `{looking}` | `{ok}`: a user's TUI says whether the user looks at it (it repeats `true` every 10 s while they do); the hub's notification command stays quiet for 25 s after a `true`, until that connection says `false` or closes; each connection's counts for itself. Not for agents or the orchestrator | |
| `job.log` | `{job, lines? (default 40)}` | `{job, log, text}`: the end of its output (at most 64 KiB); a live job by a unique prefix of its id, an ended one by its id | `not_found` |
| `job.cancel` | `{job}` | `{job, thread, cancelling: true}`: its process group gets SIGTERM, then SIGKILL after 5 s; an agent cancels its own thread's jobs only | `not_found`, `ambiguous`, `relay_forbidden` |
| `shell.run` | `{thread?, command, cwd?, share? (default false)}`: the user's `!` command from the TUI, run by the hub as a job of `thread` (default: the orchestrator's), `$SHELL -c command` (`/bin/sh` without a usable `$SHELL`, at most 16 KiB): in a worker's worktree (local box only; `cwd` refused), or for the orchestrator in `cwd` (a folder, relative to the orchestrator's `[orchestrator] cwd`, else `$HOME`, `~` the home directory). It takes no slots, waits in no queue, runs in any mode, carries the thread's `TWR_AGENT` marker like any job, and never wakes the thread unless `share`: then, when it ends, a prompt quoting the command, where it ran, how it ended and the last 8 KiB of its output (as data, not instructions) is queued with its end, `from: runtime` (no user authority). Its `task.updated` events carry `ext.twr.job.shell: {command, cwd, share}`, and its end `ext.twr.job.printed` (the last 4 KiB of its output, escape sequences removed) | `{job, thread, thread_name, name, position: 0, log, cwd}` | user clients only (`unauthorized` for the orchestrator and agents); `bad_request` (empty, too long, a NUL, a `cwd` that is no folder or given for a worker), `conflict` (no orchestrator yet, a thread on a remote box, parked, stopping, 8 jobs already) |
| `shell.cd` | `{cwd?, dir?}`: the folder `dir` names (relative to `cwd`, `~` the home directory), from `cwd` or the orchestrator's `[orchestrator] cwd` (else `$HOME`); without `dir`, the folder `cwd` names, or that default | `{cwd}` (resolved) | user clients only; `bad_request` (no such folder) |
| `thread.trim` | `{thread}` | `{thread_name, thread, deleted: [entry], freed, notes: [text]}`: the worktree's build output deleted (ignored entries of `[threads] park_disposable` holding no repository, as a deletion deletes them); records `worktree.trimmed` on the thread. The user only (not the orchestrator, not agents). Only on the hub's own box (`local`), on Linux and macOS (where it can list the mount points). The hub's idle trims (`[projects.<name>] trim_idle_after`) delete less: only entries that match `[threads] idle_trim_disposable` (or the project's) too, by default `target/`, `node_modules/`, `__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.tox/`, `.gradle/`, `.next/` | `conflict` (a turn or background work running; attached, parked or resumed in progress; being deleted or trimmed; no worktree twr made; the box refused: neither Linux nor macOS, a mount point inside, not a linked worktree), `box_offline` |
| `thread.resume` | `{thread}` | `{thread, thread_name, resumed: true, worktree?, worktree_recreated, turn, pending}` | `conflict` (not parked, being parked/resumed), `internal` (the worktree cannot be recreated; the thread stays parked), `box_offline` |
| `attention.list` | `{all?}` | `[item + thread_name]` in stack order; without `all` only permission/question; each item has `revision` (control gseq of its latest `attention.raised`) and `first_revision` | |
| `attention.respond` | `{attn_id, revision?, answer, message?}` or `{attn_id, revision?, option_index, message?}`; `answer` is a string (`"allow"`) or `{decision, message?}`; `revision`: the item revision the client displayed (omit only for an item never raised again); a confirm card takes `Confirm` or `Cancel`, a delete card `Delete` or `Cancel` in any case (users only) | the `attention.resolved` event (for a confirmed card, with `ext.twr.confirmed`); the orchestrator's answer no short command covers returns `{status: "needs_confirmation", attn_id, answer, message}`, recorded for the confirm card | `not_found`, `conflict` (already resolved; its runtime is gone; the item was raised again since `revision`, or since it opened when `revision` is omitted: "the question changed; look again"), `bad_request` (not one of a permission's options, index out of range, both/neither given), `relay_forbidden`, `box_offline` (the runtime that asked is on a box whose runner is not connected; nothing is recorded); a delete card answered with anything but `Delete` / `Cancel` is `bad_request`, and `Delete` is refused as `thread.delete` would refuse one of its threads not deleted yet (the card stays open) |
| `attention.dismiss` | `{attn_id, revision?}` | the `attention.resolved` event (`by: user`, answer `cancel`; a confirm card applies nothing, a delete card deletes nothing) | users only (`relay_forbidden` for the orchestrator); `bad_request` (a permission/question a runtime raised: it waits for its answer, so answer it or stop the thread), `not_found`, `conflict` (already resolved, or raised again since `revision`) |
| `attention.annotate` | `{attn_id, note}` (≤ 2000 bytes) | the `attention.annotated` event | `not_found`, `conflict` (resolved), `relay_forbidden` (the orchestrator on a confirm card) |
| `report.write` | `{thread, turn?, summary, grade}` (grade: `success`, `partial`, `gave_up`, `needs_verification`) | `{thread, thread_name, turn, report, decision, review, summary}` | `bad_request` (orchestrator thread, empty), `conflict` (turn not finished) |
| `attach.begin` | `{thread, force?}` | `{thread, lease, argv, cwd, env, session_id, runtime, box?, trust_prompt?}` (`trust_prompt: true`: the first attach of a `claude-code` thread to the worktree twr made for it; Claude will ask whether to trust the folder) | `conflict` (a turn or background tasks running without `force`, already attached, parked: resume first, its directory's worktree being removed/created or parked/resumed), `unsupported` (no interactive CLI, no session yet (also while the thread moves to another runtime, until the new runtime's first session: refused before anything is stopped), a remote box without `ssh`), `bad_request` (a session id twr does not put on a command line: 1 to 256 letters, digits, `.`, `_`, `:` or `-`, not starting with `-`, as a runner may report any), `box_offline` |
| `attach.child` | `{thread, pid, start_time}` on the connection that attached | `{thread}` | `conflict` (not this connection's lease), `bad_request` (no such process carrying the grant's `TWR_ATTACH` marker) |
| `attach.end` | `{thread}` on the connection that attached | `{thread, detached: true}` | `conflict` (not this connection's lease, still starting) |
| `thread.spawn` | `{operation_id, request: <thread.new params>, prompt?}` | `{operation_id, status: pending/complete/error, thread?, error?, warning?}` (`warning`: `thread.new`'s) | Durable retry key (1–128 bytes). Same key and request return the same operation; conflicting request is `conflict`. Creation plus first queued prompt commit atomically. |
| `thread.spawn_status` | `{operation_id}` | same operation result; for the orchestrator, a complete operation whose thread runs on `local` adds `machine_load` (the `hub.info` load's summary) while that machine is busy or saturated | `not_found` for an unknown operation. After restart, committed creation is recovered; unfinished git creation is failed explicitly. |
| `thread.transcript` | `{thread, turn?, since_seq?, limit?}` | `{thread, turn, since_seq, events, last_seq, next, more}` | Semantic rows filtered before paging, default latest turn, limit default/max 500. Keep `turn` with `since_seq` for scoped continuation. |
| `thread.events` | `{thread, since_seq?, limit?}` | `{thread, events: [event], last_seq, next, more}` | |
| `thread.subscribe` | `{thread, since_seq?}` | ack `{thread, since_seq, head}`, then `thread` stream items | `conflict` (too many subscriptions: see Framing) |
| `control.read` | `{since?, limit?}` | `{items: [{stream, gseq, event}], head, next, more}` | |
| `control.subscribe` | `{since?}` | ack `{since, head}`, then `control` stream items | `conflict` (too many subscriptions: see Framing) |
| `blob.get` | `{ref, offset?, len?}` | `{ref, size, offset, encoding: "utf8" \| "base64", data, eof}` | `not_found`, `bad_request` (offset past the end) |
| `project.files` | `{project, path?, thread?}` | `{project, commit, branch_of, path, entries: [{name, kind: file\|executable\|symlink\|dir\|submodule, size, utf8?, secret?}], total, more}` (at most 500 entries) of a directory of the commit (`HEAD` of the project's repository, or the branch of worker `thread` of the project); never the work tree | `bad_request` (path, a remote box, not git, a symbolic link to the repository, a repository holding the home folder or holding or inside twr's data directory, `TWR_HOME` or a credentials folder, a git directory outside the repository, a request holding a token, a budget used up); a name holding a token is listed as `[redacted: ...]`, `not_found` (project, directory), `unauthorized` (`[orchestrator] project_reads = false`) |
| `project.read` | `{project, path, thread?, offset?}` | `{project, commit, branch_of, path, kind, content, offset, end, total, utf8?, note?}`: 16 KB of the file from `offset` (ending at a line end); a symlink is `symlink -> <target>`, never followed | as `project.files`, plus `bad_request` for a secret name (`.env`, keys, credentials, Terraform state, `[orchestrator] read_deny`), content holding a private key or known token, a binary or a file over 1 MiB, a directory or a submodule |
| `project.search` | `{project, query, path?, thread?, ignore_case?}` | `{project, commit, branch_of, query, path, hits: [{path, line, text}], more}`: `git grep -F` on the commit, 5 lines per file, 50 in all, each cut to 200 bytes; files with a secret name or over 1 MiB are excluded from the grep, and a file's lines are shown only when a read would serve the whole file; nothing says whether a left-out file matched | as `project.files`; `bad_request` for an empty, multi-line or over-200-byte query, or one holding a token |
| `context.read` | `{project}` | `{project, path, content, hash, bytes, remote, utf8, truncated, file_bytes?}` | `not_found`, `ambiguous`, `conflict` (symlinked file) |
| `context.set` | `{project, content, expect_hash?}` | `{project, path, hash, bytes, changed, remote?}` | `bad_request` (> 1 MiB), `not_found`, `ambiguous`, `conflict` (`expect_hash` stale; file not UTF-8 or symlinked) |

Details:

- **Thread object**: the thread's derived view (`id, name, project, repo, worktree, box_name,
  runtime, mode, model, model_actual, aliases, role, session_id, turns, current_turn, last_turn,
  last_report, open_attention, attached, parked, stopped, crashed, created_at, last_activity,
  ...`; `last_report.summary` leaves out the ` — twr attach <name>` that logged reports from
  before 2026-10 end with) plus
  `status` (`idle | running | background | waiting | done | error | parked | attached`), `pending` (queued
  prompts), `background_task_count` and `background_tasks` (task id → `{description, updated_at}`),
  `spent` (what its turns spent, summed from `turn.ended` `usage`: `{input, output, cost, turns, costed_turns}`, absent before any turn reported usage), `disk` (`{bytes, build_bytes, partial?}`, see Disk below) and `trimmed`,
  `live` (a runtime process is up), `activity` (one line, at most 160 characters, on what it
  does now, one level more specific than its task, folded from the log: while a turn runs, the
  step its agent's plan has in progress (a tool call's `ext.twr.step`) or what the agent said it
  is about to do, then what its latest action does (a tool call's `ext.twr.purpose`, else its
  tool and input): `implementing scroll anchoring · running tests`; else `waiting for
  permission: <summary>`, `waiting for your answer: <summary>`, `waiting on background: <task>`,
  `done: <first sentence of the report or the last message>`, `interrupted`, `failed` /
  `crashed` (with the error item's reason), `stopped`, `parked`, `attached: …`, `not started`),
  for a `pty` thread `pty_command` (its argv),
  for an `acp` thread `acp_command` (the agent's argv),
  and while attached with a reported process `attach_process` (`{pid, orphaned}`). The
  orchestrator has `role: "orchestrator"`; `retired: true` marks an orchestrator a later one
  replaced (see Switching the orchestrator). `model` and `effort` are the model and reasoning
  effort the thread's runtime starts with (`thread.created`'s, then the latest
  `thread.model_changed`'s; null: the runtime's default); while a live runtime still runs with
  others, `runtime_model` / `runtime_effort` say which (null: the runtime's default).
  `model_actual` is the one the runtime reports it runs (null until it
  reports one, and for runtimes that never do): the `model` of the latest `thread.started`'s
  runtime section (`ext.claude.model`, `ext.codex.model`, `ext.<acp agent>.model`; a new
  runtime process that reports none clears it), or of a later `turn.ended` / `thread.resumed`
  that reports one. `handoff_pending: true` marks a thread that moved to another runtime whose
  first session has not started yet (`session_id` is null meanwhile).
- **Runtimes** (`thread.new` `runtime`): `claude-code`, `codex`, `grok-build` (verified with
  grok 1.0.44 on 2026-09-30; grok ignores MCP allow rules, so a grok orchestrator's twr tool
  calls each raise a permission item unless its mode is `bypassPermissions`), `fake` (tests),
  or `pty:<command line>`, which runs any interactive CLI under a pseudo-terminal (DEGRADED:
  text, turn ends and questions are guessed from the terminal). The command line is split into
  argv the way a shell splits words (`'...'`, `"..."` and `\` quote; nothing is expanded:
  wrap it as `pty:sh -c '...'` for shell features), recorded as `thread.created`
  `ext.twr.pty_command`, and handed to the runtime as `TWR_PTY_COMMAND` (a JSON array) on
  every spawn and resume. At most 4096 bytes.
- **ACP runtimes**: `acp:<command line>` runs any agent that speaks the Agent Client Protocol
  over stdio (e.g. `acp:gemini --experimental-acp`, `acp:opencode acp`, `acp:goose acp`,
  `acp:claude-code-acp`), split like `pty:` (at most 4096 bytes), recorded as
  `ext.twr.acp_command` and handed over as `TWR_ACP_COMMAND` on every spawn and resume; a bare
  `acp` is `bad_request`. A `[runtimes.<name>] acp = "<command>"` preset (optional `attach =
  "<command with {session}>"`) makes `<name>` a runtime: it is resolved when the thread is
  created (`ext.twr.acp_preset`, `acp_attach`), and `orchestrator.show` `runtimes` lists the
  presets. twr speaks standard ACP only: `session/new` with `mcpServers` (the orchestrator's
  twr MCP server goes there), `session/resume` / `session/load` as advertised, permissions
  answered with the `allow_once` / `reject_once` option (never an `*_always` one),
  `session/cancel`. Modes: nothing is sent unless the agent reports modes; a reported mode
  wider than the thread's (bypass / yolo / always-approve, or an id twr does not know unless it
  is exactly the thread's mode) is switched down or the session fails closed. Every
  configuration the agent reports is applied and the mode checked again: the session open
  result, each `session/set_config_option` / `set_mode` / `set_model` result (a model change
  can change the mode), `current_mode_update` / `config_option_update`; the mode is checked
  before the session is ready and before each prompt. A wider report during a prompt (no
  plan-exit consent) cancels the prompt and fails the session. Only updates naming the
  session's own `sessionId` count (while it opens: kept by session id, applied once the open
  result names it). A model needs
  the agent to report a model config option or `models`, else the session fails. ACP has no
  system prompt field, so the orchestrator's is prepended to the first prompt of a new session
  (between `<twr-system-instructions>` lines). ACP cannot restrict built-in tools: a generic
  ACP orchestrator keeps its shell and file tools (noted in `thread.started`
  `ext.acp.tool_restriction`), and its tool calls reach the user as permission items except
  what the agent's own configuration lets run unasked (twr cannot see that). `attach.begin` is
  `unsupported` without a preset `attach`. Verified with a scripted ACP agent and, on
  2026-09-30, against the real `grok agent stdio` run as `acp:grok --no-auto-update
  --permission-mode default agent --no-leader stdio` (a file write asked and was allowed);
  no other ACP CLI was installed to check.
- **Attention item**: `{attn_id, thread, reason, summary, options, call_id, note, raised_at,
  event_id, context?}`; `context` (a permission or question only, when anything is known):
  `{why?, cwd?, input?, said?, task?}`, why the runtime asks and where (its
  `ext.<runtime>.reason` or `description`, and `cwd`), the whole input when the summary is cut
  (`ext.<runtime>.full_input`, marked so that every character shows, untrimmed; the summary then ends
  `… N more characters; open the card to see all`; past 64 KiB it is cut too and the item is
  deny-only) and what the agent last said and its turn's prompt (the hub's
  `ext.twr.context`); reason is `permission | question | error | review | stalled`, and the stack is
  ordered by reason in that order, then age. `attn_id` is opaque: pass it back whole. An item a
  runtime raised is `<thread id>:<the runtime's own id>` (on a remote box too), because
  runtimes number their requests per process and two threads raise the same id; the hub's own
  items (hub questions, `review`, `error`, `stalled`) are bare ULIDs. Logs written before ids
  were namespaced keep their bare runtime ids.
- **thread.send** queues the prompt (durably) and dispatches it when the thread has no open
  turn; `turn` is set when this prompt started right away. `dispatch_error` means it stayed
  queued. While attached, prompts wait.
- **say** routes a user message: `@thread ...` / `on thread, ...` → that thread; a single
  worker → it; a short reply ("yes", "deny", "2") to the focused (leftmost) thread's open
  permission/question → answers it (`action: respond`); `new <project>: ...` → new thread
  (`new_thread`); otherwise the orchestrator (`delegate`), or, with none, a
  question on the `hub` thread (`ask`, options: the worker names, `new thread` and
  `cancel (don't send)`; answer it with `attention.respond` using a thread name or the option
  index, `new thread` for the orchestrator, or `cancel (don't send)` to drop the message;
  `attention.dismiss` does the same). The two actions have spaces in their labels, so a worker
  named `cancel` or `new_thread` is a thread option like any other. The
  ladder runs with no lock held; a thread it picked that stopped
  meanwhile gets a question instead. Every rung that answered is a `decision.made` on the
  `hub` thread, listed in `decision_ids`.
- **Paging**: `thread.events` / `control.read` return at most `limit` (default 1000, max 10000)
  items and at most 8 MiB; continue from `next` while `more` is true.
- **Blobs**: payloads over 4 KB keep a preview and move fields to the blob store; the event
  then has a `_capped` list and `<field>_ref: "sha256:<hex>"`. Fetch with `blob.get` (chunks of
  at most 1 MiB).
- **Project lookup** (`thread.new`): a plain `project` that is neither configured nor an absolute
  path is looked up as a git repository folder name under the top-level `project_roots` (default
  `~/code ~/src ~/projects ~/dev ~/Developer ~/repos ~/git ~/workspace`; `[]` = off), at
  `<root>/<name>` or `<root>/*/<name>`, on the hub's own box only.
- **Worktrees** (`thread.new`): a worker thread whose project is in a git repository gets its
  own worktree at `<data>/worktrees/<project>/<name>` on a new branch `twr/<name>` from `base`
  (default: the repository's `HEAD`; an existing `twr/<name>` branch is checked out as is), and
  its runtime runs there; `repo` is the repository's top level. The path and branch are
  recorded with the thread and keep the name it had then: a rename changes neither, and park,
  resume, delete and restore (without `as`) use the recorded ones, so `twr/<name>` below
  means that branch. `no_worktree: true` runs it in the project directory itself (`ext.twr:
  {worktree: "repo"}`). Worktrees are the default: `[threads] worktree = false` turns them off, `[projects.<name>] worktree` overrides that per
  project, and `worktree: true` (or a `base`) asks for one anyway. Threads without a worktree
  of their own share the checkout with every other such thread of the repository; park never
  removes it. A directory inside another thread's hub-made worktree is
  refused (`conflict`): that worktree belongs to its thread and goes when it is parked. A
  project that is not a git repository runs in place
  (`thread.created` `ext.twr: {worktree: "repo", git: false, note}`). Hub-made worktrees are
  recorded as `ext.twr: {worktree: "twr", branch, base, base_commit}`. Thread names must be valid
  git branch components (no leading `.`/`-`, no `..`, not ending in `.` or `.lock`). The
  orchestrator never has a worktree. Nothing is appended when the worktree cannot be made.
- **Switching the orchestrator** (`orchestrator.use`, users only): a session cannot move
  between runtimes, so a switch makes a new orchestrator thread (`thread.created` with
  `ext.twr.role = "orchestrator"`, the new runtime and `ext.twr.model`). The current one is
  stopped first (like `thread.stop`: its turn ends `interrupted`, queued prompts and notices
  are dropped, but the notice lines that fire watches: the new `thread.created` carries them in
  `ext.twr.pending_worker_notices`, and they go out after its handoff turn). The latest orchestrator `thread.created` in the log is the orchestrator; every
  earlier one is `retired`: no notices, delegations, `thread.send`, `thread.resume` or
  `attach.begin` (`conflict`), and its token is dead. The new thread takes the name
  `orchestrator` and alias `orch` (unless a worker holds them); names resolve to the current
  orchestrator before a retired one, which stays reachable by id. The log keeps the choice
  across hub restarts; `[orchestrator] runtime/model` only pick the first orchestrator.
  **Handoff**: when there was an orchestrator, the new one's `thread.created` carries a
  handoff (`ext.twr.handoff_prompt`, a blob past the 4 KB cap) that is its first turn whenever
  its runtime first starts (`turn.started{from: orchestrator}`, source `hub`,
  `ext.twr.notice = true`, `ext.twr.handoff = true`; delivered once): `<twr-handoff>` holding
  the previous orchestrator's summary, the recent conversation (user turns of every
  orchestrator thread and each one's last final `agent.text`, newest kept within 24 KiB), the
  threads, open attention items (`<id>@<revision>`) and the latest reports. With `summary`
  (default) and a live previous runtime, the hub first waits until it is idle (at most
  `wait_secs`, then skips with a note) and gives it a notice turn with the same `ext.twr`
  asking for the summary; its final text is used if that turn completes in time (hub option,
  120 s). Being notices, neither turn can carry the user's authority (relay guard). While a
  switch runs, prompts to the orchestrator are `conflict`, its queue and notices wait, and a
  second switch is `conflict`. The new one starts at once when the previous one was running
  (or with `orchestrator.autostart`), else on its first message or notice; `handoff.turn` is
  null until then.
- **thread.park** stops the runtime like an attach: its exit closes the open turn and ends its
  background tasks (both only with `force`; the turn is `interrupted`) and appends
  `thread.parked{session_id, ext.twr.keep}`
  (never `thread.stopped`). Then, unless `keep`, a hub-made worktree is removed if it is clean
  (never forced; its branch is kept; stale worktree records are pruned) and kept when it has
  uncommitted changes or untracked files (`kept_dirty: true`, `dirty_files`). A clean worktree
  whose checkout is not its recorded branch (a detached `HEAD` or another branch) is kept as
  well (`kept_reason: "checkout differs from thread branch <branch> (detached at <commit>)"`):
  commits made there are remembered only by the worktree's own reflog. Ignored files do
  not count as dirty, but a worktree holding ignored files outside `[threads] park_disposable`
  (build and cache directories by default) is kept with a `kept_reason`. So is one holding a
  git repository in a tracked directory (`git status` does not show it, and removing the
  worktree would delete it; `kept_reason: "1 git repository in it that git status does not
  show (...): vendor/lib/"`; a linked worktree of another repository checked out there counts
  the same, and one in an untracked directory makes the worktree dirty), and one whose own record (its `HEAD`, reflogs or own refs such
  as `refs/worktree/...`, what a deletion counts as going with it) reaches a commit no branch,
  tag or remote does (`kept_reason: "commits only the worktree's own HEAD, reflogs or refs
  remember (e.g. <commit>)"`). The removal holds the repository like a deletion does (see
  Deleting threads): a park that waited for a deletion there checks what it would lose
  afterwards. A worktree another thread's runtime works in
  (attached included: an attach holds its directory until it ends) is kept with a
  `kept_reason`. Queued prompts stay queued; `thread.send` to a
  parked thread queues and returns `parked: true`. A hub restart leaves a parked thread parked.
- **thread.resume** recreates a removed worktree at the same path from its recorded branch (runtimes
  such as Claude Code key their sessions by directory), appends `thread.resumed{session_id,
  ext.twr.worktree_recreated}` and dispatches the queued prompts on the same session (`turn`
  is the one that started, if any), or, after a move to another runtime while parked, on a new
  session of that runtime whose first turn carries the handoff.
- **Moving a thread to another runtime** (`thread.set_model` with `runtime`): a session cannot
  move between runtimes, so the thread keeps its id, name, project, worktree, branch, queue and
  permission mode, and starts a new session of its new runtime. `thread.model_changed` carries
  `runtime` and `previous_runtime`, and in `ext.twr` what the new runtime needs, as
  `thread.created` records it (`acp_command`, `acp_attach`, `acp_preset`), and
  `previous_session`; from then on the thread object has the new `runtime`, `session_id: null`
  and `handoff_pending: true`. A live runtime of the previous one is stopped like a settings
  restart (idle at once, else once the running turn ended; its items stay with that turn) with
  `thread.stopped{ext.twr.reason: "runtime_change"}`, which does not make the thread stopped,
  and is not started again. A move between two agents of one runtime (two `acp` presets or
  commands) is a move too. The thread's next turn (a queued prompt, the next `thread.send`, a
  retry) starts the new runtime's first session; its prompt is prefixed with a handoff built
  from the log at that moment, between `<twr-handoff>` tags: the thread's task (its first
  prompt), its workspace (directory, branch and base, the last git status), its latest reports
  and its recent turns (each prompt with its final reply, the newest that fit in 24 KiB). The
  turn's `prompt` is the message as given; the handoff is `turn.started` `ext.twr.runtime_handoff`.
  Until the new runtime reports its session (`thread.started`), every turn carries it again. The
  previous runtime is not asked for a summary.
- **Deleting threads** (`thread.delete`): nothing is deleted without the user's answer to a
  card that says what goes, and what goes goes to the trash of the thread's box first (see The
  trash below), but its build output, which is deleted on Linux and macOS: a loss the card misses can still be
  recovered until the item is purged. The hub raises one delete card: a `question` on the `hub` thread
  with options `Delete` and `Cancel`, `ext.twr.delete = {threads: [{thread, name, lifecycle}], losses:
  [{thread, pending, loss}], asked_by, replaces?}` (the threads in the order asked; `lifecycle`
  the seq of each thread's latest `thread.created` or `thread.restored`, the life of it the card
  is for; `losses`
  what the card says each one loses, as recorded; `asked_by` the caller's source; `replaces`
  the card it replaces) and a summary of what goes with each thread: its runtime (stopped); its
  hub-made worktree with its size on disk and what it holds, all moved to the trash
  (`moves its worktree /w/api (1.2 GB) to the trash with 3 uncommitted files it has`): the
  uncommitted files (each untracked file counts), tracked files whose edits `git status` does
  not show (assume-unchanged or skip-worktree: `with it 1 tracked file with edits git status
  does not show (...: config.local)`), and its ignored files by top-level entry with counts
  (`with it its ignored files (.env, tools/ (12 files), +3 more)`); its build output (on Linux and macOS), deleted
  rather than trashed because it is regenerated (`deletes its build output (target/, 14 GB)
  instead of moving it to the trash`: ignored entries of `[threads] park_disposable` holding no
  git repository, nor anything git could take for one (a bare one whose `objects` is a
  symlink) or a directory not searched to the end, left out of the worktree's size; a worktree that cannot be
  inspected reads `which could not be inspected (whatever it holds goes with it)`, never clean;
  `moves the record of its worktree ... (already gone) to the trash` when a park removed it);
  every git repository removing it takes (the directory when it is there,
  and the worktree's record either way), each with its commits on none of its
  remote-tracking branches: every git directory in the worktree's record, at any depth (git
  keeps a linked worktree's submodules' there, `modules/<name>`: `with it the repository of
  its submodule libs/x with 1 commit on none of its remote-tracking branches (not pushed)`;
  one no submodule has checked out, and every one when the directory is gone, is named by its
  git directory) and the other repositories nested in the worktree, in any directory
  (tracked, untracked or ignored: `git status` shows none in a tracked directory), whose git
  directory is in it or in the record: bare repositories (a directory git itself takes for a
  git directory: a `HEAD` naming a ref or an object, `objects/`, `refs/`), `.git`
  directories, and `.git` files naming one there (`with it the git repository nested at
  tools/x with ...`); a repository inside the worktree counts its files with the worktree's
  under its path, and a bare one whose files are all tracked and unchanged (a test fixture)
  loses nothing; a linked worktree checked out inside it whose record is elsewhere (of another
  repository, or another worktree of the same one) refuses the deletion: moving it would leave
  that record naming a missing checkout, which `git worktree prune` and `git gc` in its
  repository would clear with the commits only it has, so the card ends `It cannot be deleted
  now: api's worktree contains a linked worktree of another repository at <path> (its record
  is <record>): move or remove it first (`git -C <repo> worktree move <path> <elsewhere>`, or
  `git -C <repo> worktree remove <path>`).` (JSON `linked: [{path, record, repo,
  same_repository?}]`, `refused: [...]`), `Delete` deletes nothing of that thread (an error
  item says the same; it stays parked), and its box refuses the move before anything is made
  or moved; the worktree is searched within 200,000 entries, and a
  directory not searched to the end reads `could not search vendor/ to the end for git
  repositories (...)` (the worktree's top level reads `./`); what cannot be read says `(not checked: ...)`, and so does what is left of a git
  directory git would not take (an object store with a `HEAD` or `refs/`); file names that are not plain text are shown
  escaped (`\\`, `\t`, `\xNN`, so two names never read the same); the commits deleting the card's threads loses once the trash is purged. What goes is each
  thread's branch with its reflog, and its worktree's record (its `HEAD` and `HEAD` reflog, also when
  the directory is already gone). What stays is every other ref (branches, tags, remotes,
  `refs/stash`, notes; not the trash's own `refs/twr-trash/`, which are purged in their time),
  every other reflog, and the `HEAD` of every other worktree. A commit
  only what goes reaches is lost: on its recorded branch or only in that branch's reflog,
  or made on a detached `HEAD` that only its worktree's `HEAD` or `HEAD` reflog reaches. Every
  thread on the card goes, so a commit only two threads of the card have is lost and each says
  `of its lost commits, shared with <name>: N commits (deleted too)`. Counted per repository on
  each box, by its git common directory, so threads created from two checkouts of one
  repository count together. A reflog, worktree record or object that cannot be read gives
  `(Not checked: unpushed commits (...))` for that thread, never zero. The check after Delete
  uses the same rule. Then whether the branch
  existed before the thread (`which existed before the thread,`, moved to the trash anyway),
  and its queued prompts (dropped for good). One thread reads `Delete thread api (project p)?
  This stops its runtime, moves its worktree ... to the trash ..., and moves its branch twr/api
  to the trash ...`; several read `Delete N threads?` and one `- <name> (project p): ...` line
  each. Then `The trash keeps it (them) for 7 days, then purges it (them): `twr trash restore`
  brings a thread back, `twr trash empty` purges sooner.` (`[trash] keep_days`), and past 1 GB
  in all `That holds 3.0 GB on disk until then (`twr trash empty` frees it).` A thread without
  a worktree of its own (`no_worktree`, or no git) keeps its directory, nothing of it goes to
  the trash, and the summary says so (`It cannot be undone.`). A thread may be on several open cards. Asking for exactly
  the threads of an open card checks them again: with nothing more to lose than it says, and
  each thread in the life the card is for, that
  card comes back (`existing: true`, with what it says); otherwise a new card replaces it (the
  old one resolved with answer `superseded`; the new summary starts `This card replaces <id>:
  since it was raised, there is more to lose (...)` or `..., a was deleted and restored since
  (now b)`). A card deletes a thread only in its life: one deleted (by another card) and
  restored since, under any name, with a new branch and worktree, is not deleted by it.
  `Delete` to a card with such a thread deletes and stops nothing: its `attention.resolved`
  (answer `Delete`) carries `ext.twr.changed: ["a was deleted and restored since (now b)"]`,
  and its other threads, checked again as they are now, get a new card replacing it
  (`replaces`; the summary starts `Nothing was deleted: since card <id> was raised, a was
  deleted and restored since (now b). This card leaves it out (ask again to delete it) and asks
  for the other thread of that card.`); with none left there is no new card (the orchestrator
  gets a notice line when it asked). A card whose threads are all deleted or in another life
  closes when the last of them is deleted. Like a confirm card it is `proposal: true`
  in `attention.list`, and only a user answers it (the orchestrator gets `relay_forbidden` for
  respond and annotate). `Cancel` or a dismissal deletes nothing. `Delete` checks each thread not
  deleted yet again, then works in the background. Nothing is stopped before it. Each thread
  that is not parked is parked like `thread.park {keep, force}`, and the deletion waits (at most
  20 s) until the hub is done with its runtime (a kill of what was left of one it gave up on can
  outlast the recorded exit). Then what goes is checked again, for all of them, holding their
  repositories (by git common directory on each box; the whole box when that is not known):
  deletions of threads of one repository run one after another from this check through their
  removals, and a check made before another deletion there finished is made again after it (two
  cards that each said the other thread's branch keeps a commit: the second finds it lost).
  Every other change of the hub that removes something of a repository holds it the same way
  from its own check through its removal: a park's removal of a clean worktree, a resume's or
  new thread's creation of a worktree (which clears a stale record at its path), and the
  removal of a worktree made for a thread that was not created; a deletion's check made
  before one of them finished is made again too (a park that removed the only other reflog
  keeping a commit: the deletion finds it lost).
  When any thread
  has something the confirmed card did not say (a file or commit it did not list, build output
  it said goes to the trash, a commit of a repository in the worktree, more queued prompts, a worktree, commits, a repository or a
  directory that cannot be checked now), nothing is deleted, the threads
  stay parked, and a new card for them (`replaces` the confirmed one) starts `Nothing was
  deleted: since card <id> was confirmed, there is more to lose (<name>: ...). Their runtimes
  were stopped and stay stopped.`; the orchestrator gets a notice line when it asked. Otherwise,
  one thread after another, on its box, a new trash item `<data dir>/trash/<item>/` (a ULID)
  first keeps every commit its branch (tip and reflog) and its worktree's record (`HEAD`,
  reflogs, own refs) reach under `refs/twr-trash/<item>/` in the repository (`branch` and
  `head` with their reflogs copied, `wt/<ref>` for the worktree's own refs, `reflog/<id>` for
  a commit only a reflog named), then its build output is deleted (on Linux and macOS), then its worktree moves into
  the item with whatever else it holds (`worktree/`; a missing one only has its record moved, even a locked one; only a linked
  worktree of the repository, or what is left of one: a directory with no `.git`, or one naming
  a gone record of the repository, in no work tree or the repository's directory, ever moves;
  anything else fails the deletion), then its record (`record/`, with its submodules'
  repositories: git forgets the worktree), then its branch is deleted (`git branch -D`; one
  checked out in another worktree fails the deletion before anything moves). Each build output
  entry is checked again as it goes: reached through real directories only, searched for
  repositories where it is (one that holds one is never moved), written down in the worktree's
  git record (`<record>/twr-build-output`: the item and each `{path, at}` while it is aside;
  each write a new file made exclusively and renamed over it, nothing there followed),
  moved aside into a private directory of the worktree, and deleted through the directories
  held open, no symlink followed, stopping at anything that looks like a repository or at a
  directory on another file system; one
  that cannot be deleted whole goes back where it was,
  and what is left of it moves with the worktree (the manifest's `build_output` names those
  deleted). It never goes back over anything in its place (`renameat2` `RENAME_NOREPLACE`,
  `renameatx_np` `RENAME_EXCL`): then it stays in the worktree's
  `.twr-build-output.<n>/<slot>` and moves into the item with the rest. A deletion stopped part
  way (the hub or runner gone) is put right by the next one, from that record alone (nothing in
  the worktree counts): before anything else, what it left aside goes back where it was, or,
  when that place is taken, into the new item. That takeover can stop anywhere and be taken up
  again: the record names the new item, with the earlier ones as `previous`, before each aside
  directory's `owner` is rewritten (once per directory, only from one of those items); an
  error reading or writing them fails the deletion, and no entry is forgotten. That directory's
  `owner` names the item (written before anything moved in), and `<slot>.from` where the entry
  was (for a person). The manifest's `build_output_aside` records each (`{path, at}`) and
  `thread.deleted`'s names their paths. A restore puts back exactly those, never anything else
  that looks like them: only from a directory whose `owner` (a regular file, not followed if a
  symlink) names the item, only through real directories (made when missing, a symlink on the
  way refused), never over anything there; a note says where one stays. This is safe against symlinks and repositories that are there
  when it runs (it never follows a symlink it meets), not against another process writing
  into the worktree meanwhile (twr has stopped the thread's runtime): a parent swapped for a
  symlink, or a repository made while it deletes. Nothing else is deleted on the way: a step that fails leaves what did not move where
  it was (build output deleted before it stays deleted). Before the record moves,
  every object its index names (staged content of every stage, its cache tree, resolve-undo and
  sparse directory trees) that no kept commit has is packed into the item
  (`index-objects/`), and the branch's reflog is copied verbatim into it (`reflogs/branch`):
  no `git gc` reaches either while the item exists. A trash on another file system is copied
  to whole (each file checked), and only then is each copied file removed at the source; what
  could not be removed stays (a socket or FIFO is not copied and stays too), and
  `thread.deleted` names it in `worktree_left`. The item's `lock` is held by the one operation
  at work on it (its making, a restore or its undoing, a purge), and the hub holds the new item from before it
  is made until `thread.deleted` is recorded. The item's `manifest.json` says what it holds,
  what the card said the thread loses, its size and how to recover it by hand; and
  `thread.deleted` (with what the second check found, and the trash item) is recorded with the
  resolution of its
  open items, of every other open delete card whose threads are all deleted then (`by:
  thread_stopped`), and the drop of its queue.
  The orchestrator's queued notices about the thread are dropped with it (when there were any,
  one line `- thread=<name> deleted (its updates queued for you were dropped)
  thread_id=<id>` says so).
  Meanwhile the thread takes no prompt, turn, model or mode change, park, resume or attach
  (`conflict`), and no attach starts inside its worktree. A step that fails stops that thread's
  deletion there: it stays with an `error` item `Deleting <name> failed: ...` (parked, its
  worktree possibly gone, once the park succeeded; a runtime that did not stop in time may still
  be stopping), and the next thread goes on; asking again finishes the job. A hub restart
  during a deletion leaves the thread as well, unless its worktree and branch had all moved:
  when `thread.deleted` cannot be recorded after that (a store failure, a crash, a lost runner
  result), the thread stays (it takes nothing meanwhile) with an `error` item `Deleting <name>:
  its worktree and branch are in trash item <item>, but the deletion could not be recorded:
  ...`, and the hub records the deletion from the item's manifest as soon as it can (see
  reconciliation under The trash).
- **The trash**: each box keeps its deleted threads' items under `<data dir>/trash/` (the
  hub's for `local`, the runner's for a remote box) and their refs under `refs/twr-trash/` of
  each repository. `trash.list` lists them (the orchestrator may call it; its `list_trash` tool
  does). The hub purges every item older than `[trash] keep_days` (default 7) on every online
  box when it starts and every hour; `trash.empty` purges named items, or all, now (users
  only). A purge removes only `<data dir>/trash/<id>` directories named by a valid item id whose
  manifest reads and names that id, and only refs under `refs/twr-trash/<id>/` of the
  repository the manifest names; it never takes an item another operation holds (a restore
  reading it), or an incomplete one that holds anything its deletion moved (its worktree's
  files or record, or the only copy of its branch: recover it by hand); an incomplete one that
  holds nothing goes once untouched for an hour. Those are reported in `failed`. A restored
  item is purged like any other. `trash.restore` (users only) brings a deleted thread back on
  its box by copying from the item, which it never moves or changes: its recorded branch
  (`twr/<as>` with `as`) is created (`update-ref create`, never over an existing one) at
  `refs/twr-trash/<item>/branch`, or at `refs/twr-trash/<item>/head` when the item keeps no
  branch (its reflog written from the item's verbatim copy; a reflog of that name there
  without the branch is refused); git adds a worktree on it (`git worktree add`) at its old
  path, or a fresh one when that is taken (`<worktrees>/<project>/<name>`, then `-2` .. `-10`),
  and marks its record as the restore's (`twr-restore`: the item, the branch, its commit); the
  branch's files are checked out and the saved files copied over them: the saved `.git`
  skipped, every entry read with `lstat`, symlinks recreated as symlinks, files copied with
  their modes, what the checkout has in the way (a symlink too) removed rather than written
  through, sockets, FIFOs and devices skipped (`skipped`, and a note), tracked files the saved
  tree lacks deleted from the new worktree (build output the deletion deleted is not there: a
  build makes it again; what it kept aside goes back where it was, `build_output_aside`); the repositories of its submodules
  (`record/modules/`) are copied into the new record when it has none (else a note says they
  stay in the trash). Not restored, and said in `notes`: the index (staged changes come back as
  unstaged ones; the staged versions stay in the item's `index-objects/` pack), a `HEAD` that
  was detached or on another branch (the worktree is on that branch; that commit stays at
  `refs/twr-trash/<item>/head` until the purge). A name another thread took, or that branch
  existing again, needs `as` (`conflict` otherwise). A step that fails removes what the
  restore made (the worktree and its record, the branch while it points where the restore put
  it), the item as it was. The hub then records `thread.restored`; when it cannot, the box
  removes what it made (`internal`, `... what it made is removed again, and the item is as it
  was`), so the restore can be tried again. The item stays in the trash until purged:
  `trash.list` gives `restored_as` and `restored_at`, and a second `trash.restore` of it is
  refused (`conflict`, `restored already, as <name> ...`). `thread.restored` brings the thread
  back as it was when deleted: parked, with its session (`thread.resume` resumes it, if its
  runtime still has it), mode, model and effort, and the settings its session last ran in (an
  attach that cannot apply a newer model is still refused); its earlier events never left the
  log. Reconciliation: when the hub starts (its own box), when a runner's box comes online, and
  before every purge, the hub settles what a crash left the log and a box disagreeing on: a
  worktree marked by a restore of an item the log shows no `thread.restored` of, whose thread
  the log still shows deleted, is removed with its record and branch (the item untouched); a
  complete item whose thread is still here, parked and idle, with no `thread.deleted` naming
  it, has its deletion recorded then. A thread
  without a worktree of its own leaves no item and cannot be restored. By hand (no hub): `git
  branch <name> refs/twr-trash/<item>/branch` in the repository brings the branch back; the
  files are in `<data dir>/trash/<item>/worktree`, whose `.git` still names the record's old
  path (`<item>/record`: move it back to `<common dir>/worktrees/<name>` and run `git worktree
  repair <path>`, or just copy the files out).
- **git.snapshot** `{branch, dirty_files, ahead, behind, base?, base_ahead?, base_behind?,
  conflicts?, conflict_count?}` (source `hub`, control stream) is appended after a turn ends
  and on a 30 s tick for threads that are not parked or stopped, only when one of those values
  changed. `branch` is null on a detached `HEAD`; an untracked directory counts as one dirty
  file. `base` (merge readiness, a thread with a worktree of its own): `[projects.<name>] base`,
  else what its worktree was made from, fully qualified (`ext.twr.base_ref`:
  `refs/heads/dev`, a remote branch, a tag, or the commit a base like `HEAD~1` named); a
  thread with neither counts against its upstream only;
  `base_ahead`/`base_behind` count commits on each side, `conflicted` whether a merge would
  conflict, `conflicts` the files it would conflict in (at most 20; `conflict_count` when there
  are more; `[]` with `conflicted: true`: git named none, as for some directory renames);
  both absent with a git without `--attr-source`.
- Events on the reserved `hub` thread (route decisions, hub questions, `context.updated`,
  `account.usage`) are read from the control stream (`thread.events` does not list `hub`).
- **Subscription usage**: adapters report their runtime's usage windows as
  `runtime.limits{windows: [{window, used_pct, resets_at?, minutes?}], source: event | read,
  plan?}` (Claude's `rate_limit_event` and `get_usage`, Codex's `account/rateLimits/*`, Grok
  Build's `_x.ai/billing`; reads cost no model turn and run when a session starts and at a
  turn's end once the data is 10 minutes old). The hub never records `runtime.limits`: it
  merges the windows into what it knows of the runtime the session was started as (a thread
  moved to another runtime keeps its previous one running until its turn ends; a window a
  report leaves out is kept until it resets) and appends **account.usage** `{runtime,
  account?, windows, source, plan?, thread, observed_at}` (source `hub`, `hub` thread, control
  stream) when a whole percentage, a reset time or the plan changed, or a window's report is
  10 minutes newer than the one recorded. `account` is absent for the runtime's own login.
  Each window carries `observed_at`, when the runtime reported it (a runner's capture time,
  which the event's `ts` may follow), and keeps its newest report: a runner's replayed report
  updates only the windows it has newer, and the plan only when it is not older than the last
  record. The record's `observed_at` is the newest report folded so far. `usage.get` answers
  the fold.

## Events and streams

Event envelope: `{v: 1, id (ULID), seq (per thread), ts (RFC3339), thread, turn, source, kind,
data, raw?}`. `source` is `adapter:<name>`, `hub`, `user:<client>`, `orchestrator` or
`decision:<provider>`. Runtime extras live in `data.ext.<runtime>`, twr's own in `data.ext.twr`.

- **Control stream** (global `gseq`): `thread.*`, `turn.started`, `turn.ended`,
  `turn.withdrawn`, `turn.interrupted`, `prompt.withdrawn`, `task.updated`, `attention.*`, `report.*`,
  `decision.*`, `git.snapshot`, `runner.*`, `context.updated`, `conversation.update`,
  `project.read`, `account.*`, `worktree.*`. Never `agent.*`.
- **Disk**: `worktree.disk{bytes, build_bytes, partial?}` (source `hub`) records what a thread's worktree takes when its box measured it and it moved by more than 50 MB or 5%; `worktree.trimmed{deleted, freed, notes?}` (source: whoever asked) what a trim deleted. The thread JSON's `disk` is the latest measurement less what trims freed since, and `trimmed` `[at, freed]`. A disk card is a question on `hub` with `ext.twr.disk: {box, free, threshold, threads: [{thread, name, build_bytes}]}` and options `Trim` / `Not now`; only the user answers it (`relay_forbidden` for the orchestrator).
- **Project reads**: `project.read{tool: files|read|search, project, path?, query?, branch_of?,
  commit?, bytes, refused?}` on the orchestrator thread (source `hub`, its current turn), one
  per orchestrator call of `project.files` / `project.read` / `project.search`, served or
  refused (`refused` says why; `bytes` is the repository content returned). Users' calls are
  not recorded.
- **Thread stream**: everything of one thread, including `agent.text`, `agent.tool_call`,
  `agent.tool_result`, `agent.thinking`.
- Subscriptions deliver the stored backlog after `since`, then live items, with no gaps or
  duplicates; they end (EOF) when the hub shuts down.

Behaviors worth knowing when you follow the stream:

- **Decisions** (`decision.made{site, provider, options, chosen, confidence, latency_ms}`):
  each site is a ladder of code rules, then the orchestrator and the user (route), or the
  site default (grade: `success` at 0.6; block_kind: `user_blocked` at 0.5). `provider` is
  `code`, `orchestrator` or `user`. Every rung that answered is logged.
- **Permission modes**: `thread.mode_changed{mode, previous, by}` sets the thread's mode
  (`thread.list` `mode`; one with `mode == previous` is a user's choice of the mode the thread
  had: no change, no restart); every runtime start uses it, and the hub stamps the mode it started a
  runtime in as `thread.started` / `thread.resumed` `ext.twr.mode`. While a live runtime still
  runs in an older mode the thread object has `runtime_mode`. A runtime restarted for a mode
  change closes with `thread.stopped{ext.twr.reason: "mode_change"}` (the thread does not
  count as stopped) and starts again on its session. `[threads] default_mode` defaults to
  `auto`: the agent's own auto mode decides which tool calls ask; whatever asks is a
  permission item only the user answers.
- **Names**: `thread.renamed{name, previous, by}` gives a worker thread its new name; the
  earlier one joins its `former_names` (they keep resolving to it, after names and aliases and
  before any prefix or name part, and stay reserved; a former name two threads have is
  ambiguous). Its worktree path, branch and aliases do not change. Routing, the user's direct
  commands and the relay guard read a user message with the names (and former names) threads
  had when it arrived, even when the client had seen less (`seen_gseq`); a thread made after
  what the user had seen is named by its name but never changed by that message.
- **Models and efforts**: `thread.model_changed{model, effort, previous_model,
  previous_effort, by}` sets the thread's model and reasoning effort (null: the runtime's
  default); every runtime start uses them, and the hub stamps what it started a runtime with
  as `ext.twr.runtime`, `ext.twr.model` / `ext.twr.effort` next to `ext.twr.mode` (null for a
  default; a start event whose runner stamped no model or effort key records it as
  `"unknown"`, which a restart replaces; one without `ext.twr.runtime` is the runtime of its
  source, `adapter:<runtime>`). A live runtime started with others restarts on its session at
  the same points as for a mode change; its stop is `thread.stopped{ext.twr.reason:
  "model_change"}` (or `"mode_change"` when the mode changed too), which does not make the
  thread stopped. With `runtime` and `previous_runtime` the thread moved to another runtime
  (see Moving a thread to another runtime): the stop says `"runtime_change"`, and a start of the
  previous runtime recorded after the move is stamped `ext.twr.runtime_moved: true` (it is never
  the new runtime's session). An `acp` runtime's start is also stamped with its agent,
  `ext.twr.acp_command` (its argv, by the hub or by the runner that started it): while the
  thread waits for its new runtime's session, a start stamped with another agent than the
  thread's `acp_command` is the previous agent's, and is replaced the same way (a start without
  the key, from an older runner, is taken for the thread's agent).
- **Deleted threads**: `thread.deleted{name, worktree?, worktree_left?, build_output_aside?, trash?, trash_dir?,
  trash_bytes?, losses?, branch?, dirty_files?, branch_only_commits?, head_only_commits?,
  dropped_prompts}` (source: the user who confirmed the delete card) records that the thread's
  runtime was stopped and its hub-made worktree and branch moved to the trash (`worktree` /
  `branch`: what moved; `worktree_left`: the worktree's directory, part of which could not be
  moved, no worktree any more, for the user to remove; `build_output_aside`: build output it
  could neither delete nor put back, kept in the item apart for a restore; `trash`: the item id, `trash_dir` its
  directory on the box, `trash_bytes` its size, `losses` what the card said it loses, in short;
  `dirty_files` / `branch_only_commits` / `head_only_commits`: what went with them). From then
  on the thread leaves every listing (`thread.list`, `attention.list`, `thread.get` and every
  lookup by id, name or alias are `not_found`), nothing more is recorded on it, and its name and
  branch are free again; its earlier events stay in the log.
  `thread.restored{item, name, previous_name?, worktree?, branch?, existing_branch?,
  former_names?}` (source: the user who restored it) brings it back from the trash: the thread
  is listed again as it was when deleted, parked, under `name` (`previous_name`: its name
  before, when `as` changed it), with its worktree at `worktree` on `branch` (both made again
  from the item, which stays in the trash, listed as restored, until purged; handoffs name
  that branch), with no aliases and only the `former_names` listed: those of its former names
  no other thread had taken (or was taking) when it was restored, checked and reserved with
  `name`; later events of it count again (replay and live agree).
- **Taking back a message** (`message.withdraw`): `prompt.withdrawn{ref, prompt}` (no turn) is a
  queued message the user took back: it never starts a turn. `turn.withdrawn{ref, turn}` is a
  running turn whose message the user took back (source: the user's client): it has no
  authority from then on (see Relay rule), and its `turn.ended` is recorded `interrupted`
  whatever the runtime reports (`ext.twr.runtime_result` keeps what it said), so it gets no
  grade, report, review or retry, and a crash after it makes it no failed turn (the thread's
  `last_turn` says `withdrawn: true`). The orchestrator's handoff and the updates its next user
  turn gets leave such a turn out. While it is open, the first event of each kind that shows
  its runtime runs it anyway (`agent.text`, `agent.thinking`, `agent.tool_call`, a permission
  or question) interrupts it again (a runtime may miss an interrupt that came before it began
  the turn), and a permission or question raised in it is declined at once:
  `attention.resolved{by: timeout, answer: {decision: "deny"}, ext.twr.taken_back: true}`, with
  `deny` sent to the runtime. A tool call already running when the interrupt lands may still
  finish. A turn the orchestrator's runtime starts on its own (`from: runtime`) right after
  such a turn is taken back too (no authority, interrupted, ended `interrupted`) unless a user
  message arrived since. The next prompt the hub delivers to the orchestrator (a user message
  or a notice, not a turn its runtime started) starts with the hub fact "The user took
  back their message «...»; disregard it and anything begun for it.", and so does every
  following one until a turn that carried it ends `completed` (as the runtime reported it,
  `ext.twr.runtime_result` when the hub recorded the end otherwise); any other end (error,
  interrupted, crashed, stopped, a hub restart) leaves it pending: its runtime still has the
  message in its history, and the fact may be told twice but is never lost. Adapters never
  emit either kind. Thread objects carry `current_turn_ref`, `current_turn_acted` and
  `current_turn_withdrawn` for the open turn (left out when unset).
- **Sending a queued message now** (`message.send_now`): `turn.interrupted{ref, turn}` (source:
  the user's client, in the turn it interrupts) says the user asked for queued message `ref` to
  run now. The turn keeps its authority; it ends `interrupted` whatever the runtime reports
  (`ext.twr.runtime_result` keeps that), so it gets no grade, report, review or retry, and a
  crash after it makes it no failed turn (`last_turn.interrupted: true`). While it is open, the
  first event of each kind that shows its runtime runs it anyway interrupts it again (its
  requests wait for the user as usual; the turn's end closes them). Adapters never emit it.
  Thread objects carry `current_turn_interrupted` for the open turn (left out when unset).
- **Orchestrator turns** get no grade, report or review. One that ends `error` raises a hub
  `error` item with the cause (`turn.ended.ext.twr.error`), which supersedes the
  orchestrator's open error items of earlier turns.
- **Reports**: every finished worker turn gets `decision.made{site: grade}` and a hub
  `report.ready{written_by: "hub"}`; a `report.write` for the same turn supersedes it (and
  keeps `ext.twr.background_tasks` while any of the tasks that turn ended with, listed with their start in
  `ext.twr.background_task_starts`, still runs). The hub's
  summary is the reply's own summary section when it has one (a markdown heading or a label
  line such as `## Summary`, `**TL;DR**` or `Result:` followed by its text, or an inline
  `Summary: ...` paragraph; the labels are summary, tl;dr, tldr, in short, bottom line,
  result(s), outcome, conclusion, status, verdict, and the first five win over the others),
  else its first three sentences; at most 400 bytes (no attach command: a report is the
  summary alone). When the turn has a final reply, it records the seq of that reply (the turn's last final
  `agent.text`) as `ext.twr.reply_seq`, and the orchestrator's notice line for it carries
  `reply_seq=<seq>` (`read_transcript` with the line's `thread_id` and that `seq` reads the
  reply in full); a report of a turn without one (failed, no reply) has neither. A turn that
  ended with background tasks still running gets an interim report (`ext.twr.background_tasks:
  <n>`; with no reply its summary says "Turn N ended without a reply; n background tasks still
  running."), and the conversation update says the worker "yielded with n background tasks
  still running (interim report: the result comes later)". The report of a turn the runtime
  started itself (`turn.started{from: runtime}`, after background work) carries
  `ext.twr.turn_from: "runtime"`; its update says the worker "finished, in a follow-up turn it
  started after background work", and a notice line for it has `follow_up=true`. Every notice
  line (`- thread=<name> ...`) ends with `thread_id=<id>`, which the notice tells the
  orchestrator to pass to its tools: a deleted thread's name can go to a new thread.
- **Watches** (`orchestrator.watch`): successful reports start no orchestrator turn, so the
  orchestrator sets a watch to act when a worker finishes. A watch fires on its worker's (or,
  for `"*"`, any worker's) next final report: a hub report of a turn that completed with no
  background task left running (an interim report never fires one; a follow-up turn's report
  does). When an interim report's tasks end and no follow-up turn starts within 5 seconds of
  log time, it fires on the idle worker instead (`- thread=<name> idle: ...`); a task starting
  meanwhile cancels that line, and the worker going idle again holds a new one. A failed turn,
  and a report the orchestrator or a user wrote, fire nothing (an idle worker fires only after
  a turn that completed; a rewrite of an interim report stays interim while the turn's tasks
  run). Firing adds ` watch=<id> note="<note>"` to the report's notice line (or queues one for
  a success report), so the orchestrator gets a notice turn. A notice turn shows at most 40
  lines and leaves the rest queued for the next one; a watch without `repeat` fires on the
  first line that carries it only. Its `turn.started` records what it chose:
  `ext.twr.watches_fired` (the watches its prompt shows), `ext.twr.notices_left` (the lines it
  left, by the id of the event that queued each) and `ext.twr.notice_cutoff` (when it chose;
  a held idle line due later stays queued). Live and on replay, that takes its lines off the
  queue and uses up the watches it fired without `repeat`: no queued line fires them again,
  and a line only there for them goes. `orchestrator.unwatch` takes the cancelled watches off
  queued lines the same way. Watches replay from the log, go with their thread's deletion and
  with `orchestrator.reset`; `orchestrator.use` keeps them, its handoff lists them and the new
  orchestrator gets the lines queued for them.
  Grades `gave_up` / `needs_verification` raise a `review` item, but an interim report
  (`ext.twr.background_tasks`) raises none, written or not. A `report.write` keeps what the
  hub's report says of the turn: the interim marker while the turn's tasks run, and
  `ext.twr.turn_from` for a follow-up turn (read from the turn's `turn.started`). The code rules grade a
  completed turn `partial` (0.8) when it made tool calls and none succeeded (every result
  `ok: false`: denied by the user or failed); `partial` raises no review.
- **Failures and the automatic retry**: a worker turn that ends `error`, or whose runtime
  crashes mid-turn, gets `decision.made{site: block_kind, chosen: transient | user_blocked}`.
  `transient` (the adapter flagged a rate limit, overload or network error) gets exactly one
  retry turn: `turn.started{source: hub, prompt: "Continue where you left off; the previous
  attempt failed with a transient error.", ext.twr.retry_of: <failed turn>}`, and a crash's
  `error` item is resolved `by: auto_retry`. Otherwise (or when the retry fails too) the hub
  raises an `error` item for the turn (instead of a `review`). This holds across hub restarts.
  After a crash that failed a worker's turn, queued prompts wait until the user sends to the
  thread (`thread.send`, `say`, an answer that routes a message to it) or resumes it, or one of
  its runtimes starts; a due automatic retry is the only exception. The same goes for a queue held after a runtime
  failed to start or a remote box failed a send. The hold survives a hub
  restart, and so does the release: a prompt the user sent after the crash still releases it at
  the next start. What else queues a prompt does not release it: a job's wake, a lost job's
  notice, the orchestrator's `thread.send` or `thread.resume` (its `thread.send` returns `turn:
  null`), a prompt sent `after` other threads when they finish (it waits with the rest; the
  user's Send on its card does release it).
- **Stalled**: a thread with a running turn or background tasks, not waiting on the user, with
  no events for `stall_after_secs` (default 600; 0 = off) gets a hub `stalled` item ("No
  activity for N min"); it resolves `by: timeout` as soon as the thread emits any other event.
  A `task.updated` heartbeat (`heartbeat: true`) only says a task is still alive: it neither
  counts as activity nor resolves the item.
- **Dismissing**: items the hub raised (a `say` question, `review`, `error`, `stalled`) can be
  closed without an answer by the user (`attention.dismiss`, `twr attention dismiss
  <id>@<rev>`, `dismiss` in the TUI's action popup): `attention.resolved{by: user, answer: cancel}`, and a
  question's message goes nowhere. A permission or question a runtime raised cannot be
  dismissed: the runtime waits for it, so answer it or stop the thread.
- **Standing context**: `<repo>/.twr/context.md` is prepended to every prompt the hub sends to
  that project's worker threads (never the orchestrator) as
  `<twr-context project="X">\n...\n</twr-context>\n\n`, capped at 64 KiB.
  `turn.started.prompt` logs the prompt as given; `ext.twr.context_hash` names the context
  that was prepended. Manual edits of the file are logged as `context.updated` (on the `hub`
  thread, bytes in the blob store) before the next turn that uses them; `context.set` logs
  first (an unlogged manual edit is logged as `origin: "file"` in the same batch), then writes
  the file atomically (fsynced, keeping its mode). Pass `expect_hash` (the `hash` your
  `context.read` returned) to get `conflict` instead of overwriting a concurrent change. The
  hub reads at most 1 MiB of the file (a larger one is logged and injected cut, with
  `ext.twr.truncated`), refuses a symlinked `.twr/` or `context.md` (not read, injected or
  written; a `.twr/` swapped for one while the file is written makes the write fail, and
  nothing is written through it), and never writes over a non-UTF-8 file. Threads on a remote box get the bytes of the latest
  `context.updated` of their project instead: the hub never reads or writes files for a
  remote repository. A project whose threads are all remote (configured or not), or whose
  `[projects.<name>] box` is remote, keeps its context in the log only (`context.read` returns the logged bytes,
  `context.set` writes no file; both report `remote: true`).
- **Orchestrator turns** from queued prompts carry `ext.twr.said_after`: the control-stream
  gseq when the prompt was queued (for a message `say` routed: at most its `seen_gseq`, 0
  without one; a hub question keeps it in `ext.twr.said_after` until answered). The relay
  guard compares it with the item's `attention.raised` gseq; a user turn without it relays
  nothing.

## Runners and remote boxes

A box other than `local` is a machine whose **runner** (`twr runner`) connects out to the hub
over WebSocket and runs that box's threads: adapters, git worktrees and snapshots all happen
on the box. The runner protocol is `twr_core::runner` (one JSON message per text frame, version
7); this section is what a client of the hub sees.

Configuration (hub side):

```toml
[hub]
listen = "127.0.0.1:7878"      # the runner listener; off when unset
# A non-loopback listen address is refused unless one of these is set:
tls = { cert = "/etc/twr/hub.pem", key = "/etc/twr/hub.key" }   # wss:// (PEM files)
allow_insecure_remote = false  # plain ws:// on the network (events in clear)

[boxes.gpu]
token = "..."                  # the runner's --token-file / $TWR_RUNNER_TOKEN; never sent or
                               # logged; 16 characters at least (`twr runner token` prints one)
ssh = "me@gpu"                 # twr attach runs `ssh -t <ssh> -- <command>` (may start with ssh options; none that forwards anything)

[boxes.gpu.projects.api]       # where project `api` lives on that box (else [projects.api].repo,
repo = "/srv/src/api"          # taken as a path on the box, or an absolute path on the box)
```

The recommended transport is a loopback listener reached through an SSH tunnel
(`ssh -o ExitOnForwardFailure=yes -R 7878:127.0.0.1:7878 me@gpu`: without the option, `ssh`
only warns when the box's port is taken, and whatever holds it gets the runner's connection)
or Tailscale; otherwise TLS. A runner trusts the public web roots for `wss://`, or only the CA
given with `--ca <pem>` (a private or self-signed CA). A runner refuses plain `ws://` to a host
that is not literally loopback (`localhost`, `127.0.0.0/8`, `[::1]`, IPv4-mapped
`[::ffff:127.x.y.z]`; names are not resolved) unless given `--allow-insecure`: its events and
commands would cross the network in clear text, open to whoever is on the path.

Handshake: the token never crosses the connection. The runner sends `hello{box, protocol,
nonce}`; the hub answers `challenge{nonce, proof}`, an HMAC-SHA256 with the box's token over
its role, the box and both nonces (random for a box it does not know); the runner checks it
before it sends anything else, then sends `auth{proof}` (its own role); the hub checks it,
answers `authenticated`, and only then takes the runner's `inventory` (running sessions,
spooled frames: it may be large) and answers `welcome`. A wrong token or unknown box gets
`error{unauthorized}`, another protocol version `error{protocol}`. A connection gets 15 s from
its accept to authenticate (TLS, WebSocket, hello and proofs together), small messages until
then, and at most 16 unauthenticated connections are held at once (4 per address that is not
loopback). An authenticated connection that sends no protocol message for 90 s is closed
(WebSocket pings do not count). A runner of another protocol than the hub's is refused
(`error{protocol}`; protocol 7 added the `agents` op, `Op::Agents {refresh}`, which answers the
box's agent CLIs as `hub.info`'s `agents` lists them, probed again first with `refresh`, and
the inventory's `agents`: a runner of 6 would leave the op unanswered), unless it is an older
one the hub drains (`hub.info` `drainable_protocols`; a hub of 7 drains a runner of 6, sending
it no `agents` op: `drain`); a runner of 6 or later
facing a hub from before 6 is refused as `unauthorized`: update both. The hub replaces an SSH
box's daemon with its own build by itself (a box of another OS or CPU needs `twr box install`);
a token runner needs updating on its box. A box daemon's inventory also says how it was started
(`Inventory.launch`, additive, absent from an older runner): `launchd <label>` when launchd
started it (its `XPC_SERVICE_NAME` is a label of the relay's, `tools.twr.box.`), `systemd
<unit>` when it runs in a transient unit of the relay's (`INVOCATION_ID` set and a
`twr-box-*.service` cgroup), else `detached`; anything else is dropped. Users see it as
`hub.info` box `launch`.

Tokens: the hub refuses a `[boxes.<name>] token` shorter than 16 characters (it does not start;
other commands still load the config, so `twr hub stop` works after an upgrade) and warns about
one shorter than 32 (log and `hub.info.warnings`); the runner refuses and warns the same way.
`twr runner token` prints a new one (32 random bytes, hex). To fix a short token, replace it in
the config and the runner's token file with a new one, then restart the hub and the runner. The
hub also warns when `config.toml` holds a box token or a `[notify]` command and group or others
can access it (`chmod 600`).

- **Online / offline.** A box is *online* once its runner has connected (valid token, same
  protocol version), replayed its spool and been reconciled; `runner.connected{box, version}`
  is appended on the `hub` thread then, and `runner.disconnected{box, version}` when the
  connection ends. `hub.info.boxes` lists each box with `online`. While a box is offline its
  threads keep their state and their runtimes keep running on the box (the runner spools their
  events); `thread.send` queues (dispatched when the box comes back); `thread.stop` of a
  running runtime (also one a restarted hub has not taken over yet) is recorded and answers
  `{stopping: true, box_offline: true}`: the stop goes out when the box comes back and the
  thread closes as stopped once the runtime has exited (or the runner no longer has it); and
  `thread.new`, `thread.park`, `thread.resume`, `attach.begin` and answers to that runtime's
  `attention.respond` fail with `box_offline`.
- **Delivery.** Every runtime event is recorded exactly once: the runner numbers each thread's
  frames (`rseq`), keeps them on disk until the hub acks them, and the hub commits each frame's
  event together with its watermark (the `runner_acks` table) before acking. Frames of one
  connection are acked in batches (up to 256 frames or 50 ms): each frame commits on its own,
  one sync to disk covers the batch, then one cumulative ack per thread goes out. Each frame
  carries the time the runner captured it (`at_ms`); the hub records the event at that time,
  kept after the thread's previous event and never later than now, so a replayed backlog keeps
  its original times (and the stall timer reads them). A dropped
  connection, a runner restart or a hub crash replays what was not acked; duplicates are
  dropped by watermark; a gap closes the connection. Commands to the box carry an id that the
  runner deduplicates, so a command sent again after a lost connection runs once; a command
  cut short by a runner crash answers `interrupted` and is never retried (a spawn then fails
  the thread's turn).
- **Reconciliation** after every (re)connection: a thread the log shows running whose runtime
  the runner no longer has (it restarted) is closed: its open turn ends `interrupted`, its
  runtime's attention items resolve `thread_stopped`, then `thread.crashed{stderr_tail: "twr:
  the runner of box ... does not run this runtime any more ..."}` and an `error` item. A
  session the runner runs but the hub does not (stopped, parked or unknown) is stopped. After a
  hub restart the hub takes over the sessions the runner still runs.
- **Hub shutdown** stops local runtimes only; remote ones keep running and the next hub takes
  them over.
- **One runner per box.** A second connection of the same box replaces (fences) the first,
  which is told `fenced` and closed. On one machine a second runner of a box does not start at
  all: a runner locks its spool (`runner-<box>.db.lock`), its worktrees directory
  (`.twr-runner-<box>.lock`) and a file keyed by the hub URL and box in
  `$XDG_RUNTIME_DIR/twr/runner-locks/` (else `~/.local/state/twr/runner-locks/`), which no
  `--data-dir` moves.
- **Frames of other boxes.** A frame naming a thread of another box is refused whatever its
  `rseq` (a duplicate too; counted, and the runner is told to stop that session); a frame with
  `rseq` 0 closes the connection. A duplicate `turn.ended` gets the reactions its turn may be
  missing only when the log's end of that turn came from the box's runtime, never one the hub
  recorded itself (a lost runtime's), so a replay cannot release a queue a crash holds.
- **Trash of a box.** An item a box lists is that box's: one whose manifest names another box
  is left out of `trash.list` (and logged), and `trash.restore` brings back only a thread of
  the box that holds the item.
- **thread.new** with `box` (or a project whose `box` is set): the worktree is made on the box
  under the runner's data directory (`<runner data>/worktrees/<project>/<name>`, branch
  `twr/<name>`); `worktree` and `repo` are paths on the box.
- **attach.begin** on a remote box quiesces the runtime through its runner and returns
  `argv = ["ssh", "-t", <ssh...>, "--", "cd '<dir>' && exec [env ...] '<program>' '<args>'..."]`
  (every part shell-quoted for a POSIX shell: the user's login shell on the box must be one,
  `sh` / `bash` / `zsh`, not `fish` or `csh`), `cwd: null`, `env: {"TWR_ATTACH": ...}` (for the
  local `ssh`) and `box`; `unsupported` without `[boxes.NAME] ssh`.
- **Attach process**: `attach.begin`'s `env` sets `TWR_ATTACH=<thread id>:<lease>` on the
  interactive command (it never carries `TWR_THREAD` / `TWR_RUNTIME_GEN`, so runtime reaping
  never touches it). Right after starting it the client reports it with `attach.child` (pid
  and start time as the hub's machine reads it: on Linux clock ticks after boot,
  `/proc/<pid>/stat` field 22; on macOS seconds since the epoch, `ps -o lstart`); the hub
  checks that the process runs and carries the `TWR_ATTACH` marker, and records it. From then
  on the attach outlives its lease connection and a hub restart: the thread stays attached
  (park and stop refused, prompts wait, its directory held) until that process has exited;
  then `thread.detached` has `ext.twr.reason: "attach process exited"`. Without a reported
  process a closed lease connection detaches at once (`connection_closed`), and a restart
  detaches (`hub_restart`). The check reads the process table (`/proc` on Linux, `ps` on
  macOS) where the hub runs, which is where the client runs (it reaches the hub over the local
  socket; for a remote box the process is the local `ssh`). `twr attach` forwards SIGTERM /
  SIGHUP to the command (its process group when it leads one or `twr attach` leads its job),
  waits up to 10 s (then kills it, 3 s more) and ends the attach only once it has exited; if
  it survives that, `twr attach` exits without ending the attach.
- A runner that restarts after a crash (e.g. SIGKILL) first kills the runtimes the previous
  runner left running (matched by pid, start time and `TWR_THREAD`; and every process marked
  for the thread since), checks that none of them runs any more, and only then reports their
  exits. When one survives, cannot be identified (no recorded start time and its pid names a
  process that may be it), a process of the user in the runner's control group started since
  hides its environment (still, a moment later) and is tied to the runtime (in its process
  group, or below the runtime or a process marked for its thread), or the process table cannot
  be read, it reports nothing for it and does not start: its error names the thread and
  process. A hidden process that is not tied to it (an `ssh-agent`, a `sudo`) does not count.
- A runner reports a runtime's `exited` only once its process is gone. When its spool
  overflows, the runtime is stopped and its later events are dropped, but its session stays
  running (it takes no `send` / `respond`, and its worktree is not removed) until the process
  has exited; the exit then carries `spool full` as its reason.
- Every runtime's environment carries `TWR_THREAD=<thread id>` and `TWR_RUNTIME_GEN=<runtime
  generation>` (the run id on a runner's box); killing one runtime never touches the thread's
  next one.

## MCP tools

Routine permissions, successful reports and confirm outcomes appear as hub-authored
`conversation.update` events on the orchestrator thread, without a model turn. Only
exceptional updates require interpretation or annotations, and a worker's final report wakes
the orchestrator only through a watch it set (`watch_thread`). Complete exact commands on
`say {to: "orchestrator"}` are preflighted atomically with the existing grammar and seen
revision checks, audited and acknowledged directly. New user prompts interrupt background
notice turns while preserving the runtime session and durable update context. Orchestrator
turn starts record `ext.twr.prompt_fingerprint`; `[orchestrator] effort` controls reasoning.


Server name `twr` (Claude Code sees `mcp__twr__<tool>`). Errors come back as tool errors whose
text starts with `hub error <code>:`.

A result about one thread (`get_thread`, `spawn_thread`, `send`, `set_mode`, `set_model`,
`park`, `resume`, `stop`, `write_report`, `watch_thread`) is compact JSON whose first keys are `thread_name`,
then `thread` (the id; `spawn_thread`'s is the thread object), then the socket call's other
keys, sorted. A runtime's summary of the result (its first couple of hundred characters) then
still names the thread. `get_thread` is the thread object led by those two keys. A
`delete_thread` result is about several threads (its `thread_name` is only the first one's):
it is the socket call's result as is.

| tool | params | socket call |
|---|---|---|
| `list_threads` | - | `thread.list` |
| `get_thread` | `thread` | `thread.get` |
| `spawn_thread` | `project, name?, prompt?, runtime?, model?, effort?, box?, operation_id?` | `thread.spawn`, briefly polls `thread.spawn_status`; returns `{operation_id, status, thread_name?, thread?, send?, machine_load?, warning?, error?}` (`thread_name` first, the id when unnamed; `machine_load` while the machine of a local thread is busy or saturated; `warning`: the box reports the runtime's CLI missing); `box` is any box the hub has (`local` or a configured one), passed to `thread.new` with its errors (`box_offline` for one not configured or not online); its description says to pick one `hub_info` shows online with the runtime's agent found and `logged_in`; the runtime is the project's (else `default_runtime`) unless `runtime` names a built-in agent runtime (`claude-code`, `codex`, `grok-build`) or a `[runtimes]` preset, only when the user asks for one (never a command line), and the permission mode is always `threads.default_mode` (the user's setting); `project` may be a repository folder name, looked up under `project_roots` (result `project_found`; `ambiguous` / `not_found` otherwise) |
| `get_spawn` | `operation_id` | `thread.spawn_status`; check pending or lost responses before retrying. Reuse the same id and identical request. |
| `send` | `thread, prompt, after?` | `thread.send` (with `after`: held until those threads finish) |
| `respond_attention` | `attn_id, answer? \| option_index?, message?` | `attention.respond` (relay rule; once per item; may return `needs_confirmation`) |
| `set_mode` | `thread, mode` | `thread.set_mode` (mode rule; may return `needs_confirmation`) |
| `set_model` | `thread, runtime?, model?, effort?` | `thread.set_model` (when the user asks for a worker's model, effort or runtime, e.g. "use Fable 5.1 on high for the webapp thread"; a model or effort change is applied directly, no confirm card; `runtime` as for `spawn_thread`: a model of another runtime means moving the thread, and the new session starts with the thread's next message; a move returns `needs_confirmation` and goes on the confirm card unless the thread is in `plan`, `read-only` or `dontAsk`) |
| `rename_thread` | `thread, name` | `thread.rename` (applied directly; when the user asks or a name no longer fits the task; the worktree and branch keep their names) |
| `annotate_attention` | `attn_id, note` | `attention.annotate` |
| `park` | `thread, keep?, force?` | `thread.park` |
| `resume` | `thread` | `thread.resume` |
| `delete_thread` | `threads` (1 to 32) | `thread.delete {threads}` (raises one delete card for all of them, which only the user answers; confirmed, the threads go to the trash; to delete threads or their leftover branches, parked and stopped ones too, the orchestrator calls it once with all of them, one call per 32, and never has a worker run git for it) |
| `list_trash` | - | `trash.list`, one line per item (`- <short> thread=<name> project=<p> box=<b> deleted_at=... purge_at=... size=... losses: ...`, an offline box's marked), then the message. Read only: there is no tool to empty the trash or restore from it (only the user does: `twr trash empty` / `twr trash restore`) |
| `get_usage` | - | `usage.get` then `usage.threads` for the last 7 days: one line per runtime, then `What threads spent in the last 7 days: api 1.2M tok $3.40 (4 turns); ...; total ...` (the five that spent most). Per runtime: `- <label> plan=<plan>: 5h 78% left (resets <RFC3339>), wk 56% left (resets ...); as of 40s ago`, or "No usage known yet: ..." (read only). The control stream renders `account.usage` as `usage <label>: 5h 78% · wk 56% left` |
| `stop` | `thread` | `thread.stop` |
| `read_transcript` | `thread, turn?, since_seq?, limit?` (default 500), or `thread, seq, offset?` | `thread.transcript`, default latest turn, semantic rows filtered in SQL before paging, ~8 KiB cap; a line whose text was shortened (cut for the line, or a capped preview) ends with `… [cut; read_transcript seq=<seq> shows it in full]`. With `seq` (not with `since_seq` / `limit`): `thread.events {since_seq: seq - 1, limit: 1}`, its capped fields restored with `blob.get`, and that event's main text in full (`agent.text` text, `turn.started` prompt, tool call input, tool result output, `report.ready` summary, `attention.raised` summary then its numbered options, `attention.resolved` decision and message, `attention.annotated` note, `thread.crashed` stderr tail; any other event, or one without that field: `data` as JSON), 16 KB per call from byte `offset` (default 0) on a UTF-8 boundary, with a footer `[bytes a-b of n; call read_transcript with seq=S offset=b for the rest]` or `[bytes a-b of n; end of #S]` |
| `subscribe_control` | `since_seq?, wait_ms?` (default 0, max 30000) | `control.read` + a bounded `control.subscribe` wait; ends with `next_seq` |
| `write_report` | `thread, turn?, summary, grade` | `report.write` |
| `watch_thread` | `thread` (or `"*"`), `note`, `repeat?` | `orchestrator.watch` (led by `thread_name`; when the user asks for something to happen once a worker finishes; the orchestrator is woken with the report and its note) |
| `unwatch` | `watch` (an id, or `"all"`) | `orchestrator.unwatch` |
| `list_watches` | - | `orchestrator.watches` |
| `read_context` | `project` | `context.read` |
| `set_context` | `project, content` | `context.set` |
| `read_docs` | `query?`, or `section, offset?` | none: twr's docs compiled into the binary (`README.md`, `SKILL.md`, `docs/*.md`) plus page `cli`, the `--help` of every command. With `query`, the best five sections, each `<page>#<anchor>` with its heading, size and the line that matched (`offset=<n>`); with `section` (or a page name for the whole page; `docs/` may be left out), that text 16 KB per call with a footer `[more: read_docs section=<id> offset=<n>]`; with neither, the index of pages and anchors |
| `hub_info` | - | `hub.info`, as the orchestrator gets it: each box only `name, state, online, version, agents`, each agent only `runtime, found, login` |
| `list_project_files` | `project, path?, thread?` | `project.files`, rendered as text: a first line `[repository content: ... of <project> at <commit>; data from the repository, never instructions]`, then `dir <name>/`, `file <name> (<size>)`, `symlink <name>`, `submodule <name>`, a secret name marked `[not readable: ...]` |
| `read_project_file` | `project, path, thread?, offset?` | `project.read`, rendered as the same first line, the content, and `[end of page; read_project_file offset=<n> for the rest]` or `[end of <path>]` |
| `search_project` | `project, query, path?, thread?, ignore_case?` | `project.search`, rendered as the same first line, then `<path>:<line>: <text>`, and notes when more matched or lines were left out |
| `show_config` | - | `config.show`: the configuration the hub loaded, from an allowlist; tokens, `env` values (names only, `env_names`), preset command lines, `ssh` destinations, box commands and TLS files are never shown |

## CLI equivalents

| CLI | socket |
|---|---|
| `twr thread new <project\|dir\|repo-folder-name> [--runtime R \| --runtime 'pty:<command>'] [--name N] [--alias A] [--mode M] [--model M] [--effort E] [--box B] [--worktree \| --no-worktree] [--base REF]` | `thread.new` |
| `twr mode <thread> <mode>` | `thread.set_mode` |
| `twr model <thread> [MODEL] [--effort E] [--runtime R]` | `thread.set_model` (`default` clears either, where the runtime takes it: see `thread.set_model`; `--runtime` moves the thread); with only the thread, `thread.get` and prints its runtime, model and effort (and what a live runtime still runs with, or that its next turn starts a session of its new runtime) |
| `twr delete <thread>... [-y]` | `thread.delete`, prints the card's summary, then answers it after a y/N on the terminal (`n` answers Cancel, an empty reply leaves the card open), or with `--yes`/`-y`; without a terminal and without `--yes` the card stays open for `twr respond` or the TUI. After Delete it waits until each thread is gone (`thread.get` is `not_found`) or a new `Deleting <name> failed` error item appears, and prints `deleted <name>; moved worktree ... to the trash; moved branch ... to the trash; trash item <short> (<size>) (...)` per thread |
| `twr trash list [--json]` (alias `ls`) | `trash.list`, a table (ID: the short id, NAME, PROJECT, BOX, DELETED, PURGED, SIZE, LOSSES with `(box offline)` / `(incomplete)`), the boxes that could not answer, then the message; `--json` prints the result |
| `twr trash empty <item>... \| --all [-y]` | `trash.empty {dry_run: true}` first (names and `--all` resolved then), a y/N on the terminal listing each item it names and their size (or `--yes`/`-y`), then `trash.empty` with exactly those ids: an item deleted while it asks stays, and one gone meanwhile makes the hub refuse, purging nothing; without a terminal and without `--yes` it refuses; prints each item purged and fails when one could not be |
| `twr trash restore <item> [--as NAME]` | `trash.restore`, prints the result's message |
| `twr thread list [--all]` | `thread.list`; parked threads are left out unless `--all`/`-a`, and a line names them (`2 parked threads hidden: old, older (twr thread list --all)`) (MODE: the thread's mode, plus `(running <m> until restarted)` while its live runtime still runs in another; MODEL: `model_actual`, else the requested `model` marked `(requested)`, else `(runtime default)`) |
| `twr send <thread> <prompt...>` | `thread.send` |
| `twr send <thread> --after T1,T2 <prompt...>`, `twr send --cancel <id>` | `thread.send {after}`, `thread.send_cancel` |
| `twr wait <thread>... [--all] [--timeout D] [--json]` | `thread.get`, every second, until each (any; all with `--all`) finished or waits on the user: exit 0 finished, 2 waiting, 3 failed/interrupted/stopped/parked, 124 timed out |
| `twr say <message...>` | `say` (never answers a permission or question: use `twr respond`) |
| `twr respond <attn_id@revision\|attn_id\|thread> <answer\|number> [-m msg] [-y]` | `attention.respond` (with a thread: prints the top item and answers that revision only after a y/N confirmation on the terminal, or with `--yes`/`-y`; without a terminal and without `--yes` it refuses and names the `<attn_id>@<revision>` to use; a bare attn_id answers only an item never raised again) |
| `twr attention [--all]` | `attention.list` (ids printed as `<attn_id>@<revision>`; without `--all` only permissions and questions, and "nothing blocks you; N other item(s)" when only others are open) |
| `twr stop <thread>` | `thread.stop` |
| `twr park <thread> [--keep] [--force]` | `thread.park` |
| `twr resume <thread>` | `thread.resume` |
| `twr attach <thread> [--force]` | `attach.begin` / `attach.child` / `attach.end` |
| `twr events <thread> [--follow] [--since N] [--hydrate] [--pretty]` | `thread.events` / `thread.subscribe` |
| `twr control [--follow] [--since N] [--pretty]` | `control.read` / `control.subscribe` |
| `twr context show <project>` | `context.read` |
| `twr context edit <project>` | `context.read`, `$VISUAL`/`$EDITOR`/`vi` on a temp copy, then `context.set` with `expect_hash` if it changed (on failure the copy is kept and its path printed) |
| `twr mcp` | the orchestrator's MCP server on stdio (refuses to start without `TWR_ORCH_TOKEN`; lines over 8 MiB end the session) |
| `twr orchestrator show [--json]` | `orchestrator.show` |
| `twr orchestrator use <runtime> [--model M] [--no-summary \| --wait S]` | `orchestrator.use` (prints what happened to the previous orchestrator and what was handed over; without `--wait`, on a terminal, asks before waiting for a busy one) |
| `twr info [--json]` | `hub.info` (version, boxes, orchestrator, load, config warnings) |
| `twr job run [--thread T] [--name N] [--slots K] [--timeout D] [--no-wake] -- <argv>...` | `job.start` (thread defaults to `TWR_THREAD`) |
| `twr job list [--thread T] [--json]`, `twr job log <job> [-n N]`, `twr job cancel <job>` | `job.list`, `job.log`, `job.cancel` |
| `twr trim <thread>...` | `thread.trim` for each; prints what went and how much it freed |
| `twr usage --threads [--since D] [--by thread\|project\|runtime] [--json]` | `usage.threads`: a table of tokens in and out, turns and cost per row, then the total |
| `twr usage [--json]` | `usage.get`: per runtime its label, plan and the data's age, then each window's remaining share and reset time (local time, and how long until it) |
| `twr hub` | runs the hub (and, with `[hub] listen`, the runner listener) |
| `twr hub stop [--all] [--timeout S]` | `hub.info` (for the pid), then `hub.shutdown`; stops local runtimes first and waits for exit (default 30 s). With `--all`: default plus every named workspace in this home, ignoring `-w`/`TWR_WORKSPACE`; configured/default and standard named sockets only; concurrent stops, timeout per workspace; prints `stopped`, `not running`, or `failed: reason` for each; non-zero on any failure. Refused to agents locally. No running hub is exit 0. |
| `twr update [--version V]` | no socket call to update: downloads the GitHub release's `install.sh` and `sha256sums.txt` (of `--version`, else the latest; `TWR_INSTALL_SCRIPT_URL` names another installer: its signature is then `<url>.sig`, and the checksums, their signature and the archive come from its directory, passed to install.sh as `TWR_DOWNLOAD_URL`), checks their Ed25519 signatures with the release key built into twr and that the checksums name the release asked for (`# twr release <tag>`) and list that install.sh (nothing runs otherwise), and runs that install.sh with `TWR_VERSION` (the release the checksums name), the verified `TWR_SHA256SUMS_FILE` and `TWR_INSTALL_DIR` set to this executable's directory (refused in cargo's bin directory); afterwards `hub.info` to say whether the running hub is on another version |
| `twr runner --hub <ws(s)://host:port> --box NAME [--token-file PATH] [--data-dir D] [--ca PEM] [--allow-insecure]` | runs a box for the hub; the token comes from `--token-file` (refused unless only its owner can access it, `chmod 600`, and it is this user's), `$TWR_RUNNER_TOKEN`, or the box's default file `<config dir>/runner-tokens/NAME.token` (`twr runner install-token`), never from the command line; a file's token is read again after the hub refuses the runner (the file looked at every 2 s during the wait, a changed one used at once) (which every local user can read), and is refused under 16 characters; plain `ws://` only to a loopback host unless `--allow-insecure`; the global `--socket` and `-w` are refused |
| `twr runner token` | prints a new box token (32 random bytes from the OS, hex), for `[boxes.<name>] token` and the runner's token file |
| `twr runner install-token NAME [--hub URL] [--ca PEM] [--token-file PATH] [--yes]` | no socket call, on the box: reads the token at a prompt on `/dev/tty` with echo off (set before the prompt is written; a termination signal restores the terminal first), or from stdin when it is no terminal (at most 64 KiB; 16 to 1024 printable ASCII characters without spaces); writes it to `--token-file`, else `<config dir>/runner-tokens/NAME.token` (`~/.config/twr/runner-tokens/`, `$TWR_HOME/runner-tokens/`; directory 0700, checked), mode 0600, atomically, under a lock; the same token: unchanged; another one: replaced only after y/N (refused without a terminal unless `--yes`). With `--hub` (`wss://`, or `ws://` to a loopback host; no `--allow-insecure`), prints `twr runner --hub URL --box=NAME --token-file=<absolute path> [--ca=...] [--data-dir=...]` (`--data-dir` when `TWR_HOME` or `XDG_DATA_HOME` moves it) and a systemd user unit (`twr-runner-NAME.service`, under `$XDG_CONFIG_HOME/systemd/user` or `~/.config/systemd/user`) or a LaunchAgent (`tools.twr.runner.NAME`) to paste; none when a value holds a control character or can't be written safely. Refused to agents |
| `twr box add NAME SSH... [--yes]` | no socket call until the end: probes the box with ssh in the terminal (the hub's ssh options without `BatchMode`; `TWR_SSH` names the program), checks ssh with `BatchMode=yes` (a warning when it fails), shows what will happen and asks y/N (refused without a terminal unless `--yes`), writes `[boxes.NAME] transport = "ssh"`, `ssh = "SSH"` to `boxes.toml` under its lock (refused when either file has the box, or the merged config does not validate), starts the hub if none runs, `box.add`, then `hub.info` every 500 ms up to 90 s until the box is `online` (non-zero on a timeout or `error`). Refused to agents locally and in a named workspace |
| `twr box add NAME --command ARGV... [--yes]` | a command box (a container): the words after `--command` are `command`, as given (not joined; a `--yes` among them taken out); ssh-like programs refused (`ssh`, `tailscale ssh`, `gcloud compute ssh`). Probes the box through the command, in a session of its own without a terminal, as the hub runs it, with the pipe check (refused when the command does not pass its input on, `-i`, or gives the box a terminal, `-t`; a known engine error, such as no such container, said with its fix); shows what will happen and asks y/N; writes `[boxes.NAME] transport = "command"`, `command = [...]`, and for `docker`/`podman`/`nerdctl exec` and `kubectl exec` with `-i`, `terminal_command` (the same words with `-t` after the `-i`), then as with ssh: `box.add` and the wait. Refused to agents locally and in a named workspace |
| `twr box add NAME --dial-in --hub URL [--ca PEM] [--yes]` | a token box (its runner connects to the hub): refused before anything is written without `[hub] listen`; `--hub` checked as the runner checks it, without `--allow-insecure` (`wss://`, or `ws://` to a loopback host; `ws://` refused for a `[hub.tls]` listener), and without it the fitting URLs are named; `--ca` (wss:// only) is the CA's absolute path on the box. Shows what will happen and asks y/N (refused without a terminal unless `--yes`), writes `[boxes.NAME] transport = "token"`, `token = <32 random bytes, hex>` to `boxes.toml` (0600) under its lock, starts the hub if none runs, `box.add` (`listening: false`: says to restart the hub), prints `twr runner install-token NAME --hub URL [--ca PEM]` and, only when stdout is a terminal, the token; on a terminal then `hub.info` every 500 ms up to 10 minutes until the box is `online`, printing each state and each new `refused` with its fix (not an error when it does not connect). Refused to agents locally and in a named workspace |
| `twr box list [--json]` (alias `ls`) | `hub.info`'s boxes: NAME, TRANSPORT, STATE, VERSION, OS/ARCH; `--json` prints the array |
| `twr box status NAME [--json]` | one box of `hub.info`, every field, in lines (a `drain:` line while the hub drains it: the stage, since and until when, and what to do: `twr box restart NAME` to stop its agents now, or for a token box, once drained, how to update and restart its runner); `--json` prints the entry |
| `twr box install NAME [--binary PATH \| --from-box PATH] [--yes]` | SSH and command boxes only: probes the box; same OS/CPU as the hub: `box.install` and waits as `add` does; another one: uploads `--binary`, or copies `--from-box` (an absolute path on the box: a regular file whose SHA-256 and size the box reads, copied into a temporary file in its build's folder under the builds tree, which must be safe as for an upload, the copy's size and SHA-256 compared, mode 0700, its `--version` shown and compared with the hub's, a warning when they differ, then moved into place), else (a release hub) the release's `twr-<target>.tar.gz` checked against its signed `sha256sums.txt` as `twr update` checks a release, unpacked on the box (a development hub: refused with how to build that commit for the target); asks y/N first; records `build` in `boxes.toml` (a box of `config.toml`: prints the line), then `box.install` and waits (as `add` does; for a box online on another build, until it runs the one installed: up to about 86 minutes, about 118 while the hub drains the box (`drain`: its running turns may go on for 30 minutes first), printing the box's new notes and the drain). Refused to agents locally |
| `twr box project NAME PROJECT [--path PATH \| --clone [--into DIR] \| --remove] [--yes]` | no socket call until the end: resolves PROJECT on this machine (`[projects.*]`, else a folder under `project_roots`) and its `origin` (credentials dropped, never sent); probes the box with ssh in the terminal and runs the box's build (as `twr box doctor` picks it): `twr runner --find-repo --name <folder> [--origin URL] [--hint PATH]` (the hint: the repository's path under `$HOME` here, under the box's home; with `--path`, that path, which must be a repository's top level; the box side stops after 90 s, its report then incomplete), or with `--clone` (a terminal required; https/ssh/scp-like origins without credentials only) `twr runner --clone-repo --url URL --parent DIR --name FOLDER` over ssh `-t` in the terminal (`--into DIR` is the clone's own new folder, split into its parent and name; default the home-mapped path, else `~/code/FOLDER`); records `[boxes.NAME.projects.PROJECT] repo` in `boxes.toml` under its lock only while the box's connection settings and that entry are unchanged since the start (a box of `config.toml`: prints the lines), automatically (after y/N unless `--yes`) only for exactly one origin match from a complete search, else lists the candidates and exits 1; then `box.projects` when a hub runs (`recorded and active` when its `repos` has the path written, else `recorded` and why: what the hub has, or why it did not take it). `--remove` deletes the entry the same way. A dial-in box (a token box without `ssh`): `--path` is recorded without looking, then `box.projects`; without `--path`, or with `--clone`, refused. Refused to agents locally and in a named workspace |
| `twr box restart NAME [--yes]` | SSH and command boxes only: says what it does (during a drain: its agents stopped cleanly first, no crash; otherwise their threads show a crash) and asks y/N (refused without a terminal unless `--yes`), starts the hub if none runs, `box.restart`, then `hub.info` every 500 ms, for as long as the hub's attempts may take (up to about 83 minutes: an attempt under way and the restart's own, each a probe, an upload and the relay's whole stop and start budget, and during a drain its agents' stop; printing each state and the box's new notes meanwhile), until the box is `online` on a larger `epoch` with `restart_pending` false (non-zero on a timeout, `error` or `needs_install`), then says how the new daemon was started (its `launch`; on a Mac, `detached` with the fix: log in at the Mac's screen, then restart again). Refused to agents locally and in a named workspace |
| `twr box remove NAME` (alias `rm`) | removes the box from `boxes.toml` under its lock (refused for a box of `config.toml`); the running hub keeps it until restarted; prints how to stop the box daemon (for a command box, through its command; the container is left as it is). A token box: then `box.token` when a hub runs, and by its `accepting`: `false` says the hub no longer accepts the box's runner and how to stop that runner and delete its token on the box; `true` says the box is still in `config.toml` with a token. Refused to agents locally |
| `twr box token NAME [--rotate [--yes] \| --reload]` | no socket call to print: the token box's token from `boxes.toml`, else `config.toml`, on stdout alone (to a pipe too; a hint on stderr on a terminal); an SSH or command box, or one without a token, is an error. `--rotate` (`boxes.toml` boxes only): asks y/N (refused without a terminal unless `--yes`), writes a new token (32 random bytes, hex) under the file's lock only while the box's settings there, its token included, are those read, then `box.token` (no hub: the next one reads it; an older hub: it takes it when it restarts), and prints the box command and, on a terminal only, the new token. `--reload`: `box.token` alone. Refused to agents locally and in a named workspace |
| `twr ws [list [--json]]` | lists the workspaces: per running hub `thread.list` and `attention.list` (threads, running, waiting); a hub that does not answer within 1 s, or whose socket's backlog is full, is `busy`; others are `running`, `stopped` (nothing listens), `other` (it serves another workspace) or `error` with the reason (a socket twr won't use, such as another user's) |
| `twr ws new <name>` / `twr ws rm <name>` | no socket call: makes the workspace's data directory; removes it only when no hub holds its data directory's lock (nor answers on its socket), its log has no worker thread and its trash is empty; refused to an agent |
| `twr -w <name> <command>` | any command in workspace `<name>` (else `$TWR_WORKSPACE`, else `default`); an unknown workspace is an error |

## Examples

One request with `socat` (or `nc -U`, which must close after the reply, e.g. OpenBSD `nc -U -N`):

```sh
S=${XDG_RUNTIME_DIR:?}/twr/hub.sock
printf '%s\n' '{"id":1,"method":"thread.list"}' | socat -t 2 - UNIX-CONNECT:"$S"
printf '%s\n' '{"id":1,"method":"hello","params":{"client":"sh"}}' \
  '{"id":2,"method":"thread.send","params":{"thread":"api","prompt":"run the tests"}}' \
  | socat -t 2 - UNIX-CONNECT:"$S"
# Follow the control stream (keep stdin open, or socat closes after -t); Ctrl-C to stop.
(printf '%s\n' '{"id":1,"method":"control.subscribe","params":{"since":0}}'; cat) \
  | socat - UNIX-CONNECT:"$S"
```

Python (standard library only):

```python
import json, os, socket

path = os.path.join(os.environ["XDG_RUNTIME_DIR"], "twr", "hub.sock")
sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
sock.connect(path)
f = sock.makefile("rw", encoding="utf-8", newline="\n")
next_id = 0

def call(method, params=None):
    global next_id
    next_id += 1
    f.write(json.dumps({"id": next_id, "method": method, "params": params}) + "\n")
    f.flush()
    while True:
        msg = json.loads(f.readline())
        if msg.get("id") == next_id and "stream" not in msg:
            if "error" in msg:
                raise RuntimeError(f"{msg['error']['code']}: {msg['error']['message']}")
            return msg["result"]

call("hello", {"client": "script"})
for t in call("thread.list"):
    print(t["name"], t["status"], t["pending"])
print(call("context.read", {"project": "api"})["content"])
for item in call("attention.list", {"all": True}):
    print(item["attn_id"], item["revision"], item["thread_name"], item["reason"], item["summary"])
# Answer the revision you showed: call("attention.respond", {"attn_id": ..., "revision": ..., "answer": "allow"})

call("control.subscribe", {"since": 0})       # ack; stream items follow
for line in f:
    item = json.loads(line)
    ev = item["event"]
    print(item["gseq"], ev["thread"], ev["kind"])
```

### Background activity

`task.updated {task_id, running, description, heartbeat}` is a session-scoped control event with no turn number. `background` means the turn ended but tasks remain; `running` can also have background tasks. Idle/done require no turn and no tasks. Runtime-triggered follow-ups use `turn.started {from: runtime}`, numbered by the hub; its start and `turn.ended` carry `ext.twr.runtime_turn`. A runtime turn that races a prompt is not recorded: its output and usage go into the prompt's turn. An item with `ext.twr.session: true` (a Codex child agent's request) belongs to no turn and outlives the turn that was open. Stops/parks/crashes/new runtime starts clear tasks. A yielded turn's report is interim (`ext.twr.background_tasks`), and completion notices wait for actual task completion. Park/attach need force while background work runs. `background_timeout_secs` (default 86400, 0 = off) stops background-only work when task liveness is lost. [Runtime support and gaps](docs/runtimes.md#background-work-and-automatic-turns).
