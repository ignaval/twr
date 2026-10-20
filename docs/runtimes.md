# Runtimes

A runtime is the agent CLI behind a thread. twr drives each one through its headless JSON mode, so it sees tool calls, permission requests and turn ends as structured events. Anything without a headless mode can still run under a pseudo-terminal, in a degraded tier.

Pick one per thread with `twr thread new --runtime`, per project with `[projects.<name>] runtime`, or for everything with `default_runtime`. See [configuration.md](configuration.md).

## Tiers

| Runtime | Tier | How twr drives it |
|---|---|---|
| `claude-code` | headless | `claude -p` with stream-json in and out; permissions over the SDK control protocol |
| `codex` | headless | `codex app-server` JSON-RPC; approval requests relayed |
| `grok-build` | headless | `grok agent stdio` (ACP) plus Grok's extensions; permissions, questions and plan approval relayed |
| `acp:<command>` or a `[runtimes]` preset | headless | any Agent Client Protocol agent over stdio, standard ACP only |
| `pty:<command>` | degraded | any interactive CLI in a pseudo-terminal, with guessed turns and prompts |
| `fake` | test | scripted; only with `TWR_ENABLE_FAKE=1`, for tests and the [screenshots](development.md#screenshots) |

The binaries are `claude`, `codex` and `grok` on `PATH`. `TWR_CLAUDE_BIN`, `TWR_CODEX_BIN` and `TWR_GROK_BIN` point twr at other ones. On a remote box, the runner puts its login shell's `PATH` first ([runners.md](runners.md#agents-on-a-box)). Sign in with each agent's own CLI first: twr never runs a login by itself and never sees your keys. `twr box login <box> <agent>` starts the agent's own login on a remote box, in your terminal. Every runtime runs in a session and process group of its own, with twr's secrets, such as the runner and orchestrator tokens, removed from its environment. A headless runtime has no controlling terminal; a `pty:` runtime has its pseudo-terminal as its own private one. Neither can use the terminal of whoever started the hub.

## What has been checked against the real CLIs

| Runtime | Checked against | Tests |
|---|---|---|
| Claude Code | 2.1.284; 2.1.110, 2.1.197 and 2.1.258 by live probes | recorded real sessions in `crates/twr-adapters/tests/fixtures/claude/` |
| Codex | 0.155.1 | recorded real sessions in `crates/twr-adapters/tests/fixtures/codex/` |
| Grok Build | grok 1.0.44, on 2026-09-30 | recorded real sessions in `crates/twr-adapters/tests/fixtures/grok/` |
| generic ACP | a scripted ACP agent, and the real `grok agent stdio` run as a plain ACP agent | `crates/twr-adapters/tests/acp.rs`, `crates/twr-hub/tests/acp_runtime.rs` |
| PTY | scripted programs | `crates/twr-adapters/tests/pty.rs` |

For Grok that covered sessions, permissions, questions, plans, cancel, resume, MCP servers and the orchestrator's tool restriction. Not checked yet: Grok threads on a remote runner box, and Grok's interactive TUI beyond resuming the session. No ACP CLI other than grok has been tried.

## Permission modes

Autonomy is each runtime's own permission mode, which you set per thread. A thread's mode is `--mode`, else `[threads] default_mode`, which defaults to `auto`. `twr mode <thread> <mode>` changes it later, or ask the orchestrator; see [usage.md](usage.md#change-a-threads-mode). twr never widens a mode on its own.

`auto` means the agent's own auto mode decides which tool calls need asking. What still asks reaches you as a permission card, and only you answer it: twr never answers a prompt, in any mode. If you want every edit and command to ask, use `manual`. The orchestrator is never in `auto`: it runs `dontAsk` with only the twr tools on Claude Code, and at most `manual` elsewhere.

twr's own modes work on every runtime, narrowest first:

| mode | what it allows |
|---|---|
| `plan`, `dontAsk`, `read-only` | plans or reads only, or denies what isn't allowed |
| `manual`, `default` | asks before edits and commands |
| `acceptEdits` | accepts file edits, asks for commands |
| `auto` | the agent's own auto mode decides what to ask (the default) |
| `bypassPermissions` | asks nothing |

A runtime also takes its own mode ids, listed below for each one. Case, `-` and `_` don't matter (`read_only`, `Read-Only` and `readOnly` are all `read-only`), and twr records the mode in the spelling above. Any other mode is refused when the thread is created, and an unknown `default_mode` stops the hub from starting. The error lists the modes that runtime takes. A `default_mode` that is one runtime's own id, such as Codex's `never`, works only for that runtime's threads.

Claude Code takes `default`, `manual`, `plan`, `dontAsk`, `acceptEdits`, `auto` and `bypassPermissions`, and gets them as `--permission-mode` unchanged. `read-only` becomes `plan`. In `auto`, Claude Code's own auto mode decides which tool calls run without asking; the rest arrive through `--permission-prompt-tool stdio` as permission cards. A Claude Code too old to list `auto` doesn't start an `auto` thread: the error says the mode is the thread's, how to change it, and to run `claude update`.

Codex has no single mode setting, so twr maps the mode to an approval policy and a sandbox. It also takes its own `untrusted`, `on-request`, `never`, `danger-full-access` and `yolo`:

| twr mode | approval policy | sandbox |
|---|---|---|
| `manual`, `default`, `untrusted` | untrusted | workspace-write |
| `plan` | untrusted | read-only |
| `read-only` | on-request | read-only |
| `on-request` | on-request | workspace-write |
| `auto` | on-request (`[codex] auto` can change it, below) | workspace-write, plus network, the worktree's git metadata, `/dev/shm` and build caches |
| `acceptEdits` | untrusted | workspace-write |
| `dontAsk` | never | read-only |
| `never` | never | workspace-write |
| `bypassPermissions`, `danger-full-access`, `yolo` | never | danger-full-access |

When the mapping isn't exact, the thread's start event carries a note saying so.

Codex has no auto mode of its own, so `auto` is Codex's on-request policy: commands inside the sandbox run without asking, and anything beyond it asks you. Codex's plain workspace sandbox is too tight for routine work in a twr worktree: it has no network, which blocks every socket (tests that bind a unix socket fail), and a worktree's git metadata lives in the main repository's `.git`, outside the worktree, so `git commit` asks. So in `auto`, twr widens the sandbox (checked with Codex 0.162.1):

- network access is on;
- the thread can write its worktree's git dir (`.git/worktrees/<name>`) and the repository's `.git/objects`, `.git/refs` and `.git/logs`, enough to commit, stash, tag, fetch and rebase. `.git/config` and `.git/hooks` stay read-only, because git runs what they name outside the sandbox. A thread working in a main checkout rather than a worktree still asks to commit. twr finds these directories from the repository the thread was made from, and the worktree's record there from git's own record of which worktree it belongs to, not from the worktree's own `.git` file or its git dir's `commondir`, which the agent can rewrite: if those point anywhere else, such as another worktree's record, nothing of git's is writable and a commit asks;
- the thread can write the cargo caches (`$CARGO_HOME/registry` and `git`, `~/.cargo` by default; not `bin` or `config.toml`), `$CARGO_TARGET_DIR` when the thread's environment sets one (`[threads] env`, a project's `env`, the hub's own), and the npm cache (`$npm_config_cache`, else `~/.npm`), where they exist. Each must be your own folder, named as it is, through no symbolic link, with no folder above it that another user could change; it must lie in nothing the sandbox can write (the worktree, `/tmp`, `$TMPDIR`, `/dev/shm`), in the repository's `.git` or in another checkout of the repository; and none may be or hold your home directory, the worktree, `.git` or another checkout: Codex finds its writable folders again for every command, so one inside a folder the agent can write could be swapped for a link to anywhere. A path that fails is left out. pnpm's store isn't among them: pnpm hard-links its files into other projects' `node_modules`;
- the thread can write `/dev/shm`, scratch space like `/tmp` (which Codex's sandbox already lets it write), when it is root's, sticky and writable by all, and holds no checkout of the repository. Without it, Codex gives each command an empty `/dev/shm` of its own, and what one command writes there is gone for the next.

The repository's `.git` itself stays read-only. Git locks `.git/packed-refs` whenever it deletes a ref, so in a worktree `git branch -d`, `git tag -d` and the end of a cherry-pick or merge print `error: Unable to create '.../.git/packed-refs.lock': Read-only file system`, and those ask. A rebase prints the same error but has done its work. twr adds a short note about this to an `auto` thread's developer instructions, after those of your Codex config (`developer_instructions`, which a request's own would otherwise replace), so Codex checks `git status` instead of asking to run the rebase again. If twr can't read the config, it leaves the note out. Making `.git` writable would let the agent create files there, such as a `commondir` or a `config.worktree`, that your own git would then follow outside the sandbox.

Writing anywhere else, such as another checkout, your home directory or `~/.cargo/bin`, still asks, and so does anything Codex itself decides to check with you first. Network access also opens local sockets, so a command can reach your user session's D-Bus and start something outside the sandbox (`systemd-run --user`). That is as open as Claude Code's auto mode, which has no OS sandbox. If you need the stricter sandbox, use `on-request`.

### Who answers when the sandbox says no

What the `auto` sandbox doesn't allow, Codex asks to run outside it. `[codex] auto` in the [configuration](configuration.md) decides who answers, for every Codex thread in `auto`, and a project's `codex_auto` overrides it for that project's threads:

| `[codex] auto` | Codex runs with | what happens |
|---|---|---|
| `ask` (the default) | `on-request`, reviewer `user` | a permission card; only you answer it |
| `auto_review` | `on-request`, reviewer `auto_review` | Codex's reviewer agent decides, and you aren't asked: what it approves runs outside the sandbox, what it denies doesn't run. Each decision shows in the thread's transcript as an `auto_review` tool call, with the command, the risk Codex gave it and its reason |
| `never` | `never` | nothing asks: what the sandbox refuses fails, and Codex works around it or reports it |

`auto_review` is the closest to Claude Code's auto mode: a model, not you, decides which commands go beyond the sandbox. It is your Codex's own reviewer (`approvals_reviewer = "auto_review"` in Codex's config), which judges each request by its risk and by what you asked for. twr never sees a request before it decides. `never` takes nothing away from the sandbox and adds nothing to it: it only stops the asking. The other modes are unchanged: they always send the reviewer `user`. The hub reads `[codex] auto` when it starts, and a thread's runtime takes it when it starts or resumes; a runner box from before this setting asks.

Codex adds the `[sandbox_workspace_write]` `network_access` and `writable_roots` of your Codex config, or a project's `.codex/config.toml`, to any workspace-write sandbox unless twr overrides them. So in every mode with that sandbox twr sends both: in `auto` its own list and network on, in the others no network and no extra roots. If Codex still reports network access or a writable root twr didn't ask for, or a reviewer other than the one twr asked for, twr ends the session before any prompt; and the same for a `cwd` other than the one it asked for (the sandbox lets commands write it). If a thread's settings change to any of that later in the session (`thread/settings/updated`, a child agent's too), or arrive in a message too long to check, twr ends the session then. The thread's start event lists the roots as `ext.codex.sandbox_writable_roots` and the reviewer as `ext.codex.approvals_reviewer`. `twr attach` passes the same settings, except that an attach to a thread on a runner box gets no extra roots, so a commit there asks.

Codex's own allowlist, `prefix_rule` entries in `~/.codex/rules/*.rules` (or a trusted project's `.codex/rules`), applies to twr's threads too. Codex documents that a rule with `decision = "allow"` runs matching commands outside the sandbox without asking. twr adds no rules and never picks Codex's "always allow" answer for you.

Grok Build takes `default`, `plan`, `dontAsk`, `acceptEdits`, `auto` and `bypassPermissions` as `--permission-mode`. In `auto`, Grok's own auto mode decides what to ask. `manual` becomes `default` and `read-only` becomes `plan`. Unless the mode is `bypassPermissions`, twr also turns Grok's always-approve off and cancels a session Grok reports as `yolo`. Even in `default` mode, grok 1.0.44 runs some shell commands it deems safe without asking. File writes ask.

An ACP agent takes twr's modes and its own mode ids, such as Gemini CLI's `autoEdit`, which twr keeps as you wrote them and checks against what the agent reports. With `auto`, twr switches the agent to its `auto` mode when it offers one. Otherwise the agent keeps the mode it reports if that is no wider than `auto`, and twr switches a wider one, such as `yolo`, down. An agent that offers no modes runs in its own default. See [Any ACP agent](#any-acp-agent).

`pty:` and `fake` threads apply no mode. Changing a `pty:` thread's mode only records it; its program isn't restarted.

## Models and reasoning effort

A thread's model is `--model`, else `[threads] model`, else the runtime's default; its reasoning effort is `--effort`, else the runtime's default. `twr model <thread> [MODEL] [--effort E]` changes either later, or ask the orchestrator; see [usage.md](usage.md#change-a-threads-model-and-effort). `twr model <thread> --runtime R` moves a thread to another runtime: the thread keeps its worktree and branch, and its next turn starts a new session of the new runtime with a handoff of what the previous one did. How each runtime gets them, as checked on 2026-10-03:

| runtime | model | effort | efforts | back to `default` once the thread has a session |
|---|---|---|---|---|
| Claude Code | `--model` | `--effort` (Claude Code 2.1.288 lists it; a version whose `--help` doesn't is refused with "run `claude update`") | `low`, `medium`, `high`, `xhigh`, `max` | effort yes; model no |
| Codex | `model` on `thread/start` / `thread/resume` | `config.model_reasoning_effort` on the same calls | `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, `ultra` (Codex 0.159.2's models each advertise a subset) | no |
| Grok Build | the `model` config option | the `reasoning_effort` config option (grok ignores `--reasoning-effort` in `agent stdio`) | `low`, `medium`, `high`, `xhigh` | no |
| ACP agent | the `model` config option, else `session/set_model` | the config option of category `thought_level` | the agent's own; one without such an option can't run with an effort | no |

`pty:` threads take neither: give them in the command line.

`twr attach` hands the thread's model and effort to the interactive CLI too: Claude Code gets `--model` and `--effort`, Codex `--model` and `-c model_reasoning_effort=...`, and grok `--model` and `--reasoning-effort`. grok ignores those flags in `agent stdio`, but its interactive CLI honors them on `--resume`: grok 1.0.46, resuming a session made on Grok 4.5 at `low` with `--model grok-4.6 --reasoning-effort high`, said "Switched to Grok 4.6 (high effort)" and the session kept the new values.

A generic ACP agent's attach passes them only where its preset's `attach` has `{model}` and `{effort}` (see [Any ACP agent](#any-acp-agent)). If a thread's model or effort changed while it was stopped, its session still runs the old one until its next start. An attach command that can't pass the new value would resume the session in the old one, so twr refuses that attach and says so: `cannot attach api: its session still runs model "m1", and the acp runtime's attach command cannot switch it to the thread's model "m2"; send api a prompt first (its runtime resumes the session with them), then attach`. Once the session has run with the new settings, the attach works.

A thread with no model or effort leaves it to the runtime, and twr passes nothing. That works for a new session. A resumed session, though, may keep the value it last ran with, so once a thread has a session, twr refuses to set a value back to `default` where the runtime would ignore it. The error says so: `codex keeps a session's effort on resume, so api cannot go back to the runtime's default effort (its session would go on with low): pick an effort instead (...)`. Before the thread's first start, and with a move to another runtime (`twr model --runtime`, whose first session is a new one), `default` always works. What was checked on 2026-10-03:

- Claude Code 2.1.289 keeps the model. A session started on `haiku` and resumed without `--model` still ran `haiku`, even though the settings named `opus`. Its `--model default` picks the account's recommended model, which isn't necessarily the one in your settings, so twr can't use it to mean "your default". The effort isn't kept: a session started with `--effort low` and resumed without `--effort` ran at `medium`, the same effort a new session gets.
- Codex 0.159.2 keeps both. A thread started on `gpt-6-luna` at `low` and resumed with neither still reported `gpt-6-luna` and `low`, though `config.toml` said `gpt-6.1-sol` and `xhigh`.
- grok 1.0.44 keeps both. `session/resume` reported the `model` and `reasoning_effort` set earlier, and its config options don't mark a default to set back.
- For any other ACP agent, twr can't tell, and ACP has no way to mark a default, so twr treats it like Grok Build.

## Subscription usage

twr shows how much is left in each subscription's usage windows (`twr usage`, the TUI's status bar, the orchestrator's `get_usage`). It calculates the remaining share from the runtime's used percentage, clamped to 0–100. It never spends a model turn to find out: each runtime reports usage as it works, and twr asks for it through a read that only queries the provider's usage endpoint. A session reads when it starts and again at the end of a turn once its data is ten minutes old, so the numbers stay fresh while you use twr and go stale while you don't. twr never polls in the background and never reads a token.

| Runtime | What it reports on its own | twr's read | Windows |
|---|---|---|---|
| Claude Code | `rate_limit_event` in stream-json, when a window's rounded percentage or reset time moves (`rate_limit_info.unifiedWindows`, fractions used, reset in epoch seconds; checked in 2.1.295) | the `get_usage` control request with `skip_behaviors` (the plan's windows without the transcript scan; percentages used, ISO reset times), answered with `total_api_duration_ms: 0` | `five_hour` (5h), `seven_day` (wk) |
| Codex | `account/rateLimits/updated` notifications during turns (sparse: a null window is unknown) | `account/rateLimits/read` on the app-server (0.160.0) | `primary` and `secondary`, named by `windowDurationMins`: 300 is 5h, 10080 is wk. Which one is weekly depends on the plan |
| Grok Build | nothing | `_x.ai/billing`, the ACP extension behind grok's `/usage` (1.0.46): the current credit period and `creditUsagePercent`, which grok's protobuf JSON leaves out when it's 0 | the credit period: weekly (wk) or monthly (mo) |
| any ACP agent | ACP has no rate-limit signal yet | none | none |
| PTY | none | none | none |

A read the runtime refuses stops further reads for that session: an older Claude Code without `get_usage`, a Claude Code session on an API key (`rate_limits_available: false`), a Codex logged in with an API key, a Grok account not signed in with grok.com. Claude's `get_usage` is marked experimental and its stream-json `unifiedWindows` internal, so twr treats a missing field as unknown, never as 0% used (except Grok's percentage, as above).

The hub keeps the latest windows per runtime (the one a session was started as: a thread moved to another runtime keeps its previous one until its turn ends, and what that one reports is its own) and records them on its own thread as `account.usage` when a whole percentage, a reset time or the plan changes, or when the last record is ten minutes old ([SKILL.md](../SKILL.md#events-and-streams)). A report that covers only some windows keeps the others until they reset. Entries are kept per runtime and account, so that several logins of one runtime can each have their own later; today every entry is the runtime's own login.

## Claude Code

`claude -p --input-format stream-json --output-format stream-json` with `--permission-prompt-tool stdio`, so every approval comes to twr. A new thread gets a fresh session id; resume uses `--resume <id>`. `twr attach` runs Claude's interactive CLI on the same session in the thread's worktree. The first attach to a new worktree shows Claude's folder trust question; see [usage.md](usage.md#attach).

twr needs Claude Code 2.1.110 or newer. Before the first start of a `claude` binary, twr runs its `--help` and `--version` and remembers the answer for that file. A `claude update` replaces the file, so twr reads it again.

- 2.1.259 added `--permission-prompts`. twr passes `--permission-prompts host` only when `--help` lists it. Older versions get prompts to twr through `--permission-prompt-tool stdio` alone.
- Before 2.1.200 the `manual` mode was called `default`. twr passes `manual` to those versions as `default`, which is the same mode.
- An older version, or one that lacks an option or the permission mode a thread needs, doesn't start. The error names the program and what to do, for example `` `claude` 2.1.100 is too old for twr (needs 2.1.110 or newer): run `claude update` ``. twr never swaps in a wider mode.
- If `--help` fails or takes over 10 seconds, twr passes every option. A version that rejects one crashes with Claude's own error, and twr adds the same `claude update` hint as its last line.

The first thread to start on a new binary waits for that check, so a refusal shows up as the runtime's exit instead of a start error.

If Claude Code is not signed in, its login expired or its API key is invalid, the turn fails with Claude's message plus "(sign in again: run `claude`, then /login)". twr never retries those turns on its own.

When a turn fails for another reason, the error card quotes Claude Code's own report, such as "API Error: Repeated 529 Overloaded errors" or "Reached maximum number of turns (1)". A turn you stop is not a failure.

A thread's [activity line](tui.md#the-activity-line) uses what Claude Code says about its own work. The task its `TaskUpdate` sets in progress, or the `in_progress` item of a `TodoWrite`, is the step it leads with (its `activeForm`, such as "Implementing scroll anchoring"), until that task is completed. A Bash call's `description` says what the command is for. Subagents' tasks don't count. Other runtimes have no such plan in their events, so their line leads with what the agent says it is about to do.

## Codex

`codex app-server` over JSON-RPC, because `codex exec --json` has no approval channel. Command and file-change approvals, MCP tool approvals and questions all become cards. An answer of `allow` accepts; anything else declines.

A turn that fails because Codex is not signed in, or its login expired, says so with "(sign in again: run `codex login`)". Codex reports it as a failed connection with HTTP status 401. twr does not count that as a transient error, so it never retries it on its own.

When a turn fails for another reason, the error card quotes Codex's error message, or names the kind of error when Codex gives no message, such as "Server overloaded" or "Http connection failed (HTTP 502)".

A Codex orchestrator gets twr's MCP server and no other. Codex adds the servers a thread is given to those of your Codex config instead of replacing them (checked with Codex 0.160.0), so twr first reads the config as Codex sees it from the orchestrator's directory (`config/read`, project config included), turns every server it lists off by name (`mcp_servers.<name>.enabled = false`), and turns plugins and apps off too, since they bring servers of their own. A server named `twr` in your config, a config Codex can't read, or a server Codex starts anyway ends the session with the reason. Workers keep your servers.

## Grok Build

`grok --permission-mode <mode> agent --no-leader stdio`, with auto-update off. Permission requests, questions and plan approvals become cards. twr only ever picks "allow once" or "reject once", never an "always" option. Grok ignores MCP allow rules, so a Grok orchestrator asks you before each of its twr tool calls unless its mode is `bypassPermissions`.

When a turn fails, the error card quotes Grok's error, or says why Grok stopped, such as "Grok refused to continue (stopReason refusal)".

## Any ACP agent

`--runtime 'acp:<command>'` runs any agent CLI that speaks the [Agent Client Protocol](https://agentclientprotocol.com) over stdio: `gemini --experimental-acp`, `opencode acp`, `goose acp`, qwen-code, Claude through `claude-code-acp`, and so on. twr splits the command the way a shell splits words. Quotes work and nothing is expanded, so wrap it as `acp:sh -c '...'` if you need shell features. The command can be at most 4096 bytes.

A `[runtimes.<name>]` entry gives an agent a short name, so `--runtime gemini`, `default_runtime = "gemini"` and `twr orchestrator use gemini` all work:

```toml
[runtimes.gemini]
acp = "gemini --experimental-acp"
attach = "gemini --resume {session}"   # optional: what `twr attach` runs
```

In `attach`, `{session}` stands for the thread's session id and `{mode}` for its permission mode as twr recorded it (`default` when it has none), each put into its argument as is, never through a shell. `{session}` is required. `{mode}` is optional, but a thread in `plan`, `dontAsk` or `read-only` mode can't be attached through a template without it: the agent's CLI would start in its own default mode, which allows more.

`{model}` and `{effort}` stand for the thread's model and reasoning effort. Both are optional. Put each one in the same argument as its flag, as in `attach = "gemini --resume {session} --model={model}"`, because twr leaves that argument out for a thread that has no model or effort. A template with `{model}` or `{effort}` as a whole argument (`-m {model}`), or sharing an argument with another placeholder (`--settings=mode:{mode},effort:{effort}`), is refused when the thread is created: leaving that argument out would drop the flag or the other value with it. Without them, twr refuses to attach a thread whose model or effort changed since its session last ran: the agent's CLI would resume the session with the old ones. Use them only where the agent's interactive CLI applies the flag to a resumed session.

Sign in with the agent's own CLI first. twr never runs a login by itself and never sees your keys (`twr box login` starts only Claude Code's, Codex's and Grok Build's).

When a turn fails, the error card quotes the agent's error, or says why it stopped, such as "gemini hit its output token limit (stopReason max_tokens)".

twr speaks only standard ACP to these agents, which sets some limits:

- Every permission the agent asks for becomes a card you answer. twr only ever picks "allow once" or "reject once", never an "always" option. What the agent runs without asking is up to the agent's own settings, and twr can't see it. Check the agent's approval settings before you use it for real work.
- twr sends no permission mode unless the agent reports modes. If it reports one that allows more than the thread's mode, such as a bypass, yolo or always-approve mode, or a mode twr doesn't know, twr switches it down or ends the session. That holds whenever the agent reports its mode: when the session opens, in the answer to a model or mode change, and later during the session. No prompt runs while the mode is wider. An agent that switches itself to a wider mode in the middle of a prompt, without the plan-exit you allowed, has that prompt cancelled and its session ended: switching it back would leave the prompt running in the wider mode until the agent answered. The next prompt starts the session again in the thread's mode. Only reports for the thread's own session count: one naming another session, or none, is ignored, also while the session opens.
- `--model` works only when the agent reports a model setting. Otherwise put the model in the command: `acp:gemini -m gemini-2.5-pro --experimental-acp`.
- As the orchestrator, the agent gets the twr tools over MCP and the orchestrator instructions at the top of its first message, since ACP has no system prompt field. ACP can't take an agent's own shell and file tools away, so it keeps them. The calls it asks about come to you as cards like any other permission, but whatever its own settings let it run unasked, it runs. If the agent's CLI can restrict its tools, put that flag in the command.
- `twr attach` needs an `attach` command in the preset. twr can't guess an arbitrary agent's interactive CLI.

For Grok itself, use `grok-build`, which adds Grok's own extensions: tool restriction, questions, plan approval and the yolo check.

## Any other CLI: PTY

`--runtime 'pty:<command>'` runs any interactive CLI in a pseudo-terminal and guesses what it's doing from the screen. This is the one place twr reads a terminal, and every event it produces is marked degraded.

- Turn ends and questions are guessed from idle time and the last line on screen. A `[y/N]` prompt becomes a question with `y` and `n`.
- There are no tool calls, no permissions and no usage. A CLI's own permission prompts show up as questions at best.
- There is no session: a resume starts the command again, and there is no attach.
- A multi-line prompt arrives as one line unless the program turned on bracketed paste.

The command is split like an `acp:` command, at most 4096 bytes. Use a headless runtime whenever the agent has one.

## Adding a runtime

A runtime is one file in `crates/twr-adapters` that implements the `Adapter` trait from `twr-core`: spawn a session, turn the agent's output into twr events, and take `send`, `respond` and `stop` commands. See [architecture.md](architecture.md#adapters).

## Background work and automatic turns

A job (`twr job run`, [usage.md](usage.md#jobs)) is background work the hub runs for a thread: its task is `job:<id>`, recorded by the hub, and unlike a runtime's own tasks it outlives the runtime. What follows is about a runtime's own tasks.


A thread with tasks still running after its foreground turn ends shows `background`, with a task count. While a foreground turn runs it shows `running` and the same count. A blocking permission/question remains `waiting`. `idle` and `done` mean no observed turn or tasks remain.

The count includes unfinished command/tool calls while their turn is running; it is a count of observed live work, not OS processes. Keeping those calls tracked before they yield prevents a turn ending from briefly showing idle. A child agent counts once, even if it runs several commands.

Codex opts into experimental app-server methods to read its background-terminal inventory. Other experimental notifications remain ignored unless the adapter recognizes them; approval and sandbox policies are checked as before.

That inventory reports live terminal presence, not exit results. A final task disappearance means no work remains; it does not by itself establish that tests or a review passed. The agent's follow-up turn supplies those results.

Codex's parent `thread/status/changed` reports active/idle and approval/input flags, without a session-wide running-task count. A parent's turn ending or becoming idle therefore does not retire its terminals or children. The hub previously derived activity solely from `current_turn`; adapters now send session-scoped task updates independently of that turn.

| Runtime | Signals used | Limits |
| --- | --- | --- |
| Claude Code | `system/task_started`, `task_progress`, `task_updated` (including terminal `patch.status`), and `task_notification`; structured Bash task ids and injected `<task-notification>` completion text | There is no task inventory query in this adapter. Missing lifecycle events fall back to the timeout. A `run_in_background` request alone is not proof that a task started. A resumed session may first consume the notice of a task its previous process left behind, with a `result` of `origin.kind: task-notification` and `num_turns: 0`, before it reads the prompt: that `result` ends no turn and opens none, and the prompt's turn ends with the answer. Its `total_cost_usd` is the session's earlier spend when no run of this process has reached the model yet; after one, what it adds goes to the next charged turn. |
| Codex | `commandExecution` item lifecycle, `processId` with a missing exit code for yielded commands, `collabAgentToolCall.agentsStates`, `subAgentActivity`, child thread status; `thread/backgroundTerminals/list` and metadata-only child `thread/read` | Inventories are reconciled at turn end and every 5 seconds. Unknown-method errors disable terminal polling on older servers. Child agents count as one task, including their background terminals. Unreported work may take until the next inventory to appear. |
| Grok Build / generic ACP v1 | `tool_call` / `tool_call_update` with `in_progress`, `completed`, or `failed`; unfinished tool calls are kept after the prompt response | ACP v1 has no portable background-task inventory or autonomous-turn boundary. A tool reported completed while its detached process keeps running cannot be inferred as active. Grok's currently supported extensions add no task inventory. |
| PTY | No structured task lifecycle | Screen silence and guessed turn completion cannot establish whether detached work exists. Background work remains unobservable. |

Claude's API message start or output after a result (or a `result` of a run that showed none) and Codex's unsolicited `turn/started` open a new `from: runtime` turn. Claude's `init` alone opens none, since a run may never reach the model, but a prompt written after it waits for that run to end. The adapter sends it without a number, and the hub gives it the next one. Its start and end carry the adapter's id for it: the Codex turn id, or a counter for Claude. If the hub has just sent a prompt as that turn, the runtime's own turn isn't recorded: its output goes into the prompt's turn, its end doesn't close that turn, and its usage counts for it. Codex can start its own turn after twr's `turn/start` went out but before the answer. The adapter waits for the answer to tell the two apart. Everything that turn reports meanwhile waits for it too: text, tool calls, questions, approval requests and token usage. A request withdrawn before then is never shown. When a turn Codex started on its own completes, its unanswered requests are withdrawn, since the hub may have folded that turn into twr's, whose end doesn't close them. A child agent's approval request is session-scoped: the parent's turn ending doesn't close it. Child-agent output and task progress do not open a parent turn. Live task and turn identities are kept independently; terminal notifications do not resurrect a task when late progress arrives. Remote threads require a matching updated runner (protocol 4 or later); the existing version check refuses older runners with an update instruction, since older hubs cannot decode runtime-initiated turns.

Automatic retries and runtime restarts for settings changes wait for background tasks to finish. The user can still send a prompt between turns while those tasks run.

Codex shell/child output, Claude task progress and ACP tool output reset stalls as real progress. Progress observations with the same description are coalesced to at most once per second; unchanged inventory observations only renew liveness, at most once per minute. For inventory-based tracking, use a background timeout comfortably above that minute.

The parser tests use synthetic protocol-shaped messages, alongside the existing recorded sessions. Codex's inventory and child-item fields were checked against the installed **0.160.0** app-server JSON schema; Claude's installed CLI is **2.1.291**, and its task message shapes follow the [official SDK types](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/types.py). See also the [Codex app-server protocol](https://github.com/openai/codex/tree/main/codex-rs/app-server) and [ACP v1 tool-call lifecycle](https://agentclientprotocol.com/protocol/v1/tool-calls). Background scenarios have not been recorded from signed-in live runtimes.

`background_timeout_secs` defaults to **86400** (24 hours), with `0` disabling it. If background-only work receives no task liveness signal for that long, twr durably stops the runtime and its tasks and raises one error card. This is a fallback for lost terminal events, not a maximum command duration: Codex inventories and Claude task progress renew liveness. Increase it for long, silent work on runtimes without inventories. Checks are suspended for disconnected remote boxes; reconciliation resumes when the runner reconnects. Runtime exit/restart and stop clear tracking immediately through their recorded lifecycle events.
