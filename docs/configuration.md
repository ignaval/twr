# Configuration

twr reads `~/.config/twr/config.toml`, or `$XDG_CONFIG_HOME/twr/config.toml`, or `$TWR_HOME/config.toml` when `TWR_HOME` is set. Every key is optional and a missing file means all defaults. An unknown key is an error, so a typo doesn't go unnoticed. The hub reads the file when it starts: restart it after a change.

`twr info` shows what the running hub uses: its boxes and the orchestrator runtime. It also lists warnings about the config the hub started with, such as a short box token or a config file other users can access.

## Workspaces

[Workspaces](workspaces.md) (`twr ws new <name>`, then `twr -w <name>` or `TWR_WORKSPACE`) are separate hubs on one machine that share this file. A named workspace ignores what belongs to the `default` workspace's hub: `hub.socket` (it has its own socket), `hub.listen` and `hub.tls` (runners connect to `default`), and `[orchestrator] cwd` (its orchestrator works in the workspace's own directory, so a Claude Code orchestrator keeps its own memories). The default workspace refuses a `hub.socket` in the directory of the workspaces' sockets (`<socket dir>/ws/`), where it could be one of theirs; `--socket` still works. Restart a workspace's hub (`twr -w <name> hub stop`, then `twr -w <name>`) after a change, like any hub.

## A second instance

For another twr on the same machine, a [workspace](workspaces.md) is usually what you want: it shares this config. `TWR_HOME` moves everything twr keeps, its workspaces included, to another directory with a config of its own, as the tests and the screenshot script do. Run another independent twr with its own home:

```sh
TWR_HOME=~/.twr-work twr
```

