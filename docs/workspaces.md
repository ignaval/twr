# Workspaces

Workspaces run separate hubs on one machine. Each workspace has its own hub, orchestrator conversation, threads, worktrees and trash. Use one per client, side project or experiment, and switch between them the way you switch tabs. `default` is the twr you already have; it stays exactly as it was until you rename or remove it. Workspaces share the machine's runner service and paired machine trust; each keeps its own key and worker data on a linked runner.

```sh
twr ws new client-x          # create a workspace
twr -w client-x              # open the TUI on it (starts its hub)
twr -w client-x thread list  # any command, in that workspace
export TWR_WORKSPACE=client-x  # or select it for the whole shell
twr ws                       # list them
```

`twr ws` asks every running hub at once how many threads it has, how many run and how many requests wait on you; `twr ws list --json` prints the same as JSON, and `--json --detail` adds each hub's whole thread list and requests, which is how the desktop app reads every workspace in one go ([SKILL.md](../SKILL.md#cli-equivalents) has the fields).

`-w` (`--workspace`) wins over `TWR_WORKSPACE`, which wins over the [default workspace](#the-default-workspace) (`default`, unless you chose another). `--socket` still picks the socket. A workspace that doesn't exist is an error that says how to create it, so a typo never makes a new one. Names are 1 to 32 lowercase letters, digits or `-`, starting and ending with a letter or digit.

## In the TUI

Threads, the sidebar on the left, lists every workspace, a card each, with its threads under it. The current workspace's worker cards are nested under its card; each other workspace lists its threads, a row each:

```
 THREADS                            ‹
▌_ default                      ⚠1 ●2
▌  working · waiting on you
▌ ┌ beta ─────── sonnet-5-5 ─ manual ┐
▌ │waiting                           │
▌ │ PERMISSION  Bash: rm -rf target  │
▌ │1 allow  2 deny                   │
▌ └──────────────────────────────────┘
▌ ┌ alpha ────── sonnet-5-5 ─ manual ┐
▌ │_ running                         │
▌ └──────────────────────────────────┘

 _ client-x                     ⚠1 ●1
   working · waiting on you
   I need you to approve the deploy of
   billing.
   ├ deploy · waiting on you
   ├ billing · running
   └ docs · done

 ○ scratch
   hub not running

 + New workspace
 ⚙ Settings
```

Each card has the workspace's name, its activity and the start of the orchestrator's latest reply. It says `working` when a worker is running, even if the orchestrator is idle; permissions and questions still say `waiting on you`, and an orchestrator's failure still says `failed`, alongside `working` when other work continues. The working icon before the name means the orchestrator or a thread there is working: a turn, background tasks or a job of its own (not a `!` command you run). `⚠` counts the permissions and questions waiting on you, `●` the threads running, and `○` marks a workspace whose hub isn't running; its card says `hub not running` and keeps the last reply the TUI saw. Cards fit their content: status text wraps, the reply takes up to two rows, and a missing reply leaves no empty preview rows. The current workspace's card is highlighted. Another workspace's threads are as its hub last said them, those waiting on you first, up to six rows (`└ +3 more` for the rest); parked ones aren't listed, and none are while its hub isn't running. When the current workspace's cards need the room, the other cards get shorter, down to their names; where even those don't all fit, `↑ more` and `↓ more` stand for the rest, and a tap on one brings the next into view. In the grid and the decision table the cards aren't in the sidebar, and the workspaces' cards scroll there with the wheel.

It is the TUI's only sidebar, narrower below 100 columns. `Ctrl-B` (or `Ctrl-T`, `/sidebar`, `/spaces`, or the `‹` in its header) hides it whole, the workspaces and the cards nested in it, and shows it again; the TUI remembers it for the next time. Hidden, a strip of tabs tops every view instead, once there is more than one workspace:

```
 ≡  default ⚠1 ●2 │ client-x ●1 │ scratch ○
```

The `≡` at its left shows the sidebar again, as `Ctrl-B` does.

Each workspace card ends its name row with `⋯`. A click or tap on it opens that workspace's menu, `/ws menu [name]` too: `Open`, `Rename…`, `Make default` and `Delete…` (`o`, `r`, `m`, `d`). On a phone each choice, and the `⋯` with the row under it, is two rows tall. `Rename…` asks for the new name and checks it as `twr ws rename` does; `/ws rename [name] <new>` opens it with the name typed. `Delete…` (`/ws rm <name>`) first finds out what deleting would remove, then shows it under `Delete <name>` and `Cancel`: a long summary scrolls (wheel, `PgUp`/`PgDn`) and the two choices stay on screen. It opens on `Cancel`, so `Enter` alone never deletes; `y`, `Enter` on `Delete` or a second tap on it deletes, once the summary has been on screen a moment. A workspace with threads, a trash that isn't empty or an orchestrator at work says why it can't be deleted and how to clear it, with only `Close`. A running hub with none of those is stopped first, and the summary says so. Deleting the workspace the TUI shows switches to the default one (or the next one) first; the only workspace there is can't be deleted from the TUI, since it would have none left to show. A TUI that a thread's agent runs opens no menu. In the [compact layout](tui.md#on-a-phone) the workspaces are on its `Threads` screen, as in the sidebar, the current one's thread cards nested under its card, in a list that scrolls, each workspace and thread entry on it two rows tall. Switch with:

- a click on a card, a thread under another workspace's card, or a tab;
- `Alt-1` to `Alt-9` for the workspace at that place, `Ctrl-PgUp` / `Ctrl-PgDn` for the previous and next one (these work while typing too);
- `/ws <name>` (a name or an original name); `/ws` alone lists them. A name that doesn't exist yet asks `y` to create it.

`+ New workspace` (or `/ws new`) asks for a name and checks it the way `twr ws new` does: lowercase letters, digits and `-`, and not a workspace's name or original name (`default` included while the workspace in twr's data directory exists; once it was removed, `default` makes it again). `Enter` creates the workspace and switches to it. `/ws new <name>` does it in one go. `/ws new`, `/ws menu`, `/ws rename`, `/ws rm` and `/ws delete` are commands, so a workspace with one of those names isn't reached with `/ws <name>`: use its card, its tab, `Alt-1`..`9`, or `/ws menu <name>` and its `Open`. A TUI that a thread's agent runs creates no workspace, whichever way it's asked, and neither does `twr ws new` from an agent. `⚙ Settings` opens the [settings](tui.md#settings).

