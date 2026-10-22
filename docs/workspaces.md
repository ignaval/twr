# Workspaces

A workspace is a separate twr on the same machine: its own hub, orchestrator conversation, threads, worktrees and trash. Use one per client, side project or experiment, and switch between them the way you switch tabs. `default` is the twr you already have; it stays exactly as it was.

```sh
twr ws new client-x          # create a workspace
twr -w client-x              # open the TUI on it (starts its hub)
twr -w client-x thread list  # any command, in that workspace
export TWR_WORKSPACE=client-x  # or select it for the whole shell
twr ws                       # list them
```

`-w` (`--workspace`) wins over `TWR_WORKSPACE`, which wins over `default`. `--socket` still picks the socket. A workspace that doesn't exist is an error that says how to create it, so a typo never makes a new one. Names are 1 to 32 lowercase letters, digits or `-`, starting and ending with a letter or digit.

## In the TUI

On a screen 120 columns wide or more, a sidebar on the left lists every workspace, a card each:

```
 WORKSPACES            ‹
▌_ default         ⚠1 ●2
▌  running
▌  Merged api into dev;
▌  the web tests still…

 _ client-x           ●1
   waiting on you
   I need you to approve
   the deploy of the…

 ○ scratch
   hub not running
   Done: the parser
   handles the new format.

 + New workspace
 ⚙ Settings
```

Each card has the workspace's name, its orchestrator's status (running, idle, waiting on you, failed, or `hub not running`) and the start of the orchestrator's latest reply. The working icon before the name means the orchestrator or a thread there is working: a turn, background tasks or a job. `⚠` counts the permissions and questions waiting on you, `●` the threads running, and `○` marks a workspace whose hub isn't running; its card keeps the last reply the TUI saw. The current workspace's card is highlighted.

`Ctrl-T` (or `/spaces`, or the `‹` in its header) hides the sidebar and shows it again; the TUI remembers it for the next time. Narrower than 120 columns, or with the sidebar hidden, a strip of tabs tops every view instead, once there is more than one workspace:

```
 ≡  default ⚠1 ●2 │ client-x ●1 │ scratch ○
```

