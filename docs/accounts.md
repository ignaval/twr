# Accounts

An account is one login of a runtime. You may have two Claude subscriptions, a work and a personal one, or a ChatGPT account per client for Codex. twr runs each thread on a runtime and an account, and keeps every account's login in its own config directory, the one the runtime's own variable selects: `CLAUDE_CONFIG_DIR` for Claude Code, `CODEX_HOME` for Codex, `GROK_HOME` for Grok Build. twr never reads, stores or passes a token. Logins always run through the runtime's own CLI, in your terminal.

Every runtime has one account already: its **native** account, the login it finds in its usual directory (`~/.claude`, `~/.codex`, `~/.grok`, or wherever `CLAUDE_CONFIG_DIR` and the like point in the hub's environment). It's called `default` until you name it. With only the native account, nothing in this page changes anything you see: there are no account labels, no new environment and no new prompts.

Accounts on remote boxes, balancing new threads across accounts, moving threads when an account hits its limit, and accounts for `[runtimes]` presets aren't supported yet. A thread on a remote box runs on that box's own login, so removing or logging out your native account here doesn't affect it, and `twr account list` and `twr account remove` count only this machine's threads.

## Add an account

```sh
twr account add claude work
```

1. twr checks the name and asks the hub where the account goes. Without `--dir`, it's `~/.local/share/twr/accounts/claude-code/work`, shared by every workspace.
2. If this is the runtime's first added account and the native login works, twr asks what to call the native one (`--native-name personal` answers for it). The new account may take the name the native one gives up: `twr account add claude default --native-name personal` renames the native login to `personal`, then adds `default`. twr checks both with the hub before it changes anything.
3. It makes the directory, mode 700, and links your native setup into it (`--share config`, the default): Claude Code's `settings.json`, `CLAUDE.md`, `agents/`, `commands/`, `skills/`, `output-styles/`, `hooks/`, `plugins/`; Codex's `config.toml`, `AGENTS.md`, `rules/`, `skills/`, `prompts/`, `hooks.json`, `plugins/`; Grok Build's `config.toml`, `skills/`, `trusted_folders.toml`. Only what exists is linked, and nothing in the account's directory is overwritten. `--share none` links nothing. The login, the sessions and the per-project memory always stay the account's own.

   A settings file that picks credentials isn't linked, since the runtime would use those credentials instead of the account's login, whatever twr removes from its environment: Claude Code's `settings.json` with `apiKeyHelper`, `awsAuthRefresh`, `awsCredentialExport`, `gcpAuthRefresh`, or an `env` entry for one of the variables twr removes (below) or sets; Codex's `config.toml` with a `[model_providers.<id>]` `env_key`, `experimental_bearer_token`, `auth` or `aws`; Grok Build's `config.toml` with `auth_provider_command` (under `[auth]` or `[grok_com_config]`), or a `[model.<id>]` `api_key`, `env_key` or `auth_provider`. A file twr can't read or parse isn't linked either. twr reads a settings file only if it is a regular file of at most 1 MiB, so a FIFO or a device in its place can't stall the hub. twr says which file it left out and why (`settings.json sets apiKeyHelper, which would sign every account in as the native one: left out; copy it without that key if you want it shared`) and never edits a settings file. If you want the rest of it shared, copy it into the account's directory and take those keys out.
4. It runs the runtime's login in your terminal with the account's variable set: `claude auth login`, `codex login`, `grok login`. Arguments after `--` go to it (`twr account add claude work -- --console`), and `--device` uses a device code for Codex and Grok Build. twr captures nothing; the browser, the URL and any code go between you and the runtime.
5. It records the account in the hub and checks the login: `work: logged in (claude.ai)`.

If the login fails, the directory stays and twr prints the command to finish. `twr account add --no-login` sets an account up without logging in, and `twr account login claude work` signs it in later, or again after an expiry or a logout. Like `add`, `login` refuses a directory that isn't yours or that other users can open. Neither logs in a directory that is a runtime's own directory or another account's.

The TUI can't hand its terminal to another program, so `/account add claude work` shows the command to run in another terminal.

### Declare accounts in config.toml

For dotfiles, accounts can be declared in `config.toml` instead:

```toml
[accounts.claude-code.work]
dir = "~/.claude-work"            # unset: ~/.local/share/twr/accounts/claude-code/work
# share = "config"                # config | none: what `twr account add` links into it

[accounts.claude-code.personal]
native = true                     # names the native account: the runtime's own directory, no variable set
```

The hub reads them when it starts, like every other key. `twr account add` of a declared account logs it in in the declared directory and records nothing. When an account is both declared and recorded, the config wins, and `twr account list` says where each one comes from (`config`, `twr`, `config (also recorded)`). That holds when the two disagree on its kind too, such as a recorded account that `config.toml` declares `native = true`. The account commands never write `config.toml` (the TUI's settings view writes only the one setting you change there, never an account): `twr account remove` of a declared account removes only twr's record of it, and refuses one twr has no record of, telling you to remove it there. A record removed that way stays removed, so taking the account out of `config.toml` later doesn't bring it back.

Balancing, failover, retry at reset and shared Codex sessions aren't available yet: `[account_policy.<runtime>]` takes only `default`, and `balance`, `pool`, `failover`, `avoid_above`, `move`, `resume_at_reset` or an account's `sessions` are refused when twr reads `config.toml`, with a message saying the key isn't available yet.

If `config.toml` declares the name you gave your native login as a named account, the native login is called `default` again. A native login you removed stays removed under that name too, since the removal was of the login, not of its name; only `native = true` in `config.toml` (the config wins) or `twr account name-native` brings it back. If you also have a named account called `default`, the name means that named account everywhere: `--account default`, a pin, `[account_policy.<runtime>] default` and `twr account login`.

A `dir` can't be the runtime's own directory (setting `CLAUDE_CONFIG_DIR=~/.claude` isn't the same as leaving it unset, so the hub refuses it; use `native = true`), and two accounts can't share a directory. The hub won't start with a declared account like that. Nor will it with a declared account called `default` while the native account is still called that: name the native account first, with `native = true` on another declared account, or with `twr account name-native` before you declare it.

An account added with `twr account add` can end up like that later, for example when you set `CODEX_HOME` to its directory for the hub. The hub still starts, so you can fix it: `twr info` warns, `twr account status` says why, and the account is refused (new threads, moves, and its threads' starts, attaches and jobs) until you change the variable back and restart the hub, or remove the account with `twr account remove`. A declared account can end up like that too while the hub runs, when its `dir` is a symlink you point at a runtime's own directory or another account's. It is refused the same way until you change its `dir` in `config.toml` and restart the hub. A declared account in the directory of one added with `twr account add` is refused along with that one until you remove either.

## List, check, remove

```sh
twr account list                  # every account: where it comes from, its login, threads, directory
twr account status [claude:work]  # check the logins again now; also the links, and variables twr removes
twr account remove claude work [--move-to personal | --force] [--logout] [--purge]
twr account name-native claude personal
```

A named account whose settings file is the native one (a link made before twr checked, or a native file that set such a key later) and sets one of those keys, or that twr can't read (not a regular file, or over 1 MiB), is refused for new threads, moves, and its threads' starts, attaches and jobs, with the reason; `twr account status` and `twr info` say it too. Replace the link with a copy without those keys. An account's own settings file may set them, since that may be how the account signs in, and `twr account status` notes it. It also notes a named account that signs in with an API key or a cloud provider while its settings file is the native one.

The hub checks every account's login when it starts, every 10 minutes, after an account is added and after a turn fails because its login expired. It keeps only whether it's logged in and how (`claude.ai`, `console`, `api-key`, ...): no email, organization or plan. Grok Build has no status command, so its accounts say `unknown`.

`twr account remove` with threads on the account needs `--move-to` (each thread moves, with a handoff) or `--force` (the threads keep the account and can't start until you move them). The orchestrator moves only by a switch, so `--move-to` refuses while it runs on the account: switch it first (`twr orchestrator use claude-code:personal`), then remove the account. With `--logout` or `--purge`, twr first waits (up to 10 minutes) for any runtime still running on the removed login, such as a moved thread finishing its turn, and otherwise says what to run by hand. The directory stays unless `--purge`, which deletes only a directory under twr's accounts directory, after asking. `--purge` and `--logout` refuse a directory that is a runtime's own directory or another account's, since they would delete or log out that login too. That check runs again right before the logout, so an account added back in the directory during the wait keeps its login. Account directories are shared by every workspace, so `--logout` and `--purge` also ask each other workspace's hub, before the removal and again right before each step. They refuse while another workspace has an account in the directory, or a thread whose runtime still runs in it on an account it removed, even if that account was added back somewhere else since. Threads there that only keep the removed account, with no runtime running on it, don't count, as in this workspace. If another workspace's hub isn't running, twr can't tell whether it uses the directory, so it refuses and names the workspace: start that hub (`twr -w <workspace>`) and run the command again, or remove the account there. Nothing locks the directory meanwhile, so a workspace that adds an account in it between that check and the deletion can still lose it. If the logout can't start or fails, `twr account remove` exits with an error saying the account was removed but the logout failed, and `--purge` keeps the directory. For an account config.toml also declares, only twr's record goes and the account stays, so nothing is logged out.

## Which account a thread uses

A thread's account is picked when it's created and kept. The first of these that applies:

1. The one you give: `twr thread new api --account work`, `--runtime claude-code:work`, `/new api --account work`, or the orchestrator's `spawn_thread {account}` when you ask it for one.
2. Its project's pin, in `config.toml`:

   ```toml
   [projects.acme-api]
   repo = "/home/me/code/acme/api"
   account = "work"                       # a name, a glob ("work-*") or a list, for every runtime
   accounts = { codex = "team" }          # per runtime; replaces `account` there
   exclude_accounts = ["personal"]        # never these

   [boxes.gpu.projects.api]               # a box project's keys replace its project's
   account = "work"
   ```

   A list or a glob that matches several accounts takes the first one that is logged in. A thread opened by a path instead of the project's name gets the same pin when the path is in that project's repository: the repository's own path, a symlink to it, a subdirectory or another worktree of it. Its moves keep to that pin too.
3. `[account_policy.<runtime>] default`.
4. The native account.

A pin is a promise about where a project's code goes, so twr never guesses: a pin that names an account the runtime doesn't have, or whose accounts are all logged out or excluded, refuses the thread with what to do, such as `project acme-api pins account "work", which codex doesn't have: add it (twr account add codex work), or change the pin ([projects.acme-api] accounts.codex = "<account>")`. The orchestrator can never put a thread outside its pin. You can, with `--outside-pin`, and the thread records it.

The orchestrator's own account is `[orchestrator] account`, else the policy default, else the native one. `[orchestrator] account` takes a name, a glob or a list, like a project's `account`, and the orchestrator gets the first of them that can be used. `twr orchestrator use claude-code:work` switches it, with a handoff, and a reset keeps it. If the configured account can't be used (unknown, removed or logged out), twr doesn't make the orchestrator, and says why when you talk to it. It never runs it on the native account instead. Fix the account, or pick another with `twr orchestrator use`.

## Moving a thread

```sh
twr model api --account personal          # or --runtime claude-code:personal, or /account api personal
```

A session can't move between accounts, so a move starts a new session on the new account with twr's handoff, as a move to another runtime does: the thread keeps its worktree, branch, queued prompts, model and effort, and its next turn starts the new session. A running turn finishes first. Moving a thread back to an account it left (on the same runtime, or with the runtime) resumes the session it left there, told what the thread did since, as a move back to a runtime does: only that account's own session, while the account still has the directory the session began in, reaches it by the same path (a runtime may tie a login to the path, so two symlinks to one directory count as two logins), and wasn't removed since. Otherwise, such as after `twr account remove` and `add` again, a new `dir` or a `dir` written through another symlink, the move starts a new session with the full handoff. The move's message says which. twr checks again when the next turn starts: if the account was removed (and maybe added again) after the move, that turn starts a new session with the full handoff instead, and `twr events --pretty` shows why on the turn. A move back between accounts of one runtime keeps the thread's current model and effort, and the resumed session runs with them; a move back with the runtime restores the ones the session had, unless you give others. If the thread's current model or effort is the runtime's default and the runtime keeps a resumed session's own value (Codex and Grok Build keep both, Claude Code its model), the move starts a new session instead, since the session would go on with its old value. twr never resumes one account's session on another account. Pins apply to moves too. A list or a glob (`--account work,personal`) picks its first usable account, as for a new thread, so a thread on `personal` moves to `work` when `work` can be used, and stays where it is only when the account picked is the one it already runs on.

If an account's directory changed since a thread's session began (you edited `config.toml`, removed the account and added it again elsewhere, or pointed the symlink it is at somewhere else), the thread refuses to start until you move it: `twr model api --account work` starts a new session there. The same goes for a named account whose directory is gone: its threads, attaches and jobs are refused until `twr account add <runtime> <name>` makes the directory again and logs it in. A thread keeps the path its account's directory was given by when it got the account, since a runtime can tie a login to that path (Claude Code's macOS Keychain entry is). If that path stops leading to the directory, say a symlink was removed and `config.toml` now reaches the directory through another one, the thread refuses to start rather than go on through the other path. `twr model api --account work` then starts a new session through the configured path. A queued job is checked again just before it starts, and once more as its command is let go: if its thread moved to another account, or its account was removed, while it was starting, it fails without running, and you run it again.

If you configure an account's directory by another path to the same place (a symlink), threads already on it keep the path their session began with, since a runtime may key its login on the path (Claude Code's macOS Keychain entry). `twr account list --json` lists them as `retained_paths`, `twr account login` names them, and `twr model <thread> --account <same name>` moves one to the configured path with a new session.

## What you see

A runtime shows **account labels** while it has two or more accounts that aren't removed and aren't logged out. Then its threads say `claude-code:work` (`claude-code:work@gpu` on a box) on cards, tiles, the dive header, the orchestrator's header, `/model`, completions, `twr thread list`, `twr model`, `twr info` and `twr orchestrator show`. With one account, nothing changes. Two Claude accounts and one Codex account means Claude threads show their account and Codex threads don't. Labels appear and disappear at once when an account is added, removed or logs in or out. The usage line in the TUI, `twr usage` and the orchestrator's `get_usage` follow the same rule: `Claude:work`, and the native account under its name, such as `Claude:personal`.

Some things name the account whatever the rule says: a move (`account work -> personal`), a thread on a removed account (`claude-code:work (removed)`), `twr account list`, `twr events --pretty` (it prints history, and every session's start records its account), and the cards below.

The socket API (`thread.list`, `thread.get`, `--json`) always has `account` and `account_shown`. The orchestrator sees `account` only on a runtime that shows labels, and its `list_accounts` tool lists the accounts without their directories.

## Limits and sign-in

When a turn ends on a usage or spending limit, twr doesn't retry it, and the error card says which account and when it resets: `api: claude-code:work hit its weekly limit · resets Thu 05:00 (in 13h)`. A named account is always named on that card. A turn that fails because an account's login expired says how to sign in again, `(sign in again: twr account login claude-code work)`, and twr checks that account's login again. The orchestrator's failed turns say the same on its error card, and are never retried either.

## The environment

A thread on a named account gets the runtime's variable set to the account's directory, after `[threads] env` and `[projects.<name>] env`, so the account wins over a project env that sets the same variable. Claude Code also gets `ANTHROPIC_CONFIG_DIR=<dir>/anthropic`, so a Console or workload-identity profile stays with its account. twr removes the variables that would override the account's login: `ANTHROPIC_API_KEY`, `ANTHROPIC_AUTH_TOKEN`, `CLAUDE_CODE_OAUTH_TOKEN` and Claude Code's other credential variables, `OPENAI_API_KEY`, `CODEX_API_KEY`, `CODEX_ACCESS_TOKEN`, `XAI_API_KEY` and `GROK_AUTH_PROVIDER_COMMAND`. The same goes for `twr attach`, the orchestrator and a thread's jobs. The native account keeps your environment as it is.

The native account's directory is the one the hub's environment selects: `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `GROK_HOME` as the hub had it when it started, else `~/.claude`, `~/.codex`, `~/.grok`. `twr account` follows the hub, not the shell you run it from. `twr account login` of the native account runs the login with the hub's value of that variable, or with the variable removed when the hub doesn't have it set, so an export in your shell can't sign a different directory in. `twr account add` links from the hub's native directory, `twr account list` shows it, and the checks that an account's directory isn't a runtime's own use the hub's directories. When your shell selects a different directory, twr prints a note naming the hub's. An older hub doesn't report its directories, so with one twr uses your shell's.

Next-message suggestions in the TUI ([tui.md](tui.md#suggestions)) always run on each runtime's native account, whatever account the thread runs on. twr sets no account variable for them, and a Codex suggestion uses the native Codex directory above, found as Codex finds it (`CODEX_HOME`, else `.codex` in your home). They count toward the native account's usage, and a named account's limit or logout doesn't stop them. A Codex thread in `auto` whose reviewer is `auto_review` (the default) gets its reviews from its own Codex process, so they run on the thread's account.

A Claude Code orchestrator on a named account that shares `config` keeps its memories with the native account's: twr points Claude Code's `autoMemoryDirectory` at the native memory directory, and the memory guard allows exactly that directory. Server-managed settings (`remote-settings.json`) are read from the account's own directory.

Grok Build reads `~/.claude.json` and `~/.claude/skills` whatever `GROK_HOME` says, so every Grok account sees your native Claude Code MCP servers and skills.

## Older versions of twr

Once you add an account, an older twr refuses this database. A twr from before accounts would ignore them and start every thread on the native login, so the hub marks its database as newer (schema version 10) the first time it records an account, a login check (`twr account status`, the native account's too), or a thread on a named account. An older hub then stops with `database schema version 10 is newer than this twr (9)`. Removing the accounts doesn't undo this. A database where none of that happened stays at version 9, and an older twr still opens it. Accounts declared only in `config.toml` mark it too, once a thread runs on one of them.

## Providers' terms

Providers set usage limits per subscription. Holding several subscriptions, such as one your employer pays for and one of your own, is ordinary. Moving work between your accounts to keep going past a limit may conflict with their terms: Anthropic's limits "assume ordinary, individual usage", and OpenAI's terms rule out circumventing rate limits. Check the terms of each subscription, especially accounts your employer pays for. twr doesn't decide this for you. That's why twr has no automatic balancing or failover, and why later versions will keep them opt-in, with this note next to the settings.

See also [configuration.md](configuration.md#notes-on-some-keys) and the design, [design/multi-account.md](design/multi-account.md).