The TUI header shows `[.twr-work]` (the resolved home directory's name); the usual instance shows
`[default]`. The label comes from the connected hub, including when you pass `--socket`.
`twr info` shows the same instance name.

| Location | With `TWR_HOME=~/.twr-work` |
| --- | --- |
| Config | `~/.twr-work/config.toml` |
| Hub socket | `~/.twr-work/hub.sock` |
| State and event history | `~/.twr-work/data/twr.db` |
| Worktrees | `~/.twr-work/data/worktrees/` |
| Trash | `~/.twr-work/data/trash/` |
| Hub log | `~/.twr-work/data/hub.log` |

There is no fallback to `~/.config/twr/config.toml`: a missing instance config uses defaults.
Create the directory and write `config.toml` there if you need projects or runtime settings;
you can also copy your usual config. When copying, remove `hub.socket` or give it a unique
path, and remove or change `hub.listen` if another hub already uses its port. `--socket` and
`hub.socket` explicitly override the instance's default socket.

Use the same home for every command targeting that instance, or export it in its terminal:

```sh
export TWR_HOME="$HOME/.twr-work"
twr
twr orchestrator reset
twr hub stop
```

Local worktree directories are separate. New worktree branches in an isolated instance use
`twr/<home-path-hash>/<thread-name>`, so both instances can have `api` threads in the same
repository. Existing threads keep their recorded branches, including through park, resume
and trash restore. This changes new branch names for existing `TWR_HOME` users too: a new
thread looks for its namespaced branch, rather than reusing an old `twr/<name>` branch.
`--no-worktree` and `threads.worktree = false` still share the repository
checkout by choice. Agent installations and their login credentials are shared. A runner
box shared between hubs also shares its remote worktree and branch layout. Remote branches
retain `twr/<name>`: use separate runner homes and repository clones (or separate machines)
for full remote isolation ([runners.md](runners.md)).

Restart each existing hub after upgrading to get the new commands and instance label; no
config migration is needed. Restart an instance after editing its hub config.

## Reference

The values below are the defaults, except where a comment says "example".

```toml
default_runtime = "claude-code"   # claude-code | codex | grok-build | acp:<command> | a [runtimes] name | pty:<command>
stall_after_secs = 600            # foreground/background work silent this long raises stalled; 0 = off
background_timeout_secs = 86400    # stop background-only work without task liveness for 24h; 0 = off
# Where a project given by its folder name is looked up (see below). Unset = these folders;
# [] = no lookup.
# project_roots = ["~/code", "~/src", "~/projects", "~/dev", "~/Developer", "~/repos", "~/git", "~/workspace"]

[orchestrator]
# effort = "high"                  # runtime reasoning effort (unset: runtime default)
# runtime = "claude-code"         # the first orchestrator; unset = default_runtime
# model = "haiku"                 # unset = [threads] model, else the runtime's default
# cwd = "/home/me"                # its working directory; unset = your home directory
autostart = false                 # start it with the hub instead of on your first message
project_reads = true              # it may read your projects' committed files (false: never)
# read_deny = ["*.sqlite", "config/prod/**"]  # more files it may never read, over the built-in secret names

[threads]
default_mode = "auto"             # permission mode for new threads; checked when the hub starts
# model = "sonnet"                # default model for new threads; unset = the runtime's default
thinking = false                  # log agent.thinking events
worktree = true                   # false: threads work in the repository checkout, shared
park_disposable = ["target/", "node_modules/", "dist/", "build/", ".venv/", "venv/",
  "__pycache__/", ".next/", ".cache/", ".gradle/", ".pytest_cache/", ".mypy_cache/",
  ".tox/", "coverage/", ".DS_Store"]
idle_trim_disposable = ["target/", "node_modules/", "__pycache__/", ".pytest_cache/",
  ".mypy_cache/", ".tox/", ".gradle/", ".next/"]  # what an idle trim may delete, of park_disposable
# env = { CARGO_INCREMENTAL = "0" } # added to every worker thread's runtime environment
disk_warn_gb = 10                 # under this many GB free on a box, a disk card; 0 = never
job_slots = 0                     # the machine's job slots for `twr job run`; 0 = its CPUs
# cpu_weight = 20                 # the workers' CPU weight against the rest (systemd: 100); unset = no limit
# memory_high = "8G"              # each worker's memory before it's slowed (systemd MemoryHigh); unset = none

[trash]
keep_days = 7                     # deleted threads stay in the trash this long; 0 = purge at the next check

[projects.api]                    # example
repo = "/home/me/code/api"        # the repository
# runtime = "codex"               # runtime for this project's threads; unset = default_runtime
# box = "gpu"                     # run this project's threads on a remote box by default
# worktree = false                # overrides [threads] worktree for this project
# env = { CARGO_TARGET_DIR = "/home/me/.cache/api-target" }  # over [threads] env, for this project
# base = "dev"                    # the branch its threads merge into: ahead/behind and conflicts on their cards
# trim_idle_after = "24h"         # trim the build output of its threads idle this long (90m, 24h, 2d)
# idle_trim_disposable = ["target/"]  # replaces [threads] idle_trim_disposable for this project

[notify]                          # the hub runs a command when something needs you; see notifications.md
# command = ["notify-send", "--app-name=twr", "{title}", "{body}"]  # unset = off; no shell; {title} {body} {workspace}
events = ["permission", "question", "confirm", "error", "review", "report", "reply"]  # add "stalled" for stalls
detail = false                    # true: what's asked or reported goes along (it may hold secrets)
timeout_secs = 30                 # the command is ended after this long (1 to 600)

[tui]
mouse = true                      # the TUI selects and copies text; false = the terminal's own selection
animate = true                    # the working icon moves; false = a still _ marker
card_order = "stable"             # "attention": the most recently active thread first, every move at once
compact_width = 60                # narrower, one screen at a time (a phone); 0 = never

[tui.notify]
terminal = "off"                  # bell | osc9 | osc777: alert the terminal when something needs you
when = "unfocused"                # always: also while the terminal has focus
events = ["permission", "question", "confirm", "error", "review", "report", "reply"]  # add "stalled" for stalls

[runtimes.gemini]                 # example: an ACP agent under a short name (--runtime gemini)
acp = "gemini --experimental-acp"
attach = "gemini --resume {session}"  # optional: what `twr attach` runs; also takes {mode}

[hub]
# socket = "/run/user/1000/twr/hub.sock"  # unset = the standard location
# listen = "127.0.0.1:7878"       # WebSocket for runners; off when unset
# A non-loopback listen address needs one of these:
# tls = { cert = "hub.pem", key = "hub.key" }   # serve wss:// (PEM files)
allow_insecure_remote = false     # true: plain ws:// on the network (events in clear text)

[boxes.gpu]                       # example: a box the hub reaches over SSH; see runners.md
transport = "ssh"                 # the hub connects to it; unset = "token" (its runner connects to the hub)
ssh = "me@gpu-box"                # where; also used by `twr attach` for threads on that box
# build = "0123456789abcdef"      # the twr build `twr box install` put on a box of another OS or CPU

[boxes.gpu.projects.api]
repo = "/srv/code/api"            # where that project lives on the box
# runtime = "codex"               # runtime for this project's threads on that box

[boxes.lab]                       # example: a box whose runner connects to the hub
token = "..."                     # the runner presents this; never logged; `twr runner token` makes one
ssh = "me@lab-box"                # used by `twr attach` for threads on that box
```

## Notes on some keys

`[orchestrator] project_reads` (default `true`) lets the orchestrator read your projects' committed files, and `read_deny` lists more file names it may never read, as globs: `*` within a path part, `**` across parts, `?` one character. A pattern without `/` matches a file name, `*.sqlite`, and one with `/` a path from the repository's top, `config/prod/**`. They add to the built-in secret names and never remove one. See [Reading your projects](orchestrator.md#reading-your-projects).

`default_runtime`, `[projects.<name>] runtime` and `[orchestrator] runtime` take the same values as `twr thread new --runtime`. A thread's runtime is the one given on the command line, else its project's, else `default_runtime`. See [runtimes.md](runtimes.md).

`project_roots` lists the folders where `twr thread new <name>`, the TUI's `/new <name>` and the orchestrator look for a project given by its folder name, when that name isn't a `[projects]` entry. Like `default_runtime`, it's a top-level key, so it goes above the first `[section]`. A leading `~` is your home directory, and every other entry must be an absolute path. twr looks for a git repository, a folder with `.git` at its top, at `<root>/<name>` and one level deeper, at `<root>/<group>/<name>`, as in `~/code/acme/api`. The middle folder can't be a git repository itself, so nothing inside a repository is found, and hidden folders are skipped. An exact name match wins. Only when there's none does a match that ignores case count. Exactly one match is used as if you had given its absolute path. Several matches, or none, are an error that lists the candidates or the folders searched. Folders that aren't git repositories are never used. The search reads at most 2048 folders. Unset, it uses the folders in the reference above that exist. `~/Documents/GitHub` isn't among them, because reading `~/Documents` makes macOS ask for permission, and neither is `~/work`, which often holds documents rather than code; add them if you keep repositories there. `project_roots = []` turns the lookup off. The lookup only searches the hub's own machine: a thread on a remote box (`--box`) needs a configured project or an absolute path on that box.

`[orchestrator] effort` sets the first orchestrator’s reasoning effort, validated against its runtime. Unset uses the runtime default.

`[orchestrator]` only picks the first orchestrator. After `twr orchestrator use`, the choice is kept in the event log and survives hub restarts. See [orchestrator.md](orchestrator.md).

When `[orchestrator] runtime` isn't set and no orchestrator exists yet, the TUI asks which agent should run it the first time it opens. See [tui.md](tui.md#choosing-the-orchestrator).

The TUI reads `[tui]` when it opens; the hub ignores it. With `mouse = true` the TUI captures the mouse: a drag copies text from one pane, without borders, and the wheel scrolls. Shift-drag, or Option-drag in iTerm2, still uses the terminal's own selection. `/mouse` toggles it while the TUI runs. See [tui.md](tui.md#mouse).

`animate = true` animates the working cursor on whatever is running a turn: worker cards, grid tiles, and the main pane's header while the orchestrator or the thread you dived into works, plus the cursor where that thread's next text will appear. The TUI then redraws about eight times a second, but only while an icon is on screen. With `false` the same places show a still `_` and the TUI redraws only when something changes. See [tui.md](tui.md#what-is-working).

`card_order = "stable"`, the default, keeps the cards in place: the cards that need you go by their items (most urgent, then oldest, first), and within the other groups (working, failed, done, idle, parked) cards stay in the order their threads were created, a card that comes to need you goes to the top at once, and any other move waits a few seconds and until no card is selected, no popup is open and the mouse isn't over the cards. `"attention"` is the older order: within a group, the most recently active thread first, and every move right away. Cards have a fixed height with either. See [tui.md](tui.md#stack-view).

`[notify]` has the hub run a command of yours when something needs you, with or without a TUI open: a push to your phone through ntfy, a Slack message, a desktop notification. It's off until `command` is set. The command gets the title and body as arguments (`{title}`, `{body}`, `{workspace}`) and the details as JSON on stdin. Every workspace's hub runs it, and nothing goes out while you use a TUI. See [notifications.md](notifications.md) for the rules and recipes.

`compact_width` is the width, in columns, below which the TUI shows one screen at a time: the conversation, what needs you, or the threads, switched with `/c`, `/n` and `/t`. It's meant for a phone over SSH or mosh. 0 turns it off; `/compact on|off` overrides it while the TUI runs. See [tui.md](tui.md#on-a-phone).

`[tui.notify]` has the TUI alert its terminal, with a bell or a desktop notification, when something needs you and the terminal isn't focused. The TUI reads it from the config of the machine it runs on. See [tui.md](tui.md#notifications).

`default_mode` is passed to the runtime as its permission mode. The default, `auto`, is the agent's own auto mode: the agent decides which tool calls need asking, and whatever it asks comes to you as a permission card. twr itself still never answers a prompt. Set `manual` to have every edit and command that needs approval ask you. It isn't free text. The hub checks it when it starts and keeps it in its canonical spelling. An unknown mode is a config error and the hub doesn't start. Creating a thread on a runtime that can't take the mode fails. `twr mode <thread> <mode>` changes one thread's mode later. The orchestrator never runs in `auto`, whatever this says: it only asks. [runtimes.md](runtimes.md#permission-modes) lists the modes and how each runtime maps them.

`park_disposable` lists ignored paths that parking may delete along with a clean worktree. `name/` matches any directory with that name, and `name` matches any path component. Any other ignored file keeps the worktree. On Linux and macOS, deleting a thread deletes the same entries (its build output) instead of moving them to the trash, unless one holds a git repository or might ([usage.md](usage.md#delete-threads)). On other systems they go to the trash with the rest. `twr trim` deletes them too. See [worktrees.md](worktrees.md).

`idle_trim_disposable` narrows what an idle trim (`[projects.<name>] trim_idle_after`) deletes, in the same syntax: an ignored entry goes only when one of its path components is named by both lists. A `build/` that `park_disposable` names, inside a directory only `idle_trim_disposable` names, stays. The default keeps to what a build always makes again, so an unattended trim leaves `dist/`, `build/`, `.cache/`, `.venv/` and `coverage/` alone. `twr trim`, the disk card, parks and deletions ignore it. `[projects.<name>] idle_trim_disposable` replaces it for one project, and `[]` makes that project's idle trims delete nothing. An entry that can't name a file or directory (empty, `..`, a `/` inside) is a config error. See [worktrees.md](worktrees.md#disk-use).

`[threads] env` sets environment variables for the runtime of every worker thread (not the orchestrator), and `[projects.<name>] env` sets them for that project's threads, over `[threads] env`. A project's threads are the ones made with its name. A thread made from a repository path is not one of them, even when its folder has the same name. The agent's processes inherit them, so its builds and tests do too. They're read when the hub starts and applied each time a thread's runtime starts, existing threads included. There's one exception: a thread made before twr began recording which project a thread was made for gets `[threads] env` but not its project's `env`, until it's made again. Values are taken as they are, without `~` or `$VAR` expansion. Names starting with `TWR_` are twr's own and are refused. Build settings are the typical use, such as `CARGO_INCREMENTAL = "0"`, or a `CARGO_TARGET_DIR` shared by a project's threads (see [development.md](development.md#disk-usage) for the trade-offs), or a make jobserver every thread's builds share ([recipes.md](recipes.md#a-shared-jobserver-for-make-and-cargo)). A thread on a remote box gets the same variables there.

`job_slots` is how many job slots the machine has: every job (`twr job run`) takes as many as it asks for (1 by default) while it runs, and waits for them in turn. Every twr hub of this home (its workspaces too) shares the pool, a lock file per slot in `<data dir>/job-slots/`. 0, the default, is the machine's CPUs. See [usage.md](usage.md#jobs).

`cpu_weight` and `memory_high` limit the workers on the hub's machine. Both are off by default. See [usage.md](usage.md#resource-limits).

`disk_warn_gb` is the free disk, in GB, under which the hub raises a disk card for a box: how much is left where the box keeps its worktrees, and the idle threads whose build output `Trim` would delete. The TUI's status bar says so too while it lasts, and `twr box doctor` warns under it. 0 turns them off. See [worktrees.md](worktrees.md#disk-use).

`[trash] keep_days` is how long a deleted thread's worktree, record and branch stay in the trash of its box before the hub purges them. The hub checks when it starts and every hour, on every box that is online; a box that is offline is purged at the first check after it reconnects. `twr trash empty` purges sooner, and `twr trash restore` brings a thread back until then. See [usage.md](usage.md#trash).

A `[runtimes.<name>]` name is 1 to 64 letters, digits, `-`, `_` or `.`, and can't be a built-in runtime name: `claude-code`, `codex`, `grok-build`, `acp`, `pty` or `fake`. `acp` is required. `attach` must contain `{session}`, which becomes the session id. `{mode}` becomes the thread's permission mode. twr refuses to attach a thread in a restricted mode, such as `plan`, through a template without `{mode}`, since the agent's CLI would then start in its own wider default mode. twr resolves the preset when the thread is created, so a later config change doesn't change an existing thread.

`[boxes.<name>] token` must be at least 16 characters long. The hub refuses to start with a shorter one, and warns about one shorter than 32 in its log and in `twr info`. Other commands still load such a config, so after an upgrade `twr hub stop` still stops the old hub. To fix it, put a new token from `twr runner token` (64 characters) in the config and in the runner's token file, restart the hub (`twr hub stop`, then `twr`) and the runner. A config that holds a box token or a `[notify]` command holds a secret: keep it private with `chmod 600`. The hub warns at start, in its log and in `twr info`, when other users can access it (any group or other permission bit). See [runners.md](runners.md#set-it-up).

`[boxes.<name>] ssh` is a destination `ssh` understands: `user@host` or a host from `~/.ssh/config`. Extra `ssh` options may come before it (or after it), separated by spaces, as in `-p 2222 me@host`. They may set how ssh reaches the box: `-p`, `-i`, `-J`, `-F`, `-l`, `-4`, `-6`, `-C`, `-q`, `-v`, `-e`, `-b`, `-c`, `-m`, `-B`, `-I`, `-P`, and `-o` options such as `Port`, `IdentityFile`, `ProxyCommand` or `ProxyJump`. They may not forward anything, run a local command, share a connection or change what ssh runs or how, which twr decides (every ssh twr runs for a box, `twr attach`'s included, turns forwarding off; [security-model.md](security-model.md#ssh-boxes)): `-A`, `-X`, `-Y`, `-K`, `-L`, `-R`, `-D`, `-W`, `-w`, `-M`, `-S`, `-O`, `-N`, `-s`, `-f`, `-t`, `-T`, `-n`, `-G`, `-Q`, `-V`, `-E`, `-y`, and `-o` with `ForwardAgent`, `ForwardX11`, `ForwardX11Trusted`, `GSSAPIDelegateCredentials`, `LocalForward`, `RemoteForward`, `DynamicForward`, `ClearAllForwardings`, `Tunnel`, `TunnelDevice`, `PermitLocalCommand`, `LocalCommand`, `BatchMode`, `StdinNull`, `RequestTTY`, `RemoteCommand`, `SessionType`, `ForkAfterAuthentication`, `ControlMaster`, `ControlPath`, `ControlPersist`, `Include`, `Match` or `Host` (in any case). Neither may `--` or a second word that isn't an option, which ssh would run on the box. twr reads the options as ssh does (`-qA` is `-q -A`, `-oForwardAgent=yes` is `-o ForwardAgent=yes`, `-o =ControlMaster=yes` sets `ControlMaster`), and refuses an `-o` whose option name it can't read. An SSH box with such a value is refused wherever the config is read, `twr box add` included; a token box's (only `twr attach` uses it) is refused when the hub starts or adds the box.

`[boxes.<name>] transport` says how the hub and the box find each other. `"ssh"`: the hub connects to the box over ssh, installs twr there and runs a box daemon ([SSH boxes](runners.md#ssh-boxes)); such a box needs `ssh` and has no `token`. `"token"`, or no `transport`: the box's runner connects to the hub's listener with the box's `token` ([Boxes the hub can't reach over SSH](runners.md#boxes-the-hub-cant-reach-over-ssh)). `[boxes.<name>] build`, on SSH boxes only, is the build id (16 lowercase hex digits) of the twr that `twr box install` put on a box of another OS or CPU, which the hub runs there; a box of the hub's own OS and CPU runs the hub's build whatever it says.

### boxes.toml

`twr box add`, `twr box install` and `twr box remove` write `boxes.toml`, next to `config.toml`, never `config.toml` itself. It holds `[boxes.<name>]` tables only, and every command and hub reads it together with `config.toml`, as if its boxes were there. A box named in both files is an error. twr rewrites the file whole, keeping every table and key but not comments, and takes a lock (`boxes.toml.lock`) while it reads and writes it, so two commands at once don't lose a change. You may edit it, for example to give a box `[boxes.<name>.projects.<project>]` entries; a box you configure by hand can just as well live in `config.toml`. A hub reads both files when it starts; `twr box add` adds a box to a running hub, and any other change to a box needs a hub restart.

## Environment variables

| Variable | Effect |
|---|---|
| `TWR_HOME` | put the config, the data directory and the socket under this directory |
| `TWR_WORKSPACE` | the [workspace](workspaces.md) commands run in when `-w` doesn't say (default: `default`); a workspace's hub sets it for what it starts |
| `TWR_RUNNER_TOKEN` | the runner's token when `twr runner` has no `--token-file` |
| `TWR_CLAUDE_BIN`, `TWR_CODEX_BIN`, `TWR_GROK_BIN` | the agent binary to run instead of `claude`, `codex` or `grok` on `PATH` (on a remote box, the login shell's `PATH` and then the runner's: [runners.md](runners.md#the-login-shells-path)) |
| `TWR_AGENT_PROBE` | `off` keeps the hub from checking its own box's agent CLIs: no `claude`, `codex` or `grok` runs for it, and `hub.info` reports the local box's agents as not reported (an empty list). Any other value, or none, leaves the check on. For tests and troubleshooting |
| `TWR_BOX_PROBE` | in a runner's environment (`twr runner`, an SSH box's daemon): `off` keeps it from asking your login shell for its `PATH` (the runtimes keep the runner's own) and from checking its agent CLIs (it reports none to the hub); a remote attach (`twr runner --exec`), `twr box login` and `twr box doctor`'s look at the box's agents then skip the login shell too, so they find the agent the runner does. Any other value, or none, leaves both on. For tests and troubleshooting |
| `TWR_SSH` | the ssh program the hub and `twr box` run for SSH boxes, instead of `ssh` on `PATH` (it gets the same arguments) |
| `TWR_BOX_LAUNCHER` | on an SSH box, in the environment ssh commands get there: `setsid` makes the relay start the box daemon in a new session, never as a systemd user service. For tests and troubleshooting |
| `VISUAL`, `EDITOR` | the editor for `twr context edit` |
| `RUST_LOG` | the log filter of `twr hub` and `twr runner` (default `info`) and `twr mcp` (default `warn`), e.g. `debug` or `twr_hub=debug`. They log to stderr; a hub that `twr` starts in the background logs to `hub.log` in the data directory |

## Standing context

Per-project context lives in the repository, not in the config: `<repo>/.twr/context.md`. See [usage.md](usage.md#standing-context).