The TUI is connected to one hub at a time. Switching opens the other workspace's hub, starting it if it isn't running, and replays its log the way opening the TUI does. What you left in a workspace is kept for when you come back: the line you were typing, the selected card, pins, the view, the scroll position and the thread you were talking to. The TUI asks every running hub how it is every five seconds, with one light call each (`hub.glance`: its counts, its orchestrator's latest reply and its threads), and a stopped hub or one that didn't answer every fifteen seconds; it never starts a hub to ask it, and looking at another workspace's threads resumes none of them. A hub older than this TUI is asked for its threads with a second read-only call (`thread.list`); one older still answers with its counts only, without its orchestrator's reply, until it's restarted. With [notifications](tui.md#notifications) on, a workspace that gets a new permission or question notifies you, naming the workspace.

A TUI opened with `--socket` shows that hub alone, without workspaces in the sidebar or tabs.

## What's separate and what's shared

| | Each workspace has its own |
|---|---|
| Hub, socket and log | yes |
| Orchestrator and its conversation | yes |
| Orchestrator's [memory](orchestrator.md#memory) | yes, on every runtime. A named workspace's orchestrator also has its own working directory, so a Claude Code orchestrator's own memory directory is that workspace's too |
| Threads, watches, reports, attention | yes |
| Worktrees and trash | yes |
| Branches | new threads use `twr/@<workspace>/<name>`, so two workspaces can each have an `api` thread in one repository |
| Input history | yes |

Every workspace reads the same `config.toml`: projects, runtimes and notification settings apply to all of them. One workspace lives in twr's own data directory: the one called `default` when twr made it (renaming it changes nothing here). Its hub alone uses `hub.socket`, `hub.listen` and `hub.tls`, dials the [boxes](runners.md) and runs the orchestrator in `[orchestrator] cwd`; the others ignore those. Its `hub.socket` can't be in the directory of the workspaces' sockets (`.../twr/ws/`). Agent installations and logins are shared, as are your repositories.

One exception for Claude Code's own memory directory: Claude Code keys memories by the git repository a working directory is in. If your `TWR_HOME` is inside a git checkout, the orchestrators of all its workspaces share that repository's memories.

An agent's own `twr` calls reach its workspace's hub: the hub sets `TWR_WORKSPACE` to the workspace's original name for everything it starts, and for a CLI you attach (`default` included), so a rename or a new default workspace never sends them elsewhere. The commands twr suggests, such as the TUI's `/attach` (`twr -w client-x attach api`), `twr resume api` or `twr trash list`, come with `-w` and the original name of the workspace they're about, unless `twr` without `-w` reaches that workspace from your shell already. What an agent asks to run is shown as it asks, even when it quotes a `twr` command.

Workspaces keep their agents apart too. Every hub treats a process that descends from any twr hub, or from a `twr attach`, as an agent, also when it hides or drops the `TWR_AGENT` marker, so another workspace's agent can only read ([usage.md](usage.md#security)). Agents can't stop hubs (including `twr hub stop --all`), or create, rename or remove workspaces or choose the default one, from the command line or the TUI. A thread can't be started in another workspace's worktree, whose hub would remove or move it with its own thread.

A command reaches only its workspace's hub: every hub says which workspace it serves, and a command that didn't get `--socket` refuses one that serves another (say, a hub started with `--socket` on that workspace's socket), the TUI too.

## Where they live

| | Default | With `TWR_HOME=X` |
|---|---|---|
| data of `W` | `~/.local/share/twr/workspaces/W/` | `X/data/workspaces/W/` |
| socket of `W` | `$XDG_RUNTIME_DIR/twr/ws/W.sock`, else `~/.local/state/twr/ws/W.sock` | `X/ws/W.sock` |
| config | `~/.config/twr/config.toml`, shared | `X/config.toml`, shared |

`W` is the workspace's original name, the one it was created with: a rename moves none of this ([below](#renaming-one)). `default` is twr's data directory itself (`~/.local/share/twr/`, `X/data/`) and its usual socket. `TWR_HOME` still moves everything twr keeps, workspaces included ([configuration.md](configuration.md#a-second-instance)). Under it, workspace branches are `twr/<home-path-hash>/@W/<name>`.

## Stopping every hub

```sh
twr hub stop --all
twr hub stop --all --timeout 10  # seconds per workspace
```

This stops every workspace in the current home, ignoring `-w` and `TWR_WORKSPACE`, through the same shutdown as `twr -w NAME hub stop`: local runtimes stop first, while remote runners keep their threads. Each workspace prints `stopped`, `not running`, or `failed` with a reason. Any failure makes the command exit non-zero. Each stop runs concurrently with its own timeout, so a hung hub cannot delay the others. It checks each hub's workspace identity before stopping it. A hub that has claimed the workspace lock or bound its socket is waited for, including one already shutting down. A later startup, after its workspace has been checked, is outside this scan.

It covers `default`'s configured `hub.socket` (or its standard socket), and the named workspaces' standard sockets. A hub started on another explicit `--socket` is outside that set: a held workspace lock with no hub answering at its usual socket times out and reports failure. Use `twr --socket PATH hub stop` for that hub; `--socket` and `--all` cannot be combined. With `TWR_HOME=X`, only X's workspaces are covered; other homes require their own command. See [stopping and updating](usage.md#stop-the-hub-and-update-twr).

## Renaming one

```sh
twr ws rename client-x acme   # alias: twr ws mv
twr ws rename default main
```

Any workspace can be renamed, `default` too, its hub running or not. Nothing moves: the name is a file in the workspace's directory (`workspace-name`), and the directory, the socket, the worktrees, the trash, the memory, the runtimes' sessions (keyed by working directory) and the branches all stay keyed by the original name. A running hub keeps running and says its new name right away; the TUIs on it show the new name on their next look (within five seconds; at once in the one that renamed it), and keep what you left in it. New threads' branches still use the original name (`twr/@client-x/<name>`), so they never meet another workspace's.

The original name keeps selecting the workspace: `-w client-x` still reaches acme, as does what an agent of acme runs. So no other workspace can be named `client-x` while acme exists (`twr ws` lists it as `acme (was client-x)`); acme can take it back with `twr ws rename acme client-x`. That alias is not a workspace of its own and holds nothing: removing acme frees both names. Likewise only the workspace in twr's data directory can be named `default`. A new name follows the usual rules and can't be another workspace's name or original name.

## The default workspace

```sh
twr ws default          # which one it is
twr ws default acme     # make acme the default
```

Commands without `-w` or `TWR_WORKSPACE`, and a bare `twr`, use the default workspace. It is `default` until you choose another (`twr ws default`, or `Make default` in a card's menu); a rename keeps it the default. `twr ws` lists it first, marked `(default)`. The choice is the file `default-workspace` in twr's data directory.

Removing the default workspace leaves `default` the default again, when it exists; removing `default` while it is the default leaves no default. Then a command without `-w` fails, naming the workspaces there are and `twr ws default <name>`, and a bare `twr` opens the first workspace listed, saying so in its status bar. Once there are no workspaces at all, a bare `twr` in a terminal asks for the name of one to make (`Enter` makes `default`), and other commands say to run `twr ws new <name>`. The first workspace made while there is no default becomes it.

## Removing one

```sh
twr -w client-x hub stop
twr ws rm client-x      # lists what goes, asks y/N
twr ws rm client-x -y   # without asking
```

`twr ws rm` removes any workspace for good, `default` too, and says what went and what stayed. It refuses while the workspace's hub runs (on any socket: it holds a lock on the workspace's data), while it has threads (delete them first) and while its trash holds deleted threads, on this machine or on a box (`twr -w client-x trash empty --all`), whose commits stay referenced in their repositories until purged. It refuses too while messages wait in its orchestrator's queue, which its next hub would send, and, for `default`, while a `twr runner` of this machine keeps its data in twr's data directory (a `runner-<box>.db` there), whether it still runs or not: that file holds what the runner hasn't delivered to its hub yet, and its worktrees and trash are `default`'s. If the runner runs, or will again, start it with a `--data-dir` of its own once its hub has what it holds; if it never will, deleting `runner-<box>.db` loses what it never delivered, and the message names what it left in `worktrees/`, `trash/` and `loops/` (its threads' files), which go with `default` (the listing shows them before it asks). `twr ws new default` refuses the same way. It refuses too while the log shows the orchestrator's work that never ended (a turn, a job or a `!` command left running when its hub stopped without ending it, a crash say): those processes may still run, and the workspace's next hub finds and ends them, so start it (`twr -w client-x thread list`), stop it and remove the workspace. It takes the workspace's log, its orchestrator's conversation and memory notes, its input history and its worktrees directory. Branches stay in your repositories, a Claude Code orchestrator's own memory directory stays in `~/.claude/projects/`, and the config file, the boxes, the accounts and the other workspaces aren't touched.

It first lists what goes (the orchestrator's conversation with its turns, how many memory notes, the log's size, any worktree directories a failed deletion left in its `worktrees/`, and the size of the workspace's files in all) and what stays, and asks `Remove workspace client-x? This cannot be undone. [y/N]`. Without a terminal it removes nothing unless you pass `-y` (`--yes`), and the error lists what would go.

For a workspace you created, that is its directory. `default` lives in twr's data directory with what every workspace shares, so there `twr ws rm` removes only `default`'s own files (`twr.db` and `archive.db` with their journals, `hub.log`, `trash/`, `jobs/`, `suggest/`, `memory/`, `loops/`, `worktrees/`, `tui-history`, its identity to the runner service `link-identity`, its name `workspace-name`, its token `workspace-generation`, the orchestrator's MCP files `orch-mcp-*`, and what a writer of those left half written) and leaves the rest: `workspaces/`, `accounts/`, `job-slots/`, the runner service's `machine/`, the gateway's receipts and the phones' voice state in `mobile/` (the pairings are in `mobile/` beside the config file), the TUI's and the speaker's state, the boxes' key, and any file that isn't twr's. A file `workspace-removed` there then says `default` is gone, and its hub won't start (`twr -w default` says so). Its duties go with it: no hub serves the boxes, `hub.listen` or `hub.socket` until `twr ws new default` makes it again, empty, in the same place, and `twr ws rm` warns about that when the config has boxes or a listener. Some of phone pairing goes too, as it works through `default`'s hub: until `twr ws new default`, you can't pair, list or revoke phones (`twr phone`, or from a phone), phones get no pushes about `default`, and the phone gateway can't start (it starts `default`'s hub first). A gateway that already runs keeps serving your paired phones' other workspaces. `twr ws rm` says so when phones are paired or `twr phone` set the gateway up. The pairings themselves are in `mobile/` beside the config file, which the removal doesn't touch, so the same phones work in `default` again once it is back. So does telling a linked runner you removed (`twr box remove`) that it is no longer this machine's, when it hasn't acknowledged that yet: the runner service retries it through `default`, so it waits while `default` is gone and goes through once it is back, and `twr ws rm` says how many wait.

The removal holds a lock that `twr ws new`, `rename`, `default` and `rm` all take, and the workspace's own lock, which every hub of it holds while it runs, so no rename, removal or hub start slips in between its checks and the removal, nor while its question waits. A hub never makes a removed workspace again: a named one's directory is never created by a starting hub, and `default`'s hub checks `workspace-removed` once it holds the lock. A removal cut short (a crash, a full disk) leaves the workspace removed, never half there: a named one's remains are in a `.rm-<id>-…` directory beside the workspaces (`<id>`: its original name), which nothing takes for a workspace and you can delete, and `twr ws new default` clears what `default`'s removal left. It never deletes what came under `default`'s names after the removal (a `twr runner` keeping its data in twr's data directory writes `worktrees/`, `trash/` and `loops/` there): an empty one goes, and one that holds files makes it refuse, listing each with its files and size, for you to move or delete. A TUI that still showed the removed workspace finds its hub gone, and can't open it again.

In the TUI, `Delete…` in a card's menu does the same, after showing what it removes ([above](#in-the-tui)).

## Upgrading

Nothing to migrate: every existing workspace keeps its directory, and its name is its directory's name until you rename it. Restart running hubs (`twr hub stop --all`, then `twr`) for them to say their new names and to give their agents `TWR_WORKSPACE=default`; until then an agent of an older `default` hub runs `twr` in whatever the default workspace is (where it is another workspace's agent, so it can only read). An older twr doesn't know renamed names or a removed `default`: don't go back to one after using them.
