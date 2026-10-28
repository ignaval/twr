# Design: several accounts per runtime

Status: approved 2026-10-20. Phase 1 is built on branch `twr/multi-account-design`, rebased onto `dev` at `cfd1937` (2026-10-27) to merge into the local `dev`; it stays off the public repo until the user decides (the failover and balancing parts touch the providers' terms). [Decisions](#decisions) lists what the reviews settled.
Dates: designed 2026-10-08 against Claude Code 2.1.291 to 2.1.294, Codex 0.160.0 and grok 1.0.46; rechecked 2026-10-20 against Claude Code 2.1.304, Codex 0.162.1 and grok 1.0.50 on a `dev` that already has usage per account, remote boxes with agent login probes, card titles that name who runs a thread, and the orchestrator's `get_usage`, `read_docs` and `spawn_thread {box}`. [Phase 0 results](#phase-0-results) has the live probes.

## Summary

One person can hold more than one subscription for the same agent: a work and a personal Claude plan, two ChatGPT accounts for Codex, a Grok account per client. Each runtime already lets a process pick its login through one environment variable that moves its whole config directory (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, `GROK_HOME`). twr can use that and nothing else. It never reads, stores or passes a token.

The design adds an **account** as a layer on top of a runtime. A thread runs on `(runtime, account)`. Every runtime always has one account, the **native** one, which is whatever login the runtime finds in its default directory today. Adding accounts is opt-in, and a runtime with a single logged-in account looks and behaves exactly as it does now: no labels, no new environment, no new prompts.

What it gives the user:

- `twr account add claude work` creates an isolated config directory, links the shared parts (settings, `CLAUDE.md`, agents, skills and so on) back to the native one, and runs Claude Code's own `claude auth login` in that directory in the user's terminal. `twr account list`, `status` and `remove` cover the rest of the lifecycle.
- Pinning per thread (`--account work`, or `--runtime claude-code:work`), per project (`[projects.x] account`) and for the orchestrator (`[orchestrator] account`, `twr orchestrator use claude-code:work`).
- Balancing new threads across a pool of accounts (`round-robin`, `least-busy`, `most-headroom`), off by default and carrying the same terms warning as failover.
- Flexible pins: a name, a glob or a list, per project, per runtime, per box project and per thread, plus exclusions. A pin that names an account the runtime doesn't have refuses the thread, with an error that says how to add the account or change the pin.
- Accounts declared in `config.toml` (for dotfiles) or recorded by `twr account add`, or both; config wins, `twr account list` says where each one comes from, and the account commands never write `config.toml`.
- Usage-limit detection with the reset time, shown on an error card with a one-key move to another account. Automatic failover is opt-in per runtime, and the docs carry the providers' terms next to the setting.
- Manual moves: `twr model <thread> --account personal`, `/account <thread> personal`, and `set_model {account}` over MCP. A move is a new session plus twr's existing handoff.
- An account label, `claude-code:work`, on cards, tiles, the dive header, the status bar, the CLI and the orchestrator's view, shown **only for runtimes that have two or more logged-in accounts**. The event log always records the account, so history keeps it even after labels disappear.

The MVP (phase 1) covers accounts, login, status, pinning, labels, attach and manual moves. Limit tracking, balancing, failover, remote boxes and session transfer follow as separate phases.

## Goals and non-goals

Goals: the user's list. Seamless login from twr, all four routing modes, clear display, safe handling of credentials, and an honest position on provider terms.

Non-goals:

- twr does not implement any provider's OAuth flow, store tokens, or use `claude setup-token` / `CLAUDE_CODE_OAUTH_TOKEN`. Sign-in always runs through the runtime's own CLI.
- twr does not create provider accounts or manage billing.
- No account support for `pty:` threads. A generic ACP agent gets accounts only if its preset names the environment variable that selects its config directory.
- No automatic "move back" after a limit resets (see [Failover](#failover)).

## What the runtimes do

### Claude Code

`CLAUDE_CONFIG_DIR` moves everything that lives in `~/.claude`: `settings.json`, `CLAUDE.md`, `agents/`, `commands/`, `skills/`, `plugins/`, `hooks/`, `projects/<cwd-key>/*.jsonl` session transcripts and the auto-memory directories `projects/<cwd-key>/memory/`. The env-vars docs name "running multiple accounts side by side" as a use for it.

Details checked in the 2.1.291 binary and the docs:

- `.claude.json` moves too, but it changes place. Without the variable it is `~/.claude.json`, beside the directory. With it, it is `$CLAUDE_CONFIG_DIR/.claude.json`, inside. That file holds the account identity, onboarding state, folder trust, per-project state and user-scope MCP servers. So `CLAUDE_CONFIG_DIR=~/.claude` is **not** the same as leaving the variable unset. The native account must be "no variable", never "the variable set to `~/.claude`".
- Credentials: `<dir>/.credentials.json` (0600) on Linux. On macOS the Keychain service name gets a suffix of the first 8 hex digits of `sha256(dir)` whenever the variable is set, so each directory has its own Keychain entry. `CLAUDE_SECURESTORAGE_CONFIG_DIR` overrides the credential location alone, so twr must remove it from an account's environment.
- Precedence: cloud provider, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_API_KEY` (always used under `-p`), `apiKeyHelper`, `CLAUDE_CODE_OAUTH_TOKEN`, a Console profile, then the `/login` OAuth credentials. An `ANTHROPIC_API_KEY` in the hub's environment would override every account's login, so a named account's spawn removes these variables.
- A Console sign-in without an API key lives in `~/.config/anthropic`, outside the config directory. Two Console sign-ins can't be kept apart this way. The design targets subscription (claude.ai) logins.
- Login: `claude auth login [--claudeai | --console | --sso | --email <e>]`, `claude auth logout`, and `claude auth status [--json]`. The JSON has `loggedIn`, `authMethod`, `apiProvider`, `email`, `orgId`, `orgName`, `subscriptionType`, `configDirectory` and `projectsDirectory`. `configDirectory` lets twr confirm it checked the right directory. Logged out, it prints `loggedIn: false`, `authMethod: "none"` and exits 1. `claude auth login` doesn't need a terminal to start: it prints the sign-in URL and then waits on standard input for the pasted code (`Paste code here if prompted >`), so a person still has to finish it.
- Resume: a session is "not found" from another config directory. A JSONL copied into another directory's `projects/<cwd-key>/` is found by `--resume` (tested). Thinking blocks carry signatures that may be organization-bound. When the server rejects one, Claude strips all thinking blocks and retries (`tengu_thinking_signature_strip_retry`, read in the 2.1.294 code). Whether the first turn then succeeds under another account needs a second login to test. The prompt cache is lost either way.
- Limits in `stream-json`: a `rate_limit_event` arrives whenever the limit state changes, with `rate_limit_info.{status: allowed | allowed_warning | rejected, resetsAt, rateLimitType: five_hour | seven_day | seven_day_opus | seven_day_sonnet | seven_day_overage_included | overage, utilization, overageStatus, isUsingOverage, ...}`. It is "always absent for API-key, Bedrock, and Vertex sessions". A limit hit ends the turn with an assistant message `{error: "rate_limit", apiErrorStatus: 429}` whose text is like `You've hit your weekly limit · resets 5am (UTC)`. `resetsAt` is in epoch seconds: Claude's own code compares `resetsAt*1000` with `Date.now()`. The SDK control request `get_usage` returns utilization and reset times per window for a live session.

### Codex

`CODEX_HOME` (default `~/.codex`) holds `auth.json`, `config.toml`, `sessions/YYYY/MM/DD/rollout-*.jsonl`, `session_index.jsonl`, `history.jsonl`, several SQLite stores (`state_5`, `thread_history_1`, `memories_1`, `logs_2`, `goals_1`, `queue_1`), `rules/`, `skills/`, `plugins/`, `hooks.json` and the app-server daemon files.

- `cli_auth_credentials_store` can be `file`, `keyring`, `auto` or `ephemeral`. The keyring key is `cli|<hash of the canonical CODEX_HOME>`, so each home has its own entry either way.
- Login: `codex login` (browser), `--device-auth`, `--with-api-key` (stdin), `codex login status` (prints `Not logged in` and exits 1 in a fresh home), `codex logout`. The app-server also has `account/read` (returns `chatgpt{email, planType}` or `apiKey`), `account/login/start` and `account/logout`.
- Resume: `codex resume` and `thread/resume` look only in the current home. A rollout copied into another home's `sessions/` resumes there, by thread id or by the experimental `path`, without a login (tested). The first turn is another matter. 38 of the 40 most recent local rollouts carry `encrypted_content` reasoning, a resume after switching organization has been reported to fail with "Encrypted content organization_id did not match the target organization", and the 0.160.0 binary has no fallback that strips it. Whether two personal accounts count as different organizations needs a second login to test.
- Limits: `account/rateLimits/updated` notifications (sparse, merge into the last read) and `account/rateLimits/read` give a `RateLimitSnapshot` of `primary` and `secondary` windows (`usedPercent`, `windowDurationMins`, `resetsAt`), `planType`, credits and `rateLimitReachedType` (`rate_limit_reached`, `workspace_{owner,member}_{credits_depleted,usage_limit_reached}`). Its docs say clients must not infer recovery from percentages or reset times, but should use `ordinaryUsageAllowed`. A limit hit is an `error` notification whose `codexErrorInfo` is `usageLimitExceeded`; short-term throttling is `rateLimitExceeded`. Messages read "You've hit your usage limit." and "... for <model>. Switch to another model now".

### Grok Build

`GROK_HOME` overrides `~/.grok`: `auth.json`, `mcp_credentials.json`, `config.toml`, `sessions/<cwd>/<id>/` (SQLite), `memory/`, `skills/`, `logs/`.

- Login: `grok login [--oauth | --device-auth]`, `grok logout`. Tokens expire after 7 days and refresh themselves. `XAI_API_KEY` exists. The README and the user guide disagree on whether it beats the stored login, so a named account removes it.
- grok can run a shared "leader" backend. Its socket is `$GROK_HOME/leader.sock` (tested: a leader started with a scratch `GROK_HOME` listened there), so accounts can't share a leader. twr runs `grok agent --no-leader stdio` anyway.
- ACP `initialize` reports `authMethods` `cached_token` ("Cached token from ~/.grok/auth.json") and `grok.com`.
- Limits: no structured window or percentage. Errors carry kinds like `RateLimited`, `UsagePoolExhausted` and `UsageLimitReached` and messages like "You've reached your free Grok Build usage limit for now" and "You've hit your spending cap." No reset time.
- Status: there is no status command, but `grok models` starts with `You are not authenticated.` when there is no usable login (exit code 0 either way). twr reads that first line.
- `grok mcp add` writes `config.toml` through a symlink, but rewrites the whole file, so comments in it are lost.

### Generic ACP

ACP has `authenticate {methodId}` and the error code `-32000 auth_required`, but no standard login-status call yet (an `auth/status` RFD is in draft) and no rate-limit signal. The usage RFD says rate limits "could be addressed in a future RFD". Each agent picks its config directory its own way (for example `GEMINI_CLI_HOME` for Gemini CLI, unverified), so a preset has to name the variable.

## What twr does today

Findings from the code, with the places the design touches:

- **Presets are ACP aliases only.** `RuntimePreset { acp, attach }` (`crates/twr-core/src/config.rs:267-316`) always resolves to the generic `acp` adapter in `hub/runtime_spec.rs:121`. It can't wrap `claude-code`, `codex` or `grok-build`, and the hub resolves it when the thread is created (`RuntimeSpec::record`, `runtime_spec.rs:70`).
- **The environment is inherited.** `process::SCRUBBED_ENV` (`crates/twr-adapters/src/process.rs:11`) removes only twr's two tokens. No adapter sets `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `GROK_HOME`.
- **`SpawnConfig.env` reaches every runtime, local and remote.** The hub fills it in `start_runtime` (`crates/twr-hub/src/hub.rs:1584`). `WireSpawnConfig` (`crates/twr-core/src/runner.rs:698`) carries it to a runner unchanged.
- **Attach drops the environment.** `retain_attach_env` (`crates/twr-adapters/src/lib.rs:42`) keeps only `TWR_ACP_ATTACH`, and the Claude, Codex and ACP `attach_command`s don't apply `cfg.env`. An attach on a non-native account would resume in the wrong directory.
- **Session ids live only in the event log.** Nothing reads runtime session files or detects "session not found".
- **A runtime move is already a new session plus a handoff.** `thread.model_changed` with `runtime`/`previous_runtime` (`crates/twr-core/src/event.rs:576`), `Core::runtime_switch` (`hub/switch.rs:81`) and the handoff built from the log (`Core::thread_handoff`, `switch.rs:159`). The closest existing case to an account move is a move between two ACP agents of the same `acp` runtime (`restart.rs:398` test), which is already a full move.
- **The memory guard follows the config directory by itself.** `memory_dirs` (`crates/twr-adapters/src/claude_code/guard.rs:355`) derives the memory directory from the hook's `transcript_path`, and `check` resolves both the target and the memory directory through symlinks. Only `PolicySources::system()` (`guard.rs:117`) reads `CLAUDE_CONFIG_DIR` from the **hub's** environment, once at start (`hub.rs:305`), to look for `remote-settings.json`. That has to become per account.
- **Limits are folded into "transient".** Claude's `rate_limit_event` only sets a transient hint on `rejected` and ignores `resetsAt` (`claude_code/translate.rs:604`). Codex ignores `account/rateLimits/updated` (`codex/translate.rs:1995`) and does not count `usageLimitExceeded` as transient. ACP and Grok match text (`acp/translate.rs:2671`). The `block_kind` rule (`crates/twr-decisions/src/code/mod.rs:69`) then gives one automatic retry for transient failures (`hub/retry.rs`). A Claude usage limit today burns that retry for nothing.
- **There is no notion of account anywhere.** Login failures get hints such as "(sign in again: run `claude`, then /login)" (`claude_code/translate.rs:1629`), and `docs/configuration.md` says `TWR_HOME` instances share agent logins.
- **Every client reads `ThreadView`.** It is defined at `crates/twr-core/src/reduce.rs:121` and serialized by `thread_json` (`hub.rs:1105`), so a field added there reaches `thread.list`, `thread.get`, MCP and `--json` at once. The hand-written views are the TUI (`status_lines`, `runtime_box`, `main_title` in `crates/twr-tui/src/ui.rs`), the CLI table (`thread_list`, `crates/twr-cli/src/main.rs:3071`), the event pretty-printer (`pretty.rs`), the MCP event renderer (`crates/twr-mcp/src/render.rs`) and the handoff thread lines (`hub/handoff.rs:470`).

## Decision: an account is a layer on a runtime, not a preset

I considered three shapes.

1. **Accounts as presets** (`[runtimes.claude-work] base = "claude-code", env = {...}`). It's the smallest change, but every account becomes a separate runtime. A move from `claude-work` to `claude-personal` would count as a runtime change: model and effort validation run against the "new" runtime, pickers list one row per account, and "same runtime" checks stop working. It also ties the label to the runtime name, so it can't disappear when only one account is logged in. Balancing across "runtimes" makes no sense either.
2. **A first-class concept unrelated to runtimes.** It is overbuilt. Accounts mean nothing outside a runtime.
3. **A layer: a thread runs on `(runtime, account)`.** The runtime keeps deciding the adapter, models, efforts and modes. The account only decides the config directory, and with it the login, sessions and per-account state. **This is the choice.**

So a preset stays what it is (an ACP alias) and gains an optional `account_env` to take part. The runtime keeps its name everywhere, and the account is a separate field that the UI shows next to it when it matters.

## Concepts and names

- **Account**: a named login of one runtime, `(runtime, name)`. Its directory is either the runtime's own default (the native account) or a directory twr sets through the runtime's variable.
- **Native account**: the runtime's default directory, with no variable set (so `CLAUDE_CONFIG_DIR` unset, `~/.claude.json` where it is today). It always exists. It is named `default` until the user names it. All existing threads are on it.
- **Account name**: 1 to 64 letters, digits, `-`, `_` or `.`, unique per runtime. That is the same charset as preset names, so neither contains `:` or `@`.
- **Account reference**: `<runtime>:<account>`, such as `claude-code:work` or `gemini:team`. It works wherever a runtime is accepted (`--runtime`, `twr orchestrator use`, `/model --runtime`, `/orch`). `runtime_spec::resolve` already takes `acp:` and `pty:` as prefixes first. Any other `X:Y` where `X` is a built-in runtime or a preset is an account reference, which no existing value can be, because preset names can't contain `:`. Runtime aliases `claude` and `grok` (for `claude-code` and `grok-build`) are accepted by `twr account` commands, and I suggest accepting them everywhere.
- **Label**: how the UI writes a thread's runtime and account, `claude-code:work`, or `claude-code:work@gpu` on a box (today `runtime@box`).

### When labels show

The user's rule, made precise:

> A runtime **shows account labels** while it has two or more accounts that are registered (not removed) and logged in. Otherwise no thread on that runtime shows an account anywhere.

- It is decided per runtime. Two Claude accounts and one Codex account means Claude threads show `claude-code:work` and Codex threads show `codex`.
- "Logged in" is the last recorded status (see [Status checks](#status-checks)). An account whose status hasn't been checked yet counts as logged in if it was the last time, and a new account counts as logged in once its login succeeds. The status is in the event log, so a hub restart doesn't make labels flicker.
- Remote boxes count by name: `claude-code:work` logged in on `local` and on `gpu` counts once.
- The decision is a pure function of the account reducer: `AccountsView::shows_labels(runtime) -> bool`. Every surface asks it, so they never disagree.

Some things name the account even when labels are hidden, because they are about the account:

- a line in a transcript or event log that records an account **change** (`account work → personal`);
- an attention card about an account (limited, logged out, removed);
- a thread pinned to an account that was removed (`claude-code:work (removed)`), which can't start until it moves;
- `twr account list` and `status`, and MCP `list_accounts`.

**When accounts change.** `account.added`, `account.removed` and `account.status` are control-stream events. Clients fold them into the same `AccountsView` and redraw, so labels appear on every claude-code card, tile and header the moment a second login succeeds, and disappear when one is removed or logs out. Nothing about the threads is rewritten. Every thread event records the account as it was (see [Events](#events)), so `twr events`, transcripts and handoffs keep which account ran each session regardless of what the UI shows today.

## Config

Accounts come from two places (see [Two sources](#two-sources-configtoml-and-twrs-records)): `[accounts.<runtime>.<name>]` tables in `config.toml`, and the ones `twr account add` records in the event log. The config file also holds the pins and the policy, which the hub reads at start like every other key. All keys are optional.

```toml
[accounts.claude-code.work]       # an account declared in config (dotfiles); `twr account add` records one instead
dir = "~/.claude-work"            # its config directory; unset = <data dir>/accounts/claude-code/work
# share = "config"                # none | config | all (see "Shared and per-account state")

[accounts.claude-code.personal]
native = true                     # names the native account (the runtime's own default directory, no variable set)

[accounts.codex.team-b]
# sessions = "shared"             # codex only, opt-in, experimental (phase 6): its sessions live in the native
                                  # home's store; only for accounts of one organization (see "Shared Codex sessions")

[accounts.gemini.team]            # a [runtimes] preset with account_env takes accounts too
dir = "~/.gemini-team"

[orchestrator]
# account = "personal"            # the first orchestrator's account, or a pool ("max-*", ["a", "b"]);
                                  # unset = the policy's default

[projects.acme-api]
repo = "/home/me/code/acme/api"
account = "work"                  # on every runtime: a name, a glob ("work-*") or a list of them (a pool)
# accounts = { codex = "team", grok-build = ["g1", "g2"] }   # per runtime; replaces `account` there
# exclude_accounts = ["personal"] # never these, on any runtime; globs too

[projects.blog]
repo = "/home/me/code/blog"
exclude_accounts = ["work*"]      # no pin, only a rule: never a work account

[boxes.gpu.projects.api]
repo = "/srv/code/api"
# account = "work"                # the same keys work per box project

[account_policy.claude-code]      # one table per runtime or preset; unset = every default below
# default = "personal"            # account for threads nothing pins; unset = the native account
balance = "pinned"                # pinned | round-robin | least-busy | most-headroom
# pool = ["max-*"]                # accounts balancing and failover may pick (globs too);
                                  # unset = every account not paused
failover = "off"                  # off | auto; see "Failover" and the terms caveat before turning it on
avoid_above = 90                  # balancing skips an account at or above this % of any window
# move = "handoff"                # handoff | resume (experimental, phase 6)
# resume_at_reset = false         # true: a turn that hit a usage limit is retried at its reset without asking
                                  # (phase 2; one account is enough; see "Retry at reset")

[runtimes.gemini]
acp = "gemini --experimental-acp"
account_env = "GEMINI_CLI_HOME"   # optional: the variable that selects the agent's config directory
# login = "gemini"                # optional: the interactive login command, run with account_env set
# logout = "..."                  # optional
# status = "..."                  # optional: exits 0 when signed in
```

The `[account_policy]` name keeps away from `route`, which is the decision-ladder site for unaddressed messages.

### Pins

Pins should fit how each person splits their accounts, so every form below works and they combine.

- **What a pin names.** An account name (`"work"`), a glob (`"work-*"`; `*` and `?` only, which account names can't contain), or a list of them. A list or a glob that matches several accounts is a pool: balancing and failover choose inside it, in list order for `pinned`.
- **Where a pin goes.** `account` applies to every runtime. `accounts = { <rt> = ... }` applies to one runtime and replaces `account` there. A box project's keys replace the project's for threads on that box. `[orchestrator] account` takes the same forms. On the command line, `--account` takes a name, a glob or a comma-separated list (`--account work-a,work-b`), and so do MCP `spawn_thread {account}` and `/new --account`. A thread created with a pool keeps that pool as its own pin, for failover.
- **Exclusions.** `exclude_accounts` names accounts a project's threads never use, on any runtime, whether or not there's a pin. That's often what "work code" really means: anything but `personal`. Exclusions apply after the pin, so `account = "*"` plus `exclude_accounts = ["personal"]` is a valid pool.
- **When a runtime has no matching account, the thread is refused** (decided 2026-10-20). A pin is a promise about where the project's code goes, so twr never guesses. The error names both ways out: `project acme-api pins account "work", which codex doesn't have: add it (twr account add codex work), or change the pin ([projects.acme-api] accounts.codex = "<account>")`. The same goes for a pin whose accounts are all excluded, removed or logged out: `project acme-api pins account "work" on claude-code, which is logged out: twr account login claude-code work`. A thread whose project has no pin for its runtime uses the runtime's default. (An earlier draft had an `unmatched_account = refuse | single | any` setting; it's dropped.)
- **Who can leave a pin.** Balancing, failover and the orchestrator never pick an account outside a thread's pin or inside its exclusions. The user can, after a `y` confirmation that names the pin (`acme-api pins account work: move api to personal anyway?`). The thread then records the override (`ext.twr.account_reason: "override"`), and the pin applies again to the project's next threads.
- `[orchestrator] account` picks the first orchestrator's account. After `twr orchestrator use claude-code:work`, the log keeps the choice, as it keeps the runtime today.


### Two sources: `config.toml` and twr's records

Both are supported (decided 2026-10-20). Config declarations suit dotfiles: the same names and directories on every machine, with only the login done per machine. Records suit `twr account add`, which takes effect at once.

- **Config** (`[accounts.<runtime>.<name>]`) is read when the hub starts, like every other key; changing it needs a hub restart. An unknown key, a bad name, a runtime that can't take accounts, a relative `dir`, two accounts with one directory, a `dir` that is the runtime's default directory (that must be `native = true`, because setting `CLAUDE_CONFIG_DIR=~/.claude` is not the same as leaving it unset), or two native accounts for one runtime stop the hub from starting, with the key named.
- **Records** are `account.added` and `account.removed` events in the log, written by `twr account add` and `remove`. They take effect at once and survive restarts.
- **Precedence: config wins.** An account in both is the config's: its directory, share and native flag come from `config.toml`, and the record is ignored. `twr account list` shows a SOURCE column: `config`, `twr` (recorded), or `config (also recorded)`, and `status` adds `the recorded directory X is ignored: config.toml says Y` when they differ. A config native name also beats a recorded one.
- **The account commands never write `config.toml`** (the TUI's settings view, which came later, writes only the one setting changed there, never an account). `twr account remove` of a config account is refused: `claude-code:work is declared in config.toml ([accounts.claude-code.work]); remove it there and restart the hub`. If it's also recorded, the record is removed and the message says the config account stays. `twr account add` of a name config already declares doesn't record anything: it runs the login in the config's directory, the same as `twr account login`.
- **What either source keeps:** runtime, name, directory, share and the native flag. No email, organization or plan, the same rule `dev`'s agent probes follow (`crates/twr-adapters/src/agents.rs`). Login state is kept as `logged in` / `logged out` / `unknown` plus the method (`claude.ai`, `console`, `api-key`, ...).

## Data model

### `twr-core`

A new module `crates/twr-core/src/account.rs`:

```rust
pub struct AccountRef { pub runtime: String, pub name: String }   // parses "claude-code:work"

pub enum AccountDir { Native, Path(PathBuf) }

pub enum Share { None, Config, All }

pub enum LoginState { LoggedIn, LoggedOut, Unknown, Error(String) }

pub struct Limit {
    pub kind: LimitKind,            // Usage | Spend | Rate
    pub scope: LimitScope,          // Account | Model(String)
    pub window: Option<String>,     // "five_hour", "seven_day", "primary", ...
    pub resets_at: Option<Ts>,      // None: unknown, use the cooldown
    pub message: String,            // the runtime's own words, for cards
    pub thread: Option<ThreadId>,   // where it was seen
}

pub struct UsageWindow { pub window: String, pub used_pct: f32, pub resets_at: Option<Ts> }

pub struct AccountState {
    pub runtime: String, pub name: String, pub boxes: BTreeMap<String, AccountDir>,
    pub share: Share, pub source: Source,              // Config | Recorded | Both (config wins)
    pub login: LoginState, pub method: Option<String>, pub checked_at: Option<Ts>,
    pub paused: bool, pub removed: bool,
    pub limit: Option<Limit>, pub usage: Vec<UsageWindow>,
}

pub struct AccountsView { /* (runtime, name) -> AccountState, plus native naming */ }
impl AccountsView {
    pub fn shows_labels(&self, runtime: &str) -> bool;
    pub fn label(&self, runtime: &str, account: Option<&str>, box_: Option<&str>) -> String;
    pub fn eligible(&self, runtime: &str, now: Ts) -> impl Iterator<Item = &AccountState>;
}
```

`AccountsView` is a reducer over the control stream, like the attention stack. Replaying the log rebuilds it.

`ThreadView` (`reduce.rs:121`) gains `account: Option<String>`. `None` is the native account under whatever name it has now. That's the value every thread has before this change, so old logs need no migration. The thread JSON (`thread_json`, `hub.rs:1105`) gains:

- `account`: the account's current name, always present (the native name when `None`);
- `account_shown`: `shows_labels(runtime)`, so scripts can mirror the UI;
- `account_removed: true` only when the account was removed.

`ThreadModelChanged` (`event.rs:576`) gains `account` and `previous_account`, both optional. `ChangedBy` (`event.rs:534`) gains `Failover` (and `Balancer` is not needed, since balancing only picks at creation). A new restart reason `ACCOUNT_RESTART_REASON = "account_change"` sits next to `RUNTIME_RESTART_REASON` (`event.rs:628`).

### Events

New control-stream kinds:

| Kind | Data | When |
|---|---|---|
| `account.added` | `runtime, account, box, dir` (null for native), `share, native: bool, by` | `twr account add` succeeded, or the native account got its name |
| `account.removed` | `runtime, account, box, moved: [{thread, to}], by` | `twr account remove` |
| `account.updated` | `runtime, account, paused?, share?` | `pause`, `resume`, `relink` |
| `account.status` | `runtime, account, box, login, method?, note?` | a status check whose result differs from the last one (no email, organization or plan) |
| `account.limit` | `runtime, account, limited: bool, limit?` | a limit seen, or cleared at reset / by a successful turn |
| `account.usage` | `runtime, account, windows, source, plan, thread, observed_at` | already on `dev` (`event.rs:1405`, `hub/usage.rs`), with `account` always absent; phase 1 fills it from the thread's account (`hub/usage.rs:61`) |

Additions to existing events:

- `thread.created` `ext.twr.account` (absent for native) and `ext.twr.account_reason` (`explicit | project | orchestrator | default | round-robin | least-busy | most-headroom`).
- `thread.started` `ext.twr.account`, stamped by `RunSettings` (`restart.rs:207`). Every session start records which account ran it, so the history holds even after a rename of the native account or a removal.
- `thread.model_changed` `account`, `previous_account`, and with `by: failover` also `ext.twr.limit`.
- `thread.stopped` `ext.twr.reason: "account_change"`.
- `turn.ended` `ext.twr.limit`, set by the adapter when the turn failed on a limit (see [Detection](#detection)).
- A new thread-stream kind `runtime.limits {windows, status, source}`, which adapters send when the runtime reports its limit state mid-session (Claude's `rate_limit_event`, Codex's `account/rateLimits/updated`). Adapters don't know about accounts. The hub maps the thread to its account and folds this into `account.usage`/`account.limit`.
- `decision.made {site: "account"}` for every balancing and failover choice, so the `d` audit view shows who picked which account and why.

Older readers keep working on newer logs: unknown kinds become `EventBody::Unknown` (`event.rs:11`), and payload structs keep unknown keys. `ChangedBy::Failover` is a new enum value, though, so an older twr would fail to decode a `thread.model_changed` that carries it. The fallback is to write `by: "user"` plus `ext.twr.by: "failover"`. Which way to go is decided in phase 4 after checking how the decoder handles it.

## Login, status and removal

### CLI

```
twr account list [--json]
twr account add <runtime> <name> [--dir D] [--share none|config|all] [--device] [--native-name N] [--box B] [-- <login args>]
twr account login <runtime> <name> [--device]      # sign in again after an expiry or logout
twr account status [<runtime>[:<name>]] [--usage] [--json]
twr account remove <runtime> <name> [--move-to <name>] [--logout] [--purge]
twr account pause|resume <runtime> <name>          # leave out of balancing and failover, keep pins working
twr account relink <runtime> <name>                # repair or extend the shared links
twr account name-native <runtime> <name>           # name the native account without adding another
```

A thread's account is set with the existing commands:

```
twr thread new api --account work                  # or --runtime claude-code:work
twr model api --account personal [--now]           # a move; --runtime codex:team moves runtime and account
twr orchestrator use claude-code:work [--no-summary]
```

`Cmd::Account` follows the `twr box` pattern (`crates/twr-cli/src/boxes.rs`: a `BoxCmd` enum and `boxes::run`, wired at `main.rs:424` and `:1531`), in a new `crates/twr-cli/src/accounts.rs`. `add`, `login` and `remove` are refused to agents (`refuse_agent_action`, `main.rs:6371`), as `twr box login` is.

### `twr account add`, step by step

The CLI does the parts that need the user's terminal. The hub does the bookkeeping.

1. Check the name and the runtime. The runtime must be `claude-code`, `codex`, `grok-build` or a preset with `account_env`.
2. If this is the runtime's first added account, check the native login with the runtime's status command. If it's logged in, ask what to call it (`Your current Claude Code login stays usable as an account. Name it [default]:`). `--native-name` answers without asking, which non-interactive use needs. The answer becomes `account.added {native: true}`.
3. Create the directory, by default `<data>/accounts/<runtime>/<name>` (`~/.local/share/twr/accounts/claude-code/work`), mode 0700. `--dir` uses another path: an empty one, or an existing config directory that is already logged in, which is then adopted as it is. twr refuses a directory other users can read or write.
4. Create the shared links (see [Shared and per-account state](#shared-and-per-account-state)).
5. Run the login in the foreground, in the user's terminal, with only the account's variable added: `CLAUDE_CONFIG_DIR=<dir> claude auth login`, `CODEX_HOME=<dir> codex login [--device-auth]`, `GROK_HOME=<dir> grok login [--device-auth]`, or the preset's `login`. Arguments after `--` go to the login command (for `--console` or `--sso`). twr doesn't capture the output. The browser, the URL and any pasted code go between the user and the runtime only.
6. Check the status in that directory and show what it found: `work: logged in (claude.ai)`. twr keeps no email, organization or plan; the runtime's own login output, which shows them, goes only to the user's terminal.
7. Send `account.add` to the hub (starting it if needed). The hub runs its own status check and records `account.added`. (Telling two accounts that are the same login apart would need an identity, which twr doesn't keep; phase 3 may compare a salted hash.)
8. Print the effect on the UI: `claude-code now has 2 logged-in accounts (personal, work), so its threads show their account.`

When a step fails, the CLI stops, keeps the directory and prints the command to finish by hand. `twr account login` repeats steps 5 to 7.

**TUI.** The TUI can't hand its terminal to another program (`docs/tui.md`), so `/account add claude work` does what `/attach` does: it shows the command to run in another terminal. `/accounts` shows the list. A later phase may run device-code logins (Codex, Grok) from the TUI, because those only show a URL and a short user code that the user types on the provider's site. No secret passes through twr in that flow. Claude's flow needs a pasted authorization code, so it always stays in the user's terminal (see [Terms of service](#terms-of-service)).

### Status checks

The status command for each runtime, always run with the account's environment. Phase 1 reuses `dev`'s agent probe (`twr_adapters::agents`, which `twr box doctor` and `box.agents` use) with the account's variables added to the probe's commands:

| Runtime | Command | What twr reads |
|---|---|---|
| Claude Code | `claude auth status --json` (exit 1 when logged out) | `loggedIn` and `authMethod`, as `dev`'s agent probe already does (`agents.rs`, `claude_login`) |
| Codex | `codex login status` | the exit code (0 logged in), as the agent probe does |
| Grok Build | none yet: `unknown`, as the agent probe says | phase 2 may read the first line of `grok models` (`You are not authenticated.` means logged out) |
| ACP preset | `status`, if given | exit code 0. Without one, `Unknown` |

When the hub checks:

- when it starts, in the background, so a slow check never holds anything up;
- every 10 minutes, with the agent probes;
- right after a turn fails with `auth_failed` on that account;
- when `twr account status` asks.

A result is recorded only when it differs from the last one.

Claude Code reporting `authMethod` other than the account's login (for example an `ANTHROPIC_API_KEY` left in the environment) is shown as a warning: `work: Claude Code uses an API key from the environment, not this account's login`.

The existing auth hints change for named accounts. "(sign in again: run `claude`, then /login)" becomes "(sign in again: `twr account login claude-code work`)", and the error card names the account whatever the label rule says.

### `twr account list`

```
RUNTIME      ACCOUNT   SOURCE   STATUS               THREADS  DIR
claude-code  personal  config   logged in (native)   3        ~/.claude
claude-code  work      twr      logged in (claude.ai) 2       ~/.local/share/twr/accounts/claude-code/work
codex        default   -        logged in (native)   1        ~/.codex
```

The plan and usage windows are in `twr usage`, which already labels each runtime's accounts once it has several.

### Removal

`twr account remove claude-code work`:

- With live or parked threads on the account, it lists them and needs `--move-to <name>` (a move per thread, with a handoff) or `--force` (the threads keep the account, and each refuses to start until moved: `api runs on account work, which was removed: twr model api --account <name>`).
- The orchestrator moves only by a switch. When it runs on the account, `--move-to` refuses the whole removal before anything moves: `the orchestrator runs on claude-code:work: switch it first (twr orchestrator use claude-code:personal), then remove the account`.
- `--logout` runs the runtime's logout in the directory first. `--purge` deletes the directory after asking `y`. Without either, the login stays on disk and `twr account add --dir` can adopt it again.
- The native account can be removed too. It is then out of the label count and of balancing, and threads can't be created on it. Its directory is never touched. It keeps its name while removed, so it never takes the name of a named account (a named `default` stays itself), and `twr account name-native <runtime> <name>` brings it back.

## Shared and per-account state

Accounts should behave the same: same settings, same permission rules, same `CLAUDE.md`, same agents and skills. twr gets that with symbolic links from the account's directory to the native one. The link source is the native directory, because that's where the user's setup already is.

`--share` picks the set:

- `none`: a bare directory. The runtime starts from scratch.
- `config` (default): settings and instructions, as in the tables below.
- `all`: `config` plus session transcripts, memory and prompt history. Accounts then remember the same things and can see each other's sessions, which the experimental resume move relies on.

Codex sessions are a separate opt-in on top of these (`sessions = "shared"`, below), because no `share` level is safe for them across organizations.

`config` is the default because it keeps separate purposes apart. A work account and a personal one shouldn't read each other's project memory and transcripts unless the user asks for that. Two accounts that only exist to split load can use `all`.

### Claude Code

| Item | `config` | `all` | Why |
|---|---|---|---|
| `.credentials.json`, the Keychain entry | own | own | the login |
| `.claude.json` | own | own | holds the identity next to MCP servers and folder trust, so it can't be linked. Phase 2 copies the `mcpServers` key into a new account once, through Claude Code's own `claude mcp add-json` per server, rather than writing the file itself; until then a new account starts without the native user-scope MCP servers. |
| `remote-settings.json` | own | own | the organization's server-managed policy |
| `settings.json`, `CLAUDE.md`, `agents/`, `commands/`, `skills/`, `output-styles/`, `hooks/`, `plugins/` | linked | linked | the user's setup |
| the orchestrator's memory directory | shared | shared | what the orchestrator learned about the user, kept across an account switch without sharing project memory. Shared through `autoMemoryDirectory`, not a link (see below) |
| every thread's memory directory | own | shared | per-project memory, through `autoMemoryDirectory` |
| `projects/` | own | linked | transcripts |
| `history.jsonl` | own | linked | prompt history |
| `sessions/`, `session-env/`, `shell-snapshots/`, `file-history/`, `statsig/`, `cache/`, `daemon*`, `jobs/`, `remote/`, `state/`, `uploads/`, `backups/`, `stats-cache.json` | own | own | runtime state, some of it tied to the login (`daemon-auth-status.json`) |

Two concurrent sessions of different accounts writing into a linked `projects/` behave like two sessions of one account do today: each writes its own `<session>.jsonl`. Transcripts are written through the link normally (tested).

**Memory is shared through `autoMemoryDirectory`, never through a link.** A symlink anywhere in the memory path makes Claude Code stop treating it as its memory directory. In `dontAsk` mode it then refuses the write ("Permission to use Write has been denied because Claude Code is running in don't ask mode"). That happened both with `projects/<key>/memory` linked and with `projects/<key>` linked, with and without twr's guard hook (tested; see [Phase 0 results](#phase-0-results)). In other modes, memory writes through a link would count as ordinary file writes and could ask. Claude Code's `autoMemoryDirectory` setting moves the memory directory to a real path, and `dontAsk` allows writes there (tested). So for a named account whose sharing covers memory, the hub adds `autoMemoryDirectory` to the `--settings` it passes, pointing at the native account's real directory for that project: `<native config dir>/projects/<key>/memory`, where `<key>` is the key Claude Code would use (the git main checkout's path, else the working directory, sanitized as `guard::sanitize` does).

- The orchestrator gets this with `config` and `all`. Worker threads get it with `all` only, so `config` keeps project memory per account.
- Workers get a `--settings` they don't get today. It holds only this key.
- A key longer than 200 characters gets a hash in Claude Code that twr doesn't compute, so that project's memory stays per account, and twr logs why.
- If the account's own `settings.json` (normally a link to the native one) already sets `autoMemoryDirectory`, that account already uses that directory, and twr adds nothing. Claude Code reads only the account's settings, so a native `settings.json` the account doesn't link to doesn't count.
- The directory twr names is the real one, with links on the way to it followed (a link to the native config dir, say), because `dontAsk` refuses memory writes through a link.
- The native account gets no override and behaves as today.
- The memory guard learns the directory from the hook command: `twr claude-memory-guard --memory-dir <dir>`. It then allows exactly that directory instead of deriving one from the transcript path. Today the guard denies a directory moved with `autoMemoryDirectory` (`docs/orchestrator.md`), and that stays true for any directory twr didn't set itself.

Claude Code's Write and Edit tools refuse a target that is itself a symlink (`Refusing to write through symlink`) but don't check parent directories (read in the 2.1.294 code). Writes through a linked directory work at the file level. It's the memory classification that breaks.

**Linked files survive the runtimes' own writes.** All three runtimes write a linked config file through the link (tested): Claude Code resolves a linked user `settings.json` on purpose (`allowSymlink: true` for user settings; its debug log says `Writing through symlink`) and then writes the target atomically, and Codex's `codex mcp add` and Grok's `grok mcp add` update the target and leave the link in place. So files are linked, not only directories. An editor that saves by replacing the file, or a manual `cp`, can still break a link, so `twr account status` checks the links and reports `settings.json is no longer linked`, and `twr account relink` offers to keep the account's copy or restore the link.

### Codex

| Item | `config` | `all` |
|---|---|---|
| `auth.json`, keyring entry | own | own |
| `config.toml`, `AGENTS.md`, `rules/`, `skills/`, `prompts/`, `hooks.json`, `plugins/` | linked | linked |
| `sessions/`, `session_index.jsonl`, `state_5.sqlite`, `thread_history_1.sqlite` | own | own (see below) |
| `memories_1.sqlite`, `memories/` | own | linked |
| `history.jsonl` | own | linked |
| `logs_2.sqlite`, `goals_1.sqlite`, `queue_1.sqlite`, `app-server-*`, `installation_id`, `models_cache.json` | own | own |

Codex's sessions stay per account even with `all`. Its rollouts carry organization-bound encrypted reasoning, and its session index lives in SQLite next to state. Codex's memories are different: they're the point of `all`. Codex builds them in a background pipeline when a session starts: it extracts memories from idle past sessions with a model call, records them and leased jobs in `memories_1.sqlite`, and consolidates them into files under `memories/` (`memory_summary.md`, `rollout_summaries/`, ...). Both are linked in `all`. The job table already has worker ids, ownership tokens and leases, because several Codex processes on one home share it. The pipeline runs only with a ChatGPT login and with `[features] memories = true`, which is off by default (tested: a home without a login records no jobs). SQLite 3.10 and later resolve a symbolic link before naming the `-wal` and `-shm` files. Codex 0.160.0 bundles a SQLite built on 2026-03-13 and never opens with no-follow. In a test, two app-servers ran at once on homes sharing one linked `memories_1.sqlite`: neither failed, and no `-wal` or `-shm` file appeared beside the link, so both used the target's write-ahead log. Two homes running the pipeline at the same moment need logged-in sessions to test. If that ever fails, the database stays per account and `all` says so.

#### Shared Codex sessions (opt-in, experimental)

T3 Code (`pingdotgg/t3code` at `1c8e79e`, `apps/server/src/provider/Drivers/CodexHomeLayout.ts`) runs a second Codex login as a "shadow home" over the shared `~/.codex`. It keeps only `auth.json` and `models_cache.json` private and `log/`, `memories/` and `tmp/` local, and links every other entry of the shared home, including `sessions/`, `archived_sessions/`, `session_index.jsonl`, the state and thread-history SQLite files, `config.toml`, `history.jsonl` and `installation_id`. Every shadow home of one shared home gets one continuation key (`codex:home:<shared home>`), and T3's session transition policy (`orchestration-v2/ProviderSessionTransitionPolicy.ts`) restarts and resumes the same Codex thread when a thread moves between instances with equal keys. A Claude instance's key is its own config directory, so a Claude account move in T3 is always a new session with a handoff.

That is evidence that a shared session store works for T3's users in the common case, typically several personal ChatGPT accounts. It is not evidence that resuming across organizations works: T3's server and provider code have no handling of an encrypted-reasoning or organization mismatch (the only `encrypted_content` matches are the generated app-server schema), and the failure that keeps twr's Codex sessions per account (see [Session continuity](#session-continuity)) is an organization mismatch. So twr adds the store as its own opt-in and does not change `all`:

- `[accounts.codex.<name>] sessions = "shared"` (or `twr account add codex <name> --sessions shared`) links `sessions/`, `archived_sessions/`, `session_index.jsonl`, `state_5.sqlite` and `thread_history_1.sqlite` to the native home's real files. The default stays `own`. Only accounts of one person or one organization should use it: a turn written under one organization leaves reasoning the other can't read, in both directions. twr can't check the organization itself (it keeps no organization for an account, per [Decisions](#decisions)); whether `account/read` exposes an organization or workspace id that the hub could compare at link time without storing it is open.
- Login state stays apart whatever the setting: `auth.json` and the keyring entry are never linked, and `models_cache.json` (per plan) and `installation_id` stay per account. T3 links `installation_id`; twr doesn't, so two accounts don't report one installation.
- Memory and prompt history follow `share` as before. `sessions = "shared"` never turns them on.
- **Existing files.** `twr account add` links only what the account doesn't have and never overwrites a file. An account that already has its own sessions or state database is refused for `sessions = "shared"`, with the files named: twr doesn't merge two SQLite stores or move a user's sessions aside. A fresh account directory is the way in.
- **Lock files.** T3 found that Codex creates `mcp-oauth-locks/` and `.sqlite-maintenance.lock` in a new home before the shared one has them, and at each start it deletes such local copies and links the shared ones. twr keeps "never overwrite" instead: it creates the account's links before the account's first process runs (at `twr account add`, before the login), and a lock entry that still exists as a real file or directory is reported by `twr account status` (`.sqlite-maintenance.lock is not linked`) and replaced only by `twr account relink`, and only for those two lock names, while no runtime, job or login runs on the account (checked under the core lock). Data files are never replaced. Whether `codex login` itself creates either lock before the links exist is untested.
- Concurrent use is Codex's normal case: several app-servers on one home already share its state database, and a linked SQLite file uses the target's write-ahead log (the memories test above). Two logged-in homes on one linked state database at once is still a test to run.

### Grok Build

`auth.json` and `mcp_credentials.json` are per account. `config.toml`, `skills/`, `trusted_folders.toml` are linked in `config`. Grok also imports Claude Code's MCP servers and skills: grok 1.0.50 reads `~/.claude.json` and scans `~/.claude/skills` whatever `GROK_HOME` says (traced 2026-10-20), so every Grok account sees the native Claude setup. Grok rewrites `config.toml` whole when it changes it, so comments in the shared file are lost. That happens with one account too. The memory directory is linked in `all`. That is untested for Grok, and Grok has no setting like `autoMemoryDirectory`, so if Grok treats a linked memory directory the way Claude Code does, its memory stays per account. `sessions/` stays per account.

### Presets

A preset with `account_env` gets no links by default. It may list them: `account_share = ["settings.json", "extensions/"]`.

## Spawning and attaching

### Environment

The hub turns a thread's account into environment in the same place it turns a preset into `TWR_ACP_*` today. `append.rs:123` keeps `rt.acp_env` from the log, and `start_runtime` (`hub.rs:1584`) adds it. With accounts:

- **Native account:** nothing changes. The runtime inherits the hub's environment as now, including any `CLAUDE_CONFIG_DIR` the user exported before starting twr.
- **Named account:** set the runtime's variable to the directory and remove the variables that would override the login.

| Runtime | Set | Remove |
|---|---|---|
| Claude Code | `CLAUDE_CONFIG_DIR`, and `ANTHROPIC_CONFIG_DIR=<dir>/anthropic` | `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN`, `CLAUDE_CODE_OAUTH_REFRESH_TOKEN`, `CLAUDE_SECURESTORAGE_CONFIG_DIR`, `ANTHROPIC_PROFILE`, `ANTHROPIC_IDENTITY_TOKEN`, `ANTHROPIC_IDENTITY_TOKEN_FILE`, `ANTHROPIC_FEDERATION_RULE_ID`, `ANTHROPIC_ORGANIZATION_ID`, `ANTHROPIC_SERVICE_ACCOUNT_ID`, `ANTHROPIC_WORKSPACE_ID`, `ANTHROPIC_SCOPE`, `CLAUDE_CODE_FEDERATION_CACHE_DIR`, `CLAUDE_CODE_USE_BEDROCK`, `CLAUDE_CODE_USE_VERTEX` |
| Codex | `CODEX_HOME` | `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN` |
| Grok Build | `GROK_HOME` | `XAI_API_KEY`, `GROK_AUTH_PROVIDER_COMMAND` |
| ACP preset | `account_env` | nothing, unless the preset lists `account_scrub` |

Why `ANTHROPIC_CONFIG_DIR`: Claude Code's profile store (Console and workload-identity profiles, which outrank a `/login` login) is `~/.config/anthropic`, and `CLAUDE_CONFIG_DIR` doesn't move it. With `ANTHROPIC_CONFIG_DIR` set per account, `claude auth status` touched no native path at all, and a Console sign-in made through `twr account add -- --console` stays with its account (traced 2026-10-20). Each variable in the Claude list is one Claude Code itself treats as selecting credentials; a stray `ANTHROPIC_API_KEY` made a scratch account report `authMethod: api_key`, and `CLAUDE_CODE_OAUTH_TOKEN` made it `oauth_token`. A stray `XAI_API_KEY` made grok report `You are using XAI_API_KEY.` Codex's app-server ignored all three Codex variables (`account/read` stayed `null`), so those are removed only as a precaution.

The account's variables are applied last, after `[threads] env` and `[projects.<name>] env` (`config.thread_env`, `config.rs:1368`): a project env that sets `CLAUDE_CONFIG_DIR` loses to a named account, and the thread's start event notes it. The native account leaves such a project env alone, which is how someone could fake accounts before this design. The orchestrator gets no thread env today (`hub.rs:2168`), so its account variables are added in `start_runtime`. Jobs a thread starts (`hub/jobs.rs`) take the thread's `spawn_config` env, so a job that runs `claude` uses the thread's account too.

These per-runtime facts live in a new `crates/twr-adapters/src/account.rs` as an `AccountSupport` value per adapter: the variable, the removal list, the login, logout and status argv, and a status parser. The hub reaches it through a new `Adapter::account_support()` trait method (`crates/twr-core/src/adapter.rs:339`), so the runner can use the same code for remote boxes. `process::scrub` (`process.rs:14`) takes an extra removal list.

Setting the variable through `SpawnConfig.env` reaches a remote runner as it is (`WireSpawnConfig`). Remote accounts still wait for phase 5, because the directory must exist and be logged in on the box; phase 1 refuses a named account on any box but `local`, with `accounts on remote boxes aren't supported yet: use the box's own login (twr box login <box> <agent>)`. Note for phase 5: a Mac box daemon passes only an allowlist of variables (`relay/launchd.rs`, `ENV_NAMES`), but the account variables travel in `SpawnConfig.env`, not in the daemon's environment, so the allowlist doesn't matter for them.

### Attach

`twr attach` must run the interactive CLI in the same account, or it would resume a session id that doesn't exist in the native directory, or open it on the wrong login. Phase 1 changes:

- On `dev` the attach commands already apply the thread's environment: Claude's `interactive` (`claude_code.rs:428`), Codex's `resume_command` (`codex.rs:879`) and ACP's (`acp.rs:658`) through `process::attach_env`, Grok's through `build_env` (`grok_build.rs:428`). So once `attach_config` (`hub/attach.rs:209`) carries the account's variables, attach keeps the account. What's new is the removals: the attach command also drops the variables that would override the login.
- A remote attach builds its command on the hub from the same config (`attach.rs:925`), so phase 5 needs nothing more there.

The first attach of an account in a new worktree shows Claude's folder-trust question again, because trust lives in the per-account `.claude.json`. That's a one-time cost per account and worktree, and the docs will say so.

### The orchestrator and the memory guard

- `orchestrator_choice` (`hub/orch.rs:1243`) and `create_orchestrator` (`:2088`) take the account from the switch request, else `[orchestrator] account`, else the policy default. `twr orchestrator use claude-code:work` is an ordinary orchestrator switch with a handoff. Switching only the account of the same runtime goes through the same path.
- `PolicySources` (`guard.rs:98-152`) is computed per spawn from the account's directory rather than once from the hub's environment (`HubOptions::claude_policy`, `hub.rs:291`). A work organization's `remote-settings.json` then turns the orchestrator's file tools off for that account only. The check is a file-existence test, cheap enough to run at every start.
- `restrict` (`orch.rs:2722`) uses that per-account policy.

### The `--help` probe

The Claude Code capability probe (`claude_code/cli.rs:589`) is cached per binary. It doesn't depend on the login, so it stays shared. It should run with the native environment, never an account's, so it can't touch an account's state.

## Routing

### Which account a new thread gets

`thread_runtime` (`hub/threads.rs:537`) picks the runtime. A new `thread_account` right after it picks the account, first match wins:

1. **Explicit**: `--account`, `<runtime>:<account>`, MCP `spawn_thread {account}`, `/new --account`. A name, glob or list, as in [Pins](#pins). It must match an account of the runtime that isn't removed. A paused account is allowed, since the user named it. A user's choice outside the project's pin or inside its exclusions needs the confirmation described there. The orchestrator's is refused.
2. **Box project or project pin**: one account, or a pool to balance within. A pin nothing matches refuses the thread (see [Pins](#pins)).
3. **Orchestrator**: for the orchestrator itself, `[orchestrator] account`.
4. **Policy**: `[account_policy.<rt>]`, with `balance` choosing from `pool` (or from every account that isn't paused).
5. **Native**.

The project's `exclude_accounts` apply at every step. A step whose candidates are all excluded falls through to the next, and if nothing is left the thread is refused with the list of what was excluded.

The choice is recorded as `thread.created ext.twr.account` and `account_reason`, plus a `decision.made {site: "account"}` when a balancer chose. After that the thread keeps its account. Resumes, restarts and retries never re-balance. Only a move changes it.

The orchestrator may pass `account` only when the user asked for it, like `runtime` today. It is refused when the account is outside the project's pin or inside its exclusions: `project acme-api pins account work; only you can move its threads elsewhere`. The user may do it from the CLI or TUI after a `y` confirmation that names the pin.

### Balancing algorithms

All of them choose among **eligible** accounts: registered, not removed, logged in or `Unknown`, not limited (or the limit's reset has passed), not paused, inside the pin or pool, and not excluded. If none is eligible, the thread is created on the first account of the pool anyway, and a card says why (`all claude-code accounts are limited; the earliest, work, resets at 15:00`). The thread then waits as described in [Limits](#limits).

| `balance` | Picks | Data it needs |
|---|---|---|
| `pinned` (default) | the first eligible account of the thread's pin in list order; unpinned, `default`, else the native account | none |
| `round-robin` | the next eligible account after the one the previous new thread on this runtime got, in pool order | the log's last `account_reason` |
| `least-busy` | the fewest **active** threads (running, background, waiting, or idle with a live runtime). Ties go to pool order | thread views, always available |
| `most-headroom` | the lowest *pressure*, the highest `used_pct` over the account's known windows, scaled by how far away the window's reset is. A window at 80% that resets in 10 minutes counts as less pressure than one at 60% that resets in 5 days. Accounts with no usage data rank by `least-busy` behind those with data | `account.usage` (phase 2) |

`avoid_above` (default 90) takes an account out of `least-busy` and `most-headroom` while any window is at or above it, and lets it back in once all are 10 points below. That gap is there so a new thread doesn't flip-flop between two accounts near the edge.

Two accounts that are the same login would be one account for balancing, but twr keeps no identity to tell (see [Two sources](#two-sources-configtoml-and-twrs-records)); phase 3 decides whether a salted hash of the identity is worth keeping for that.

## Moves

A move changes a thread's account, and with `--runtime` maybe its runtime too. It reuses the runtime-move path:

- `Core::runtime_switch` (`hub/switch.rs:81`) treats `(runtime, account)` as the identity. Its "same runtime and same agent" early return (`switch.rs:104-117`) also compares the account. A move between two accounts of one runtime is a full move: the old runtime stops with `account_change` once it's idle and its background tasks are done, `session_id` is cleared, `handoff_pending` is set, and the next turn starts a new session with the handoff.
- The thread keeps its model and effort, since the runtime didn't change. If the new account can't use the model (a plan without it, `model_not_found`), the turn fails with the runtime's message and the card offers `/model`.
- `thread_handoff` (`switch.rs:159`) gains one line: `Previous account: work (it hit its 5-hour limit, resets 15:00)`. For a same-runtime move, the handoff can be richer than across runtimes. When the old account still works (a manual move, not a limit), `--summary` asks the old session for a summary first, as the orchestrator switch does (`hub/handoff.rs`, 60 s wait). That's phase 3. Moves start without it.
- `--now` stops a running turn instead of waiting for it, after a `y`.

Surfaces: `twr model <thread> --account <a>` (and `--runtime <rt>:<a>`), `/account <thread> <a>` (in a dive `/account <a>`), and MCP `set_model {account}`. The transcript records `account work → personal (by you)`, and with a runtime change `runtime claude-code:work → codex:team`.

### Session continuity

A session can't follow an account by default, so a move is a handoff. I looked for something better:

- **Claude Code.** Transcripts are plain JSONL under `projects/<cwd-key>/`. With `share = all`, both accounts see the same file, so `--resume <same id>` from the other account could pick it up with nothing copied. With `config`, twr could copy the one file into the target's `projects/<key>/`. The lookup works (tested), and Claude strips thinking signatures and retries when the server refuses them. Still open: whether the first turn succeeds under a second account, and the cost. The resumed session's first turn on the new account reads the whole context with no cache, which on a long session costs far more of the new account's limit than the handoff's 24 KiB. So a resume move is not always better. It is `[account_policy.claude-code] move = "resume"`, experimental, phase 6, only after phase 0 shows it works, and with a fallback to the handoff when the resume fails with "session not found" or an API error on the first turn.
- **Codex.** A copied rollout resumes locally (tested), but nearly every rollout carries organization-bound encrypted reasoning that has broken the next turn for at least one user, and Codex has no fallback. Handoff by default. A resume move is possible only between accounts that share a session store and an organization (see [Codex resume moves](#codex-resume-moves)).
- **Grok Build and ACP.** Unknown session formats. Handoff only.

The handoff already carries the first prompt, the worktree and branch, recent reports and the recent turns. The worktree itself never moves: the files the old session wrote are all there.

### Which saved session a thread may resume

`dev` (`cfd1937`, from `twr/t3code-exploration`) has switch-back resume: a thread moved back to a runtime it used resumes its earlier session of that runtime with a delta handoff, and falls back to a new session with the full handoff when the resume fails. On `dev` alone it picks the session by runtime (the latest `thread.model_changed` whose `previous_runtime` is the target, and its `ext.twr.previous_session`). With accounts, a runtime has several session stores, so phase 1, rebased onto it, keys saved sessions by account too (`Core::departure` in `hub/switch.rs`; `ext.twr.session_store` is built for the native account only, shared stores are not):

- A session's **store** is the real path of the directory the runtime keeps it in: the account's directory (Phase 1 already stamps `account` and the resolved `account_dir` on `thread.started`), the native directory for the native account, and the native home for a Codex account with `sessions = "shared"` (the store is stamped on `thread.started` as `ext.twr.session_store`).
- Moving back resumes a saved session only when the target `(runtime, account)` is the one the session ran on, the account's directory still resolves to the stamped one, the account wasn't removed since the session's start, and a named account's runtime is given the directory by the path the session's runtime had (`ext.twr.account_env_dir` of the account's binding then: the move or `thread.created` that set it). A directory change, a removal and a re-add (in the same directory too), another path to the directory, or a different account of the same runtime means a new session with the full handoff, as Phase 1 does for every account move. The session-start lookup counts only runtime-reported `thread.started` / `thread.resumed` (a park's or the user's resume records no account stamps). The turn that carries the resume checks all of it again under the lock it starts in; one that no longer holds records `ext.twr.resume_dropped` (and `resume_dropped_why`), the fold drops the resume with the move still pending, and that turn starts a new session with the full handoff saying why.
- **Native directory.** A native start on the hub's own box is stamped `ext.twr.session_store`: the runtime's own directory as the environment it starts with selects it (`Core::native_store` of the start's `SpawnConfig` env over the hub's: the selector variable from `threads.env` or the project's `env` when set or removed there, else the hub's; else the default under that environment's `HOME`; with neither touched, `HubOptions::native_dir`; links followed where it exists), recorded only, the process's environment unchanged. The move and the launch recheck compute the same for the target from the thread's `spawn_config` with no account. A move back to the native account resumes only when that is the runtime's own directory now; a hub restarted with `CODEX_HOME` (or `CLAUDE_CONFIG_DIR`, `GROK_HOME`) elsewhere starts a new session. Residual: a native start from before the stamp (every session before this build, including `dev`'s switch-back users with no accounts) is taken to be in the directory and still resumes, so `dev`'s single-account switch-back doesn't regress; one whose directory changed across that upgrade is resumed in the wrong one, and the runtime's own "not found" falls back to a new session.
- **Login note.** Account directories are shared by every workspace, but each hub has its own log. So `twr account login` and `twr account add` write, after each login that ends successfully and before the hub's check (whatever it finds, and if the hub is down), a fresh ULID to `<root data>/accounts/.logins/<hex SHA-256 of the directory, links followed>` (`twr_core::account::note_login`: temporary file, mode 600, renamed over; never inside the runtime's directory). The native account's directory there is the hub's `native_dir` as `account.list` says. Starts on the hub's own box are stamped `ext.twr.login_note` (`RunSettings::login_note`), the note of the named account's directory or the native session store, read with the bounded non-blocking reader (`fsread::read_note`); a note that can't be read is stamped `unreadable`. The move and the launch recheck read the target's note now and refuse a session started under another one (`OTHER_LOGIN`; a read error refuses). An unstamped start is under no note, so it resumes until a login is noted after it.
- **Login generation.** `AccountsView::login_generation` counts each `(runtime, account)`'s logins from its `account.status` events: one recorded after a login twr ran ended successfully (`twr account login`, `twr account add`: `account.status {refresh, after_login}`, recorded with `after_login: true` whatever the check found, `unknown` and unchanged too; old events read false), and one logged in after the last known state was logged out or missing. The refresh's checks run in a task of their own, so one that outlasts `REFRESH_WAIT` still records the login. Starts are stamped `ext.twr.account_login` (absent when 0; `RunSettings::account_login`), and the move and the turn that carries the resume refuse a session of another generation (an unstamped one is generation 0, the one before any recorded login, so it is refused only once a login was recorded). Nothing about the login is stored (no email, organization or credential). It folds from the account events, which checkpoints hold, so a start from a checkpoint has it as a full replay does. Residual: a login made outside twr in the same directory over one twr never saw logged out (no `twr account login`, no logout first) raises nothing and writes no note; docs/accounts.md says to log in with twr or log out first. That includes a native thread whose `env` selects its own runtime directory: `twr account login` logs in the hub's native directory, not that one. A logout and login to the same organization (or an expired token re-signed) also raises it: a new session where a resume would have worked, the safe side.
- An account-only move back keeps the thread's current model and effort (the resumed session runs with them, as any resume is given the thread's settings); a runtime or agent move back restores the departed session's. Where the kept value is the runtime's default and the runtime keeps a resumed session's own (`twr_adapters::resume_keeps`, `runtime_spec::clear_refusal`), the move starts a new session instead. Phase 1's account moves already record the session they leave (`ext.twr.previous_session`, next to `previous_account`), so switching back to an account is a switch-back like any other.
- Returning to the **same account** is safe for Codex too: the reasoning in its own session belongs to its own organization.
- Resuming under a **different** account is only the opt-in `move = "resume"` below, never a side effect of switch-back resume.

### Codex resume moves

`[account_policy.codex] move = "resume"` (phase 6, experimental, after the tests under [Phase 0 results](#phase-0-results)) applies to a move between two Codex accounts whose sessions share one store (both `sessions = "shared"`, or one of them the native account the others link to). Accounts with their own stores always get the handoff: twr doesn't copy rollouts between homes.

- **Pins and approvals still bind.** The move's target comes from the same rules as any move (the user's choice, or failover inside the pin); `move` only decides how the session continues. A pending permission or question is never answered by a move.
- **Fallback.** When the first turn on the new account fails before the runtime reported any work (no item started, no tool call), with Codex's encrypted-content or organization error (`invalid_encrypted_content`, `Encrypted content organization_id did not match`; the exact text is recorded by the tests and classified in `codex/translate.rs`) or `thread not found`, twr drops the session and runs the same message once more on a new session with the full handoff, like switch-back resume's fallback. The transcript says so: `the earlier codex session couldn't continue on codex:team-b (its reasoning belongs to another organization); started a new session with the handoff`. A failure after the turn did work isn't redone automatically: it gets the usual error card. The turn records `ext.twr.move: resume | handoff_fallback`.
- **A failed resume may change the shared session.** If Codex appends the failed turn's message to the rollout, the account that wrote it sees that message on its next resume. Whether it does is a test.
- **Cost.** A resumed session's first turn on the new account reads the whole context with no prompt cache. On a subscription that counts against the new account's limits, on an API key it's billed as uncached input, and on a long thread it can cost far more than the handoff's 24 KiB. Whether OpenAI's prompt cache is shared between accounts of one organization, and how a ChatGPT plan counts cached against uncached input, are unknown. A resume move of a large thread could offer to compact first, as `twr/t3code-exploration`'s cache-aware compaction does for idle Claude threads; that is open.

## Limits

### Detection

Adapters turn each runtime's signals into one shape on the failing `turn.ended`: `ext.twr.limit {kind, scope, window, resets_at, message, source}`. They also send `runtime.limits` whenever the runtime reports its state mid-session.

| Kind | Meaning | Effect |
|---|---|---|
| `rate` | short-term throttling or overload (HTTP 429 with a short wait, 529, `rateLimitExceeded`) | transient, as today. One automatic retry, never a move, never marks the account |
| `usage` | a usage window is used up | the account is limited until `resets_at` |
| `spend` | credits or a spending cap ran out | the account is limited with no reset time. It needs the user |

Per runtime:

- **Claude Code** (`claude_code/translate.rs:604`, `:911`, `:1744`): keep the latest `rate_limit_event` of the session. An assistant `error: "rate_limit"` after a `rejected` event of a window type is `usage`, with that event's `resetsAt` and `rateLimitType`. Without such an event, parse `resets <time> (<tz>)` from the message, and with no time it's `rate`. `overage` and `errorCode: "credits_required"` are `spend`. `seven_day_opus` and `seven_day_sonnet` are `usage` with `scope: model`. `allowed_warning` and `utilization` become `runtime.limits`. `resetsAt` is epoch seconds (checked in Claude's code). The parser still treats a 13-digit value as milliseconds, in case that ever changes.
- **Codex** (`codex/translate.rs:120`, `:1995`, `:3089`): keep the latest merged `RateLimitSnapshot`. `usageLimitExceeded` is `usage`, with `resets_at` from the window at 100% (primary first). `rateLimitReachedType: workspace_*_credits_depleted` is `spend`. A message "for <model>" gives `scope: model`. `rateLimitExceeded` stays `rate`. Each `account/rateLimits/updated` becomes `runtime.limits`.
- **Grok Build** (`grok_build/acp.rs`, `acp/translate.rs:2671`): "usage limit" and `UsageLimitReached`/`UsagePoolExhausted` messages are `usage` with no reset time, and "spending cap" is `spend`. The current "rate limit" text match stays `rate`.
- **Generic ACP**: as today, text matches give `rate` only. A preset may add `limit_patterns = ["quota exceeded"]` for `usage` (phase 6).

The `block_kind` code rule (`twr-decisions/src/code/mod.rs:69`) gains a `Limited` kind for `usage` and `spend`. Those are never retried at once, which removes today's wasted retry.

### Account state

When a turn ends with a `usage` or `spend` limit, the hub records `account.limit {limited: true}`. If the window is model-scoped, only that model is marked. A limited account:

- is not eligible for balancing or failover;
- clears at `resets_at` plus 60 seconds, recorded as `account.limit {limited: false}`. Without a reset time, it uses a cooldown of 30 minutes, doubling up to 4 hours. Any successful turn on the account also clears it. Twr never sends a probe turn to test an account;
- is shown in `twr account list`, the TUI's status bar notes, `/accounts` and MCP `list_accounts`.

The reducer keeps all of this, so a hub restart knows which accounts are limited and until when.

### What the user sees, failover off (the default)

When a thread's turn ends on a limit, it gets an error card in the usual place instead of today's generic failure:

```
api: claude-code:work hit its weekly limit · resets Thu 05:00 (in 13h)
  [m] move to personal   [w] retry at reset   [d] dismiss
```

- The card names the account whatever the label rule says. With one account, it reads `api: claude-code hit its weekly limit · resets Thu 05:00` and offers only `w` and `d`.
- **move** is a manual move, the same as `/account api personal`, chosen by the user. It is offered only for eligible accounts inside the thread's pin, best first by the policy's `balance` (or `most-headroom` when that's `pinned`).
- **retry at reset** records `turn.retry_scheduled {at}`. The hub starts the retry then, through `start_retry` (`hub/retry.rs:233`), unless the user sent something since. It's useful with a single account too. `resume_at_reset = true` arms it without the click (see [Retry at reset](#retry-at-reset)).
- A prompt sent to a thread whose account is limited is held in the queue, and the card above appears with an extra `[s] send anyway`. A doomed turn would only add another error.
- The orchestrator gets the card as an urgent notice, like any error today (`orch.rs:1166`), and can relay the user's answer under the relay rule.

### Retry at reset

T3 Code has the same idea built (`apps/server/src/orchestration-v2/UsageLimitRecoveryWorker.ts`, settings `autoResumeLimitedThreads` and `snoozeLimitedThreads`): every thread that hit a usage limit is resumed after the reset with no click. Phase 2 takes the parts that fit twr's held prompts, limit state and timers:

- **Opt-in.** `[account_policy.<rt>] resume_at_reset = true` (or `[threads] resume_at_reset` for every runtime) arms the retry when the limited turn ends, so the card reads `api: claude-code hit its weekly limit · retry at Thu 05:01 (automatic) [c] cancel [d] dismiss`. Off by default: an unattended turn spends the new window. It needs no second account and doesn't switch accounts, so it carries no terms warning.
- **One schedule per failure.** `turn.retry_scheduled {turn, resets_at, at, by: user | policy}` names the failed turn. The reducer keys a schedule by thread, failed turn and reset time, so a second arm with the same key (a repeated sweep, a replay, a restart) is a no-op, and a hub restart never sends it twice. `turn.retry_cancelled {turn, reason}` ends one. Any later turn on the thread drops the schedule.
- **No stale resets.** A schedule needs `resets_at` later than the failed turn's end; a reset at or before it (an expired window reported with a fresh failure) arms nothing and the card asks, which stops an immediate-retry loop. Each failed turn gets at most one automatic retry. If that retry hits a limit again, it is a new failed turn and arms again only with a later reset than the one before. `spend` limits and limits with no reset time (Grok's) are never retried automatically.
- **Checked again when it fires**, under the core lock. It is cancelled, with the reason in the transcript, when: the thread is parked, retired, deleted or trashed; a permission or question card is pending (the retry never answers one); anything was sent to the thread or started on it since the failure (by the user, the orchestrator or a wake); prompts were held for the limited account (they go first: they are what the user asked for since); the thread's runtime, account, model, effort or permission mode changed since; or the account was removed, logged out or paused. If the account is still limited with a later reset (a newer `account.limit`), the schedule moves to that reset.
- **A normal retry.** It starts through `start_retry` with the retry prompt, `ext.twr.retry_of` and `ext.twr.retry_at_reset: true`, so the transcript says why it started. It is attributed to the policy or to the user who pressed `w` (or the orchestrator relaying the user's answer, under the relay rule), never shown as a message the user typed. It goes through `start_runtime`, so pins, the account checks and the thread's permission mode apply as to any turn.
- **Survives restarts.** Schedules and `account.limit` live in the log and the reducer. At start the hub sets a timer for each one still pending and runs one whose time passed while it was down, after the checks above.
- **Prompt cache.** A retry at a reset hours away starts with an expired cache. `twr/t3code-exploration`'s cache-aware compaction offers "compact first" only before a user's or the orchestrator's message; whether an automatic retry should compact first is open.

Tests: the hub restarts between the limit and the reset, and again after the reset passed while it was down; a duplicate arm; a stale reset; a message sent, a park and a pending permission before the reset; a move or `/model` before the reset; a pin that no longer allows the account; a second limit with a later reset.

Without phase 2, a single-account user still gets phase 1's part: the `limited` block kind and the card with the reset time apply to every runtime's own login, so the wasted immediate retry is gone once phase 1 merges.

### Failover

`failover = "auto"` on a runtime's policy lets the hub move a thread without asking.

1. A turn ends on a `usage` or `spend` limit with `scope: account`. A model-scoped limit doesn't fail over: another account would usually work, but so would another model, and that's the user's call. The card offers both.
2. The hub records `account.limit`, then picks a target among the thread's eligible accounts with the policy's `balance`, skipping any account already tried for this turn.
3. With no target, the card above appears, with `all claude-code accounts in api's pool are limited; the earliest, personal, resets at 15:00`.
4. With a target, it records `thread.model_changed {account, previous_account, by: failover, ext.twr.limit}` and a `decision.made {site: "account"}`. Once background tasks finish (the same wait as every settings restart), it starts the failed turn again on the new account through `start_retry`, with the handoff and a retry prompt that says the previous turn stopped on a usage limit and to carry on from the worktree's state.
5. The transcript shows `usage limit on claude-code:work (weekly, resets Thu 05:00): moved to claude-code:personal (failover)`. The status bar shows a notice, and the orchestrator gets a non-urgent notice.

A prompt sent to a thread whose account is limited moves the thread first, then runs.

**Avoiding flapping:**

- There is no automatic move back. A thread stays on the account it moved to, because every move costs a handoff and a cold context. A user who wants it back moves it.
- A limited account stays out of every choice until its reset, so a second thread hitting the same limit never moves *to* it.
- Each turn tries each account at most once. A thread fails over at most 3 times in any rolling hour. Past that it gets the card.
- `avoid_above` keeps new threads off accounts near their limit, with its 10-point gap.
- Only `usage` and `spend` fail over. A burst of 429s or 529s is `rate` and gets today's retry.

**Mid-turn.** A limit ends the turn: the runtime stops answering. Whatever the turn did is in the worktree, and its tool calls and text are in the log. The handoff's recent-turns section includes the failed turn, so the new session sees what was attempted and the worktree shows what was done. Background tasks started by the old session (Codex terminals, Claude background Bash) keep running until they finish. The move waits for them, as settings restarts already do. `twr model api --account personal --now` doesn't wait. A thread that is waiting on a permission card when its account becomes limited is left alone, because the turn hasn't failed.

**The orchestrator.** It fails over with an orchestrator switch to the other account with `--no-summary`, since the limited session can't write one (phase 4). With failover off, it gets the same card, shown in the orchestrator pane.

## Usage tracking

For `most-headroom`, the status bar and `twr account list`:

- **Passive, always on.** `runtime.limits` from live sessions: Claude's `rate_limit_event` (utilization and reset per window, sent on change) and Codex's `account/rateLimits/updated`. This costs nothing extra, but it only knows about accounts with a live session.
- **On request.** `twr account status --usage` asks each account: Claude through `get_usage` on a live session of that account, Codex through `account/rateLimits/read` on a short-lived `codex app-server` in that account's home (what Codex's own `/status` does). Grok has no usage call, so twr shows its own count.
- **twr's own count.** The tokens per account from `turn.ended.usage`, summed over 5 hours and 7 days. It's always available and works for every runtime, but it's only an approximation of the provider's meter. It's shown as `twr: 1.2M tokens / 5h`, never as a percentage.

twr doesn't poll the providers in the background. Background polling would mean starting sessions or network calls the user didn't ask for. An account with no live session shows its last known usage with its age.

## Showing accounts

The label rule is in [When labels show](#when-labels-show). This section covers each surface. In the examples, Claude Code has two logged-in accounts (`work`, `personal`) and Codex has one.

### TUI

- **Cards and tiles.** On `dev` the card title says who runs a thread on the right (`card_block`, `ui.rs:1626`; tiles reuse it, `render_tile`, `:3400`), built by `who_fit` (`:2590`): `claude-code@gpu · opus-4-1`, giving way in steps when the title is narrow (runtime left out, model cut, model left out, box name cut), while the box state is always kept. The account joins the runtime, `claude-code:work · opus-4-1` and `claude-code:work@gpu · opus-4-1`. When `who_fit` leaves the runtime out, the account stays (`work@gpu · opus-4-1`, or `work · opus-4-1`): with labels shown it tells two cards apart, the runtime name doesn't. After that the model gives way, then the account, then the box name, exactly as before.
- **Dive header** (`dive_header`, `ui.rs:5184`): `talking to api  claude-code:work · opus-4-6 · mode auto · Esc returns to the orchestrator`. The box-state fallback candidates keep the account the same way. The orchestrator's header (`main_title`, `:5087`) reads `ORCHESTRATOR  claude-code:personal · claude-opus-4-6`.
- **Usage in the status bar** (`usage_spans`, `ui.rs:7690`, through `twr_core::usage::UsageView::shown`, `usage.rs:313`): `shown` already labels a runtime's entries `Claude:work` once the runtime has more than one; phase 1 makes it follow the same rule as everything else, so a stale entry of a removed account doesn't turn labels on.
- **Status bar** (`status_notes`, `ui.rs:5269`): a note while an account is limited, `work limited until 15:00`. With a single account it's `claude-code limited until 15:00`. Failover and move results show as notices: `api moved to personal (work hit its weekly limit)`.
- **`/model api`** (`model_show`, `app.rs:1535`): `api: account work, model opus-4-6, effort high`. The account part appears only when labels show.
- **Completion** (`pools`, `app.rs:5289`): thread rows read `idle · claude-code:work · mode auto`. A new `ArgKind::Account` lists accounts with their status (`work  limited until 15:00`) for `/account` and `/new --account`.
- **The orchestrator picker** (`picker_lines`, `ui.rs:732`) and `/orch` completion: with labels shown, one row per account (`claude-code:work  installed · logged in`).
- **`/accounts`**: an overlay with the `twr account list` table, closed by any key, like `/trash`.
- **Transcript lines**: account changes always show (`account work → personal (by failover)`). A session-start line (`new claude-code session`) names the account only while labels show.
- **No colours per account.** Colour already means two things in the TUI: which thread (six 256-colour hues hashed from the thread id, `THREAD_COLORS` at `ui.rs:2098`, on conversation boxes, request boxes and thread names) and which status (`status_color` and `reason_color`, `ui.rs:1941`). Most other hues are taken by accents. A coloured `work` inside a thread's box would read as another thread's colour, and next to a card's status word it would read as a status. So the account is plain text in the label, styled like the runtime.

### CLI

- `twr thread list` (`thread_list`, `main.rs:5420`): no new column. The RUNTIME cell reads `claude-code:work`, so a single-account user sees exactly today's table.
- `twr model api` (`model_show`, `main.rs:2204`): `api: runtime claude-code, account work, model ...`, with the account only when shown.
- `twr info` (`info`, `main.rs:4132`): one line per runtime with several accounts, `accounts: claude-code 2 (work limited until 15:00)`.
- `twr orchestrator show` (`orchestrator_text`, `main.rs:5219`): the account when shown, plus `configured: account personal`.
- `--json` everywhere: `account` and `account_shown` are always present, so scripts get a stable shape.
- `twr events --pretty` (`pretty.rs`): prints `account=work` whenever the event recorded one, whatever the label rule says now. It prints history, and history keeps the account.

### MCP and the orchestrator

**Decision: MCP omits `account` from thread views unless the runtime shows labels.** The socket API (`thread.list`, `thread.get`, `--json`) always includes it, for a stable schema. MCP's `list_threads` and `get_thread` drop `account` and `account_shown` when `account_shown` is false, and keep `account` (plus `account_removed`) when it is true.

Why omit rather than always include it de-emphasised:

- The orchestrator repeats what it reads. A constant `"account": "default"` on every thread invites it to mention accounts to a user who has only one, which is exactly what the label rule is meant to prevent.
- `list_threads` runs often, and a field on every thread costs tokens each time.
- The MCP layer already reshapes `thread.get` (`AboutThread::thread`, `crates/twr-mcp/src/lib.rs:619`), so the filter fits there; `list_threads` (`:947`) passes `thread.list` through today and gains the same filter. Scripts that need a stable shape use the socket API, not MCP.

Other MCP changes:

- **`list_accounts`** (new, read-only; `account.list` joins `METHODS` and `ORCHESTRATOR_METHODS` in `server.rs:629`/`:697`, not `ORCHESTRATOR_CHANGES`) returns per runtime `{runtime, several: bool, accounts: [{name, native, source, login, threads}]}`, with `paused` and limit fields from later phases. It has no directory. Thread agents (`AGENT_METHODS`) don't get it. `get_usage` (`lib.rs:1240`) already shows usage per account through `usage.get`.
- **`spawn_thread {account?}`** (`SpawnThreadParams`, `lib.rs:177`, next to its `box`) and **`set_model {account?}`** (`SetModelParams`, `lib.rs:272`). Their descriptions say to name an account only when the user asks for one, and that a project pin can't be left.
- `render.rs`: `model_changed` (`render.rs:139`) writes `account changed by orchestrator: work -> personal` and `runtime changed ...: claude-code:work -> codex`. `thread.created` (`render.rs:452`) adds `account=` when shown. New lines for `account.limit` (`account claude-code:work limited until 15:00 (weekly window)`) and for `account.added`/`removed`.
- Handoff thread lines (`hub/handoff.rs`) use the label: `- api (...) project=acme-api runtime=claude-code:work, model opus ...`.
- `crates/twr-hub/src/orchestrator_prompt.md:38-46` gets a short paragraph: "Some runtimes have several accounts; `list_accounts` shows them, and a thread shows `account` only on such a runtime. Pick or change an account only when the user asks, or when the user answers a limit card. Never move a thread off its project's pinned accounts. Don't talk about accounts on a runtime that has one."

## Security

- **twr never reads, stores, prints, logs or passes a credential.** It sets one directory variable and runs the runtime's own login, logout and status commands. The status checks keep only logged in or out and the method, as `dev`'s agent probes do: no email, organization or plan.
- **Removed variables** keep a stray API key from overriding an account (see [Environment](#environment)). The removal list for each runtime is in one place, `AccountSupport`.
- **Directories** are 0700, created by twr under the data directory, and an adopted `--dir` that other users can read is refused. `twr account remove --purge` asks first.
- **What gets logged.** The event log stores account names, directories, share settings and login state. The log is in the user's data directory and the hub socket is 0600. Directories go to neither MCP nor thread agents.
- **No new exposure to agents.** A thread's agent sees its own `CLAUDE_CONFIG_DIR` in its environment and, being the same Unix user, could already read every runtime's credential files, native ones included. Accounts don't change that. The docs keep saying that a `bypassPermissions` thread can read anything the user can.
- **Org policy per account.** The memory guard checks the right `remote-settings.json` for each account (see [The orchestrator and the memory guard](#the-orchestrator-and-the-memory-guard)).
- **Pins are hard limits.** Balancing, failover and the orchestrator never take a thread outside its project's pin, so work code doesn't drift onto a personal account through automation.

## Terms of service

What the providers say (quoted in full, with links, in [Sources](#sources)):

- **Anthropic, Claude Code legal and compliance**: "Advertised usage limits for Pro and Max plans assume ordinary, individual usage of Claude Code and the Agent SDK." OAuth "is designed to support ordinary use of Claude Code and other native Anthropic applications." Third parties may not "route requests through Free, Pro, or Max plan credentials on behalf of their users", nor "collect, store, or intermediate Claude.ai credentials or session tokens — sign-in to a Claude account must complete through Anthropic's own flow." The same page says this does not "prevent an end user from signing in to the unmodified Claude Code binary with their own Claude subscription." Anthropic "reserves the right to take measures to enforce these restrictions ... without prior notice."
- **Anthropic Usage Policy**: no "coordinat[ing] malicious activity across multiple accounts to avoid detection or circumvent product guardrails", no circumventing a ban with another account.
- **Anthropic Consumer Terms**: no sharing of login credentials. Automated access is allowed with an API key "or where we otherwise explicitly permit it".
- **OpenAI Terms of Use**: no sharing of credentials, and no "circumvent[ing] any rate limits or restrictions".
- **xAI Terms**: no sharing of credentials. I found no clause about multiple accounts (the acceptable-use page couldn't be fetched).

None of them forbids one person holding several subscriptions, such as a work plan paid by an employer and a personal one. The risk is in what accounts are used *for*. Spreading one person's work across subscriptions so it never stops at a limit reads like "circumventing rate limits" (OpenAI) and isn't "ordinary, individual usage" (Anthropic). Automatic failover and usage-driven balancing are exactly that mechanism.

How the design reflects that:

- twr runs the unmodified CLIs with the user's own logins, and sign-in always completes through the provider's own flow in the user's terminal. twr never sees a code or token. Device-code logins in the TUI (later) only display what the CLI prints. Claude's pasted-code flow stays out of the TUI for this reason.
- Pinning per purpose is the main use. `balance = "pinned"` and `failover = "off"` are the defaults.
- `failover = "auto"` and every `balance` other than `pinned` (`round-robin`, `least-busy`, `most-headroom`) are opt-in per runtime and carry the same warning. Round-robin spreads load as much as the usage-driven ones do, so it gets the warning too. The hub logs it once when it starts with any of them on. `docs/accounts.md` puts the caveat next to the settings: "Providers set usage limits per subscription. Moving work between your accounts to keep going past a limit may conflict with their terms (Anthropic's 'ordinary, individual usage', OpenAI's rule against circumventing rate limits). Check the terms of each subscription, especially for accounts your employer pays for. twr doesn't decide this for you."
- The limit card's default is the reset time, with the move as one choice among several.
- twr never creates accounts, never shares one login between people, and never uses a token outside the runtime that issued it.

This is my reading of public terms, not legal advice, and the terms change. The docs should link to the sources rather than paraphrase them.

## Migration and backward compatibility

- **No accounts added:** there is only the native account, no labels, no new environment, no new prompts, the same CLI output and the same MCP output. `ThreadView.account` is `None` for every existing thread, and old events need no rewrite.
- **The first `twr account add`** names the native account, and existing threads show that name once labels appear. Nothing restarts.
- **Config:** every new key is optional. The config structs use `deny_unknown_fields`, so an older twr refuses a config with the new keys. That's normal for twr, but the release notes should say so for anyone who downgrades.
- **Log:** new event kinds read as `Unknown` on an older twr. `ChangedBy::Failover` is the exception (see [Events](#events)).
- **Socket API:** additive fields (`account`, `account_shown`, `account_removed`) and new `account.*` methods. `SKILL.md` documents them.
- **Runner protocol:** phase 1 works with today's protocol 7 (`runner.rs:128`), since the environment already rides in `SpawnConfig.env` and phase 1 refuses named accounts on remote boxes. Phase 5 adds account probes and setup to the box ops (next to `Op::Agents`) and bumps the protocol, with the existing drain path for the previous one (`DRAINABLE_PROTOCOLS`).
- **`TWR_HOME` instances:** each instance has its own registry, and named-account directories default to its own data directory. Native logins stay shared, as `docs/configuration.md` already says. `--dir` can point two instances at one account directory.
- **A user's own `CLAUDE_CONFIG_DIR` setup:** someone who already runs twr under `CLAUDE_CONFIG_DIR=~/.claude-work` keeps working. That directory is their native account. They can adopt other directories with `twr account add --dir`.

## Risks

- **Linked files drift.** The three runtimes write through links today (phase 0), but a future version, an editor that saves by replacing the file, or a manual copy could break a link without any error. `twr account status` detects that. Directory links are safe.
- **Runtime formats change.** `rate_limit_event`, `get_usage`, `account/rateLimits/*` and the status JSON are not all stable APIs, and parts are marked internal or experimental. Parsers must treat missing fields as "unknown", never as "not limited", and the fixtures must be recorded from real sessions as twr already does.
- **Reset times are approximate.** Codex says outright not to infer recovery from them. twr clears a limit at the reset, but a turn that fails again re-marks the account with the new time, so the worst case is one wasted turn per reset.
- **Model availability differs by plan.** A move can land on an account without the thread's model. The card says so and offers `/model`.
- **Handoffs lose context.** Every move is a fresh session. Failover can lose the thread of a long, subtle task. The handoff is twr's best summary, and phase 6 explores real resumes.
- **The orchestrator's memories split** across accounts unless twr points every account's orchestrator at the native memory directory (`autoMemoryDirectory`), which `config` sharing does by default. That depends on a Claude Code setting staying as it is.
- **Provider enforcement.** A provider may act against automated failover regardless of twr's framing. Keeping it opt-in and documented is the mitigation. Removing it would be the stronger one.
- **Shared Codex sessions across organizations.** `sessions = "shared"` between accounts of two organizations breaks every session either of them resumes after the other wrote to it. twr can't check the organization, so the setting is opt-in, documented for one person or one organization only, and backed by the resume fallback.
- **Unattended retries spend a window.** `resume_at_reset` starts a turn nobody watched start. It is off by default and cancelled by anything the user does on the thread.
- **More states for the user to read.** The label rule keeps single-account users free of all of it.

## Open questions

Questions 1 to 8 need logged-in accounts to answer, and 9 is a design choice for phase 2; the user's steps are in `~/files/twr-multi-account/pending.md`, and the tests are listed under [Phase 0 results](#phase-0-results).

1. Does a copied Claude session's first turn succeed under a **different** account? (needs a second Claude login)
2. Does a copied Codex rollout's first turn succeed under a different ChatGPT account, or fail on encrypted reasoning? (needs a second Codex login)
3. Do two Codex homes running the memory pipeline at once, through a linked `memories_1.sqlite` and `memories/`, both succeed? (needs two logged-in Codex homes; the same ChatGPT account logged in twice is enough)
4. Does a Codex session resume under a different account of the **same** organization (two seats of one ChatGPT workspace), and does it fail under a different organization with the error text the fallback should classify? (needs two such logins; two personal accounts cover only the different-organization case)
5. Do two logged-in Codex homes work on one linked session store at once (`sessions/`, `session_index.jsonl`, `state_5.sqlite`, `thread_history_1.sqlite`), and does Codex create its lock files before the links exist, `codex login` included? (the same account logged in twice is enough)
6. Does a failed cross-organization resume append to the shared rollout, and does the original account still resume it afterwards?
7. How much of the new account's limit does a resumed first turn use against a handoff, and does an organization share its prompt cache between accounts? (usage numbers from the tests above)
8. Can the hub tell two Codex logins' organizations apart without storing them (`account/read`)?
9. Should an automatic retry at a reset compact an expired-cache context first?

### Decisions

From the reviews of 2026-10-08 and 2026-10-20:

- Accounts can be declared in `config.toml` and recorded by `twr account add`; config wins, `twr account list` shows the source, and the account commands never write `config.toml` (see [Two sources](#two-sources-configtoml-and-twrs-records)). This replaces the 2026-10-08 decision to keep them out of the config.
- Pins are as flexible as possible: names, globs, lists, per runtime, per box project, per thread, and exclusions (see [Pins](#pins)). A pin naming an account the runtime doesn't have refuses the thread, with an error saying how to add the account or change the pin; the `unmatched_account` setting is dropped.
- Usage-based balancing and every other strategy but `pinned` carry the same terms warning as failover, and all of them are opt-in.
- Settings files: the runtimes write temp-then-rename onto the link's resolved target, so linked files stay linked (Phase 0, both runs); files and directories are linked, and `twr account status` checks the links.
- No email, organization or plan is kept for an account, matching `dev`'s agent probes.
- Codex's `memories_1.sqlite` and `memories/` are linked with `share = all`. Opening the database through a link works. Two logged-in homes running the memory pipeline at once is the last check.
- Claude Code memory is shared through `autoMemoryDirectory`, never a link, because a link makes `dontAsk` refuse memory writes (tested).
- Accounts get no colour: it would clash with the thread colours (see [TUI](#tui)).
- A thread that failed over doesn't move back when its old account resets.
- The native account is called `default` until the user names it. No separate "unnamed" state.

## Implementation plan

Each phase is shippable on its own and documented as it lands (`docs/accounts.md` new, plus `runtimes.md`, `configuration.md`, `usage.md`, `tui.md`, `orchestrator.md`, `SKILL.md`). Tests follow the existing patterns: the `Recorder` adapter in `crates/twr-hub/tests/runtime_switch.rs` for moves, recorded fixtures in `crates/twr-adapters/tests/fixtures/` for parsers, and reducer unit tests in `reduce.rs`.

### Phase 0: checks before code (no shipped code)

The checks that need no login were run on 2026-10-08 (scratch directories under `/tmp`) and again on 2026-10-20 with Claude Code 2.1.304, Codex 0.162.1 and grok 1.0.50 (scratch directories under `~/files/twr-multi-account/phase0/`, every file operation traced with `strace`). No real config directory was changed, no login was completed, and no credential was read.

#### Phase 0 results

| Question | Answer | How |
|---|---|---|
| Does Claude Code keep a linked `settings.json`? | Yes. User settings are written through the link on purpose (`allowSymlink: true`), atomically on the target | `claude plugin marketplace add`, `plugin install` and `plugin disable` in a scratch `CLAUDE_CONFIG_DIR` with a linked `settings.json`, plus `--debug-file` (`Writing through symlink: ... -> ...`) and the 2.1.294 code |
| Does Codex keep a linked `config.toml`? | Yes, and it keeps comments | `codex mcp add`, `codex features enable` in a scratch `CODEX_HOME` |
| Does Grok keep a linked `config.toml`? | Yes, but it rewrites the whole file, drops comments and adds a `[marketplace]` key of its own | `grok mcp add` in a scratch `GROK_HOME` |
| How do they write it? (2026-10-20) | All three write a temporary file next to the **resolved** target and rename it over the target: Claude `settings.json.tmp.<pid>.<hex>`, Codex `.tmpXXXXXX`, Grok `.<pid>.0.tmp`. The rename replaces the target, never the link | `strace` of the writes above |
| Does `CLAUDE_CONFIG_DIR` keep Claude out of `~/.claude`? (2026-10-20) | No writes outside it. It still reads the Anthropic profile store (`~/.config/anthropic`, moved only by `ANTHROPIC_CONFIG_DIR`), a hardcoded protected-path list in `~/.claude/state`, and, when the working directory is under `$HOME`, `~/.claude/CLAUDE.md` and `~/.claude/rules` as an ancestor directory's project files. With `ANTHROPIC_CONFIG_DIR` set too, `auth status` touched no native path | `strace` of `auth status`, plugin and MCP commands, `-p` |
| Does `CODEX_HOME` keep Codex out of `~/.codex`? | Yes, no access at all | `strace` of `login status`, `mcp add`, `features enable`, app-server with `thread/start` |
| Does `GROK_HOME` keep Grok out of `~/.grok`? | Yes, apart from `PATH` lookups in `~/.grok/bin`. But it reads `~/.claude.json` and scans `~/.claude/skills`, importing Claude Code's setup | `strace` of `models`, `mcp add`, `agent --no-leader stdio` |
| Do stray credentials override an account? (2026-10-20) | Claude: `ANTHROPIC_API_KEY` gives `authMethod: api_key`, `CLAUDE_CODE_OAUTH_TOKEN` gives `oauth_token`. Grok: `XAI_API_KEY` gives `You are using XAI_API_KEY.` Codex: `OPENAI_API_KEY`, `CODEX_API_KEY` and `CODEX_ACCESS_TOKEN` change neither `login status` nor app-server `account/read` | fake keys in a scratch directory |
| Does `dontAsk` let Claude write its memory through a link? | No. With `projects/<key>/memory` linked, and with `projects/<key>` linked, the write was refused ("Permission to use Write has been denied because Claude Code is running in don't ask mode"), with and without twr's guard hook. The same write to a real memory directory worked | four Haiku turns on the native login in the scratch project `/tmp/acct-mem/cwd`, with `--permission-mode dontAsk --tools Read,Write,Edit` and twr's guard as the orchestrator gets it. The scratch project was removed afterwards |
| Does `autoMemoryDirectory` work instead? | Yes. With it set to a real shared directory through `--settings`, the `dontAsk` write was allowed and landed there | one more Haiku turn, same setup |
| Are transcripts written through a linked project directory? | Yes | the session files of those turns |
| Can Claude's tools write inside a linked directory? | Yes. Only a target that is itself a symlink is refused, and parent directories aren't checked for the Write and Edit tools (they are for project and local settings and repo `.claude/` files) | the 2.1.294 code: `checkParentDir` is set only for those callers |
| Can two Codex homes share `memories_1.sqlite` through a link? | Opening and starting up: yes, with one write-ahead log at the target. The memory pipeline needs a login, so concurrent memory writes are untested | two `codex app-server`s at once, one per home. A third run with `[features] memories = true`, no login and copies of old sessions recorded no jobs |
| `resetsAt` units | epoch seconds | Claude's code: `resetsAt*1000 > Date.now()` |
| Does `claude auth login` need a terminal? | No terminal to start. It prints the URL and waits on standard input for the pasted code, so someone must finish it | scratch directory, standard input from `/dev/null`, browser opening off (`CLAUDE_TEST_NO_OPEN`) |
| `claude auth status` when logged out | `{"loggedIn": false, "authMethod": "none", "configDirectory": ...}`, exit 1 | scratch directory |
| `codex login status` when logged out | `Not logged in`, exit 1 | scratch `CODEX_HOME` |
| Grok login status | `grok models` prints `You are not authenticated.` first (exit 0) | scratch `GROK_HOME`, and the native one |
| Does `GROK_HOME` move the leader socket? | Yes, to `$GROK_HOME/leader.sock` | a leader started with a scratch `GROK_HOME` (no other leader was running) |
| Is a Claude session copied to another config directory found? | Yes. It got past the lookup and stopped at `Not logged in`. An unknown id gives `No conversation found` | a real transcript copied into a scratch directory's `projects/<key>/` |
| Is a Codex rollout copied to another home found? | Yes, by thread id and by `path`, with no login | `thread/resume` on a scratch home |
| Do Codex rollouts carry encrypted reasoning? | 38 of the 40 most recent do, and the binary has no strip-and-retry for it | the local `~/.codex/sessions`, read-only |

Two side notes. Codex warns, harmlessly, that it won't create helper binaries when `CODEX_HOME` is under `/tmp`; twr's account directories live in the data directory. And the native Grok login on this machine reported `You are not authenticated.` on both dates.

Still to do, each needing a login (see [Open questions](#open-questions)):

- **With a second Claude login:** the user runs `CLAUDE_CONFIG_DIR=<scratch> claude auth login` with the other subscription. A session made on the native account, with thinking and a tool call, is copied into the scratch directory and resumed with one short turn. Watch for the thinking-signature strip-and-retry in a `--debug-file`, and check that the answer remembers the earlier turns.
- **With a second Codex login:** the same with `CODEX_HOME=<scratch> codex login` and a copied rollout that carries encrypted reasoning. Watch for "Encrypted content organization_id did not match".
- **Codex sessions across organizations** (T3 Code's shared session store, 2026-10-27): with the same account logged in twice (no second person needed), link one scratch home's session store into another scratch home's, run both at once, check `PRAGMA integrity_check` on the state databases, list which lock files Codex created before the links, and resume one home's session from the other. With two personal ChatGPT accounts (different organizations), resume a session that has reasoning items under the other account through the shared store: record the exact error, whether the failed turn was appended to the rollout, and whether the first account still resumes it. With two seats of one organization, if the user has them, the same resume should succeed. Record each first turn's input and cached tokens against a handoff of the same thread. No real home is linked into a scratch one: the shared store is a scratch home too.
- **With two logged-in Codex homes** (the second login above, plus a scratch home logged in to the native account): memories on in both, a few short sessions in each, then both started at once with `memories_1.sqlite` and `memories/` linked from one to the other. Check that every extraction job finishes, that consolidation writes its files, and that `PRAGMA integrity_check` says ok.

Real limit-hit fixtures come from transcripts already on disk where possible (a 429 `rate_limit` line exists locally). The findings also go into `docs/runtimes.md`'s "checked against" table when phase 1 lands.

### Phase 1 (MVP): accounts, login, pinning, labels, manual moves

In phase 1 (file references are to `dev` at `bbe218e`; the phase was rebased onto `dev` at `245deb8`, then onto `dev` at `cfd1937`):

- **Accounts and sources.** `crates/twr-core/src/account.rs` (new): names, `AccountRef` (`claude-code:work`), the pin grammar (name, glob, list), `AccountsView` over `account.*` control events plus the config's declarations (config wins), `shows_labels`, `label`. Events in `crates/twr-core/src/event.rs` (`event_body!`, `:1545`): `account.added`, `account.removed`, `account.status`. Config in `crates/twr-core/src/config.rs`: `[accounts.<rt>.<name>] {dir, share, native}`, `ProjectConfig` (`:821`) `account`/`accounts`/`exclude_accounts`, `BoxProjectConfig` (`:811`) the same, `OrchestratorConfig` (`:187`) `account`, `[account_policy.<rt>] default`, `RuntimePreset` (`:908`) `account_env`/`login`, all checked in `validate` (`:1158`).
- **Per-runtime facts.** `crates/twr-adapters/src/account.rs` (new): the variable(s) to set, the removal list, the login argv (reusing `agents::login_args`, `agents.rs:73`), and the status probe (reusing `agents.rs`'s `claude auth status` / `codex login status` with the account's variables). `process.rs` gains the removals.
- **Hub.** `crates/twr-hub/src/hub/accounts.rs` (new): `account.list`, `account.add`, `account.remove`, `account.status` on the socket (`server.rs:629` `METHODS`; `account.list` also in `ORCHESTRATOR_METHODS`); status checks at start, every 10 minutes and after an `auth_failed` turn; `hub.info` gains `accounts`. `runtime_spec.rs:99` resolves `<runtime>:<account>`. `threads.rs:674` gets `thread_account` (explicit, box project or project pin, orchestrator, policy default, native) with the refuse rule, recorded at `:309`. `append.rs:126` keeps `rt.account_env` from the log, like `acp_env`, and `trash.rs:92` saves it with the rest. `start_runtime` (`hub.rs:2016`) and `attach_config` (`attach.rs:209`) add it last; `restart.rs:93` `RunSettings` gains the account, stamped on `thread.started`, with `account_change` as the restart reason; `hub/usage.rs:61` takes the account from `RunSettings`. Named accounts on remote boxes are refused.
- **Moves.** `model.rs:248` and `switch.rs:81`: an `account` on `thread.set_model` is a move (new session, handoff) even on the same runtime; `thread.model_changed` gains `account`/`previous_account`; the reducer (`reduce.rs:507`, `:442`) treats it like a runtime move; the handoff names the previous account (`switch.rs:184`).
- **Orchestrator.** `[orchestrator] account`, `twr orchestrator use <rt>:<account>`; `PolicySources` per account directory in `restrict` (`orch.rs:2722`); on a named Claude Code account whose sharing covers memory, `autoMemoryDirectory` in the orchestrator's `--settings`, and `twr claude-memory-guard --memory-dir <dir>` (`guard.rs:74`, `:312`).
- **Shared state.** `twr account add` creates the directory (0700) and the links of its `share` set (Claude Code, Codex, Grok Build tables above), links only what exists in the native directory, and never overwrites a file.
- **Labels.** TUI: `who_fit` (`ui.rs:2590`), `dive_header` (`:5184`), `main_title` (`:5087`), `model_show` (`app.rs:1535`), `pools` (`:5289`), usage labels (`usage.rs:313`), `/account` and `/accounts` (`slash.rs`). CLI: `thread_list`, `model_show`, `info`, `orchestrator_text`, `twr events --pretty`. MCP: `list_accounts`, `account` on `spawn_thread` and `set_model`, the `list_threads`/`get_thread` filter, `render.rs` lines, the orchestrator prompt paragraph.
- **Limit messages.** Adapters put `ext.twr.limit {kind, window, resets_at, message}` on a turn that ended on a usage or spending limit: Claude Code from the `rejected` `rate_limit_event` (its `rateLimitType` and `resetsAt`, `translate.rs:668`) or the assistant's `rate_limit` error text (`resets 5am (UTC)`, `:1048`); Codex from `usageLimitExceeded` (absent from `TRANSIENT_ERRORS` today, `codex/translate.rs:144`) with the latest rate-limit snapshot's reset; Grok from its usage-limit and spending-cap messages. `twr-decisions` gains a `limited` block kind (`lib.rs:122`, `code/mod.rs:69`) that is never retried at once, which ends today's wasted retry, and the error card (`retry.rs:121`) says `api: claude-code:work hit its weekly limit · resets Thu 05:00 (in 13h)`, naming the account whenever it is a named one. Auth hints name the account: `(sign in again: twr account login claude-code work)`.
- **With `dev`'s later features** (`245deb8`). Next-message suggestions (`hub/suggest.rs`) run on each runtime's native login, never a thread's account: their failure and back-off state is per runtime, and the hub's agent probe that says whether a runtime can run them checks the native login. Their Codex home is the native Codex directory (`hub::accounts::own_native_dir`, which now falls back to the password database's home as Codex does), so one helper finds it for both. The hub's wake after background work (`hub/wake.rs`) queues an ordinary turn, which starts through `start_runtime` with the account checks and environment, and a job, a `!` command included, gets the thread's account environment, checked again at launch. A turn that hit a limit ended, so it gets no hung-turn card, and it failed, so no wake follows it. Codex's `auto_review` default changes only the reviewer; the `auto` sandbox's writable roots read no account variable.
- **With `dev`'s later features** (`cfd1937`). *Schema:* `dev`'s checkpoints took schema 9, so the accounts fence is 10: a database without account data stays at 9, the first account record raises it to 10 in the same transaction, this build opens and checkpoints both (`store::is_own_schema`: the store's open, `twr log checkpoint`'s prewarm), and a build that knows only 9 refuses 10. A database a development build of accounts marked 9 (schema 8's tables) gets the checkpoint tables when opened, and 10 on its replay. *Checkpoints:* a checkpoint keeps the hub thread's account events and whether the log has account data (`FoldSnapshot` format 3), never the accounts themselves: the start folds those events over the config it has then, so config still wins over records and no config key is part of a checkpoint's settings; the start-up checks (`check_at_start`, the moved-usage clears, the login checks) run after a checkpoint start as after a whole replay. Account events are control events: never pruned, in the control snapshot and the compacted backlog, and the TUI still asks for `account.list` once subscribed and after each `account.*`. *Switch-back resume:* a move back resumes a saved session only on the account it ran on: the move that left it was away from the same runtime and the same account (the native account under any name; a named one by name, not removed), the start that ran it (a runtime-reported one) stamped the directory the account has now (`same_dir`), a named account's runtime was given it by the same path (`account_env_dir`), and no `account.removed` of the account came after that start; the turn that carries the resume checks again (`ext.twr.resume_dropped`). A move to another account of the runtime, to its account in another directory, or to an account removed and added again starts a new session with the full handoff; a move back to the account a thread left resumes its own session there, and the move's message says which. No session moves between accounts (`move = "resume"` and `sessions = "shared"` are not built, and config refuses them, below). *Prompt cache:* an account move forgets the cache's expiry like a runtime move (a new session, or its own resumed one), and a compaction (the user's, an offer's, `before_expiry`'s) runs in the thread's runtime on its account. *Memory:* the orchestrator keeps `dev`'s twr memory on any account, Codex's and Grok Build's own memories stay off on a named account too (Codex's `-c` overrides, `GROK_MEMORY=0`, neither touched by the account's variables), and a Claude Code orchestrator on a named account that shares its configuration keeps the native memory directory (`autoMemoryDirectory`, the guard's `--memory-dir`). *Config:* the later phases' keys (`balance`, `pool`, `failover`, `avoid_above`, `move`, `resume_at_reset`, `sessions`) are refused, saying they are not available yet. Summaries run on the native login, like suggestions.
- **Docs.** `docs/accounts.md` (new), plus `configuration.md`, `runtimes.md`, `usage.md`, `tui.md`, `orchestrator.md` and `SKILL.md`, with the terms caveat.
- **Tests.** `crates/twr-hub/tests/accounts.rs` (new), built on the environment-capturing adapters of `tests/hub.rs` (`:76`, `:990`) and `tests/orchestrator_switch.rs` (`:31`): both sources and precedence, add and remove through the socket, labels per runtime, pins and refusals, environment and removals on spawn, attach and jobs, moves with handoff, the orchestrator on an account, replay after a restart. Adapter tests for limit parsing and status probes with fake CLIs (`tests/agents_probe.rs`). TUI tests from `cards_tiles_and_the_main_pane_say_runtime_and_model` (`ui.rs:11681`).

Not in phase 1: limit state and timers, retry at reset and held prompts (phase 2); balancing (phase 3); failover (phase 4); remote accounts (phase 5); `pause`/`resume`, `relink` and copying user-scope MCP servers into a new Claude account (phase 2); session transfer and TUI logins (phase 6).

### Phase 2: limits and usage

`account.usage` and `runtime.limits` already exist on `dev`; phase 1 gives them the account. Phase 2 adds `account.limit` with reset and cooldown timers; the limit card with move, retry at reset and send anyway (`hub/retry.rs`, `hub/attention.rs`); held prompts for limited accounts; `turn.retry_scheduled` and the opt-in `resume_at_reset` with its keyed, restart-safe schedules and fire-time checks ([Retry at reset](#retry-at-reset)); status bar notes; Grok's login status through `grok models`; `twr account pause|resume` and `relink`; copying a new Claude account's user-scope MCP servers through `claude mcp add-json`.

### Phase 3: balancing and richer moves

`balance`, `pool` and `avoid_above` in `thread_account`; `decision.made {site: "account"}` and its row in the `d` view; duplicate-identity handling; `twr account pause|resume`; `--summary` on manual moves using the orchestrator-switch summary code (`hub/handoff.rs`).

### Phase 4: automatic failover (opt-in)

`failover = "auto"`: the move-and-retry path in `hub/retry.rs` with per-turn and per-hour budgets; moving before a prompt reaches a limited account; orchestrator failover through `use_orchestrator` (`hub/orch.rs:2352`) with no summary; the start-up warning and the docs caveat; the decision on how to encode `ChangedBy::Failover`.

### Phase 5: remote boxes

`dev` has the pieces: `twr box login <box> <agent>` runs `twr runner --login <agent>` on the box with a terminal (`crates/twr-cli/src/boxes/doctor.rs:2291`), and box daemons probe their agents every 10 minutes (`crates/twr-runner/src/agents.rs:233`, `Op::Agents`). Phase 5 adds the account's directory to both: `twr account add <rt> <name> --box <box>` and `twr box login <box> <agent> --account <name>` run the login in the account's directory on the box, and the box's probes cover its accounts (a protocol bump, next to `Op::Agents`); labels count accounts by name across boxes; `twr info` shows them per box.

### Phase 6: experiments

The `move = "resume"` session transfer for Claude Code if phase 0 shows it works; for Codex, `sessions = "shared"` and `move = "resume"` between accounts of one store with the handoff fallback ([Codex resume moves](#codex-resume-moves)), if the cross-organization tests show where it works; device-code logins in the TUI for Codex and Grok; `limit_patterns` and `account_share` for ACP presets.

## Sources

Runtime behaviour was checked with the installed binaries' `--help`, `auth status --json`, the app-server JSON schema generated from Codex 0.160.0, grok's README and user guide in `~/.grok`, and a live ACP `initialize` against grok 1.0.46. No login state was changed and no credential file was read.

- Claude Code environment variables: https://code.claude.com/docs/en/env-vars
- Claude Code `.claude` directory: https://code.claude.com/docs/en/claude-directory
- Claude Code authentication and credential management: https://code.claude.com/docs/en/authentication
- Claude Code status line (rate-limit fields): https://code.claude.com/docs/en/statusline
- Claude Code legal and compliance: https://code.claude.com/docs/en/legal-and-compliance
- Anthropic Consumer Terms (effective 2025-10-08): https://www.anthropic.com/legal/consumer-terms
- Anthropic Usage Policy (effective 2025-09-15): https://www.anthropic.com/legal/aup
- Codex authentication: https://learn.chatgpt.com/docs/auth
- Codex app-server protocol: https://github.com/openai/codex/tree/main/codex-rs/app-server
- Codex encrypted-content resume failure across organizations: https://community.openai.com/t/codex-app-cannot-resume-session-after-api-key-switch-encrypted-content-organization-id-mismatch/1374803
- OpenAI Terms of Use (effective 2024-12-11; openai.com refused the fetch, text from a mirror): https://open.windriver.com/info/uni-license-list/licenses/openai-tou-20241211.html
- xAI terms (effective 2026-09-11; x.ai refused the fetch, text from a tracker): https://tostracker.app/document/x
- ACP authentication and errors: https://agentclientprotocol.com
- T3 Code at `1c8e79e` (MIT): Codex shadow homes (`apps/server/src/provider/Drivers/CodexHomeLayout.ts`), session transitions (`apps/server/src/orchestration-v2/ProviderSessionTransitionPolicy.ts`), usage-limit recovery (`apps/server/src/orchestration-v2/UsageLimitRecoveryWorker.ts`): https://github.com/pingdotgg/t3code/tree/1c8e79ec18a77dd22cbd7f69e6ecc1d457c0e80d
- Two Claude Code accounts on one machine (community report on copying sessions): https://dev.to/daksh-gargas/one-brain-two-wallets-two-claude-code-accounts-on-one-machine-5ejb