The `≡` at its left shows the sidebar, or on a narrower screen opens it as a drawer over the view (so does `Ctrl-T`); `↑`/`↓` and `Enter` pick a card or a button there, and `Esc` or a click outside closes it. In the [compact layout](tui.md#on-a-phone) the workspaces are a screen of their own, `Workspaces`. Switch with:

- a click on a card or a tab;
- `Alt-1` to `Alt-9` for the workspace at that place, `Ctrl-PgUp` / `Ctrl-PgDn` for the previous and next one (these work while typing too);
- `/ws <name>`; `/ws` alone lists them. A name that doesn't exist yet asks `y` to create it.

`+ New workspace` (or `/ws new`) asks for a name and checks it the way `twr ws new` does: lowercase letters, digits and `-`, not `default` or one that exists. `Enter` creates the workspace and switches to it. `/ws new <name>` does it in one go. A TUI that a thread's agent runs creates no workspace, whichever way it's asked, and neither does `twr ws new` from an agent. `⚙ Settings` opens the [settings](tui.md#settings).

The TUI is connected to one hub at a time. Switching opens the other workspace's hub, starting it if it isn't running, and replays its log the way opening the TUI does. What you left in a workspace is kept for when you come back: the line you were typing, the selected card, pins, the view, the scroll position and the thread you were talking to. The TUI asks every running hub how it is every five seconds, with one light call each (`hub.glance`), and a stopped hub or one that didn't answer every fifteen seconds; it never starts a hub to ask it. A hub older than this TUI answers with its counts only, without its orchestrator's reply, until it's restarted. With [notifications](tui.md#notifications) on, a workspace that gets a new permission or question notifies you, naming the workspace.

A TUI opened with `--socket` shows that hub alone, without the sidebar or tabs.

## What's separate and what's shared

| | Each workspace has its own |
|---|---|
| Hub, socket and log | yes |
| Orchestrator and its conversation | yes |
| Orchestrator's [memory](orchestrator.md#memory) | yes, on every runtime; and its own working directory, so a Claude Code orchestrator's own memory directory is the workspace's too |
| Threads, watches, reports, attention | yes |
| Worktrees and trash | yes |
| Branches | new threads use `twr/@<workspace>/<name>`, so two workspaces can each have an `api` thread in one repository |
| Input history | yes |

Every workspace reads the same `config.toml`: projects, runtimes and notification settings apply to all of them. A named workspace ignores what belongs to the `default` hub: `hub.socket`, `hub.listen` and `hub.tls` (runners connect to `default`) and `[orchestrator] cwd`. The default hub's `hub.socket` can't be in the directory of the workspaces' sockets (`.../twr/ws/`). Agent installations and logins are shared, as are your repositories.

One exception for Claude Code's own memory directory: Claude Code keys memories by the git repository a working directory is in. If your `TWR_HOME` is inside a git checkout, the orchestrators of all its workspaces share that repository's memories.

An agent's own `twr` calls reach its workspace's hub: the hub sets `TWR_WORKSPACE` for everything it starts, and for a CLI you attach. The commands twr suggests, such as the TUI's `/attach` (`twr -w client-x attach api`), `twr resume api` or `twr trash list`, come with the `-w` of the workspace they're about, or `-w default` when your shell's `TWR_WORKSPACE` selects another. What an agent asks to run is shown as it asks, even when it quotes a `twr` command.

Workspaces keep their agents apart too. Every hub treats a process that descends from any twr hub, or from a `twr attach`, as an agent, also when it hides or drops the `TWR_AGENT` marker, so another workspace's agent can only read ([usage.md](usage.md#security)). Agents can't stop hubs (including `twr hub stop --all`), or create or remove workspaces, from the command line or the TUI. A thread can't be started in another workspace's worktree, whose hub would remove or move it with its own thread.

A command reaches only its workspace's hub: every hub says which workspace it serves, and a command that didn't get `--socket` refuses one that serves another (say, a hub started with `--socket` on that workspace's socket), the TUI too.

## Where they live

| | Default | With `TWR_HOME=X` |
|---|---|---|
| data of `W` | `~/.local/share/twr/workspaces/W/` | `X/data/workspaces/W/` |
| socket of `W` | `$XDG_RUNTIME_DIR/twr/ws/W.sock`, else `~/.local/state/twr/ws/W.sock` | `X/ws/W.sock` |
| config | `~/.config/twr/config.toml`, shared | `X/config.toml`, shared |

`TWR_HOME` still moves everything twr keeps, workspaces included ([configuration.md](configuration.md#a-second-instance)). Under it, workspace branches are `twr/<home-path-hash>/@W/<name>`.

## Stopping every hub

```sh
twr hub stop --all
twr hub stop --all --timeout 10  # seconds per workspace
```

This stops `default` and all named workspaces in the current home, ignoring `-w` and `TWR_WORKSPACE`, through the same shutdown as `twr -w NAME hub stop`: local runtimes stop first, while remote runners keep their threads. Each workspace prints `stopped`, `not running`, or `failed` with a reason. Any failure makes the command exit non-zero. Each stop runs concurrently with its own timeout, so a hung hub cannot delay the others. It checks each hub's workspace identity before stopping it. A hub that has claimed the workspace lock or bound its socket is waited for, including one already shutting down. A later startup, after its workspace has been checked, is outside this scan.

It covers the default hub's configured `hub.socket` (or its standard socket), and the named workspaces' standard sockets. A hub started on another explicit `--socket` is outside that set: a held workspace lock with no hub answering at its usual socket times out and reports failure. Use `twr --socket PATH hub stop` for that hub; `--socket` and `--all` cannot be combined. With `TWR_HOME=X`, only X's workspaces are covered; other homes require their own command. See [stopping and updating](usage.md#stop-the-hub-and-update-twr).

## Removing one

```sh
twr -w client-x hub stop
twr ws rm client-x
```

`twr ws rm` removes the workspace's data: its log, its orchestrator's conversation and memory, and its input history. It refuses while the workspace's hub runs (on any socket: it holds a lock on the workspace's data), while it has threads (delete them first) and while its trash holds deleted threads (`twr -w client-x trash empty --all`), whose commits stay referenced in their repositories until purged. Branches stay in your repositories, and a Claude Code orchestrator's own memory directory stays in `~/.claude/projects/`. `default` can't be removed.
