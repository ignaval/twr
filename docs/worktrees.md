# Worktrees

By default every worker thread gets its own git worktree under the data directory, at `<data>/worktrees/<project>/<name>`, on a new branch `twr/<name>`. Parallel threads on one repo then never touch each other's files, and `twr park` can remove a clean worktree to save space. The orchestrator never has a worktree.

```sh
twr thread new ~/code/api --name api              # branch twr/api from the repo's HEAD
twr thread new ~/code/api --name fix --base v1.4  # branch twr/fix from a tag
```

In a named [workspace](workspaces.md), new local worktrees use `twr/@<workspace>/<name>`, and with
`TWR_HOME` they use `twr/<home-path-hash>/<name>` (`twr/<home-path-hash>/@<workspace>/<name>` in a
named workspace), so independent hubs can use the same thread name in one repository. A thread
name never starts with `@`, so no thread's branch is ever in the way of a workspace's. Existing
threads keep their recorded branch. The examples above use the default instance
([configuration.md](configuration.md#a-second-instance)). A workspace keeps its worktrees in its
own data directory: `<data>/workspaces/<workspace>/worktrees/<project>/<name>`.

Renaming a thread (`twr rename`) changes neither its worktree path nor its branch: both keep the name the thread had when they were made, and that name stays reserved for the thread ([usage.md](usage.md#rename-a-thread)).

The branch starts at the repository's `HEAD`, or at `--base <ref>`. If the instance's branch already exists, twr checks it out as it is. Thread names must work as git branch names: no leading `.` or `-`, no `..`, and no trailing `.` or `.lock`. If the worktree can't be created, the thread isn't created either.

A project that isn't a git repository runs in place, in its own directory.

The TUI's cards and tiles show each thread's branch, `changes made` while its worktree has uncommitted changes (a submodule counts by its commit: changes inside its work tree don't show there, though parking and deleting still see them), and how far it is ahead of and behind its base: `twr/api · changes made · ↑4 ↓12 dev`, or `twr/api · ↑4 ↓12 dev` when it's clean. The hub takes a git snapshot after every turn and every 30 seconds, and records it when something changed.

### Merge readiness

A thread's base is the branch its work merges back into: `[projects.<name>] base` in the [configuration](configuration.md) for a configured project's threads (`base = "dev"`), else the `--base` its worktree was made from, else the branch the repository was on then. The card counts the commits the thread's branch has that the base hasn't (`↑4`), and the other way around (`↓12`). When merging the two would conflict, the line starts with `conflicts: 2 files` in red (just `conflicts` when git names no file, as for some directory renames). The orchestrator sees the same in `get_thread`, and can say "api is 12 behind dev and conflicts in hub.rs".

twr finds conflicts with `git merge-tree --write-tree`, without touching the worktree or the base. It needs a recent git, one with the `--attr-source` option; with an older one the card counts commits but doesn't say whether a merge would conflict. It merges by the attributes committed on the thread's branch (its `.gitattributes`). The merged tree is written to the repository's object store and nothing points to it, so git's next `gc` removes it. Without a base (a thread from before twr recorded one, and no `base` for its project), the card counts against the branch's upstream as before. When twr can't work merge readiness out (the base is gone, git fails or runs out of time), the card leaves it out until it can, rather than show what it found at earlier commits.

### What twr's git runs

A repository's config can name programs that git runs: filters (`clean`, `smudge`, `process`), merge drivers and diff drivers, hooks (in `.git/hooks`, and since git 2.54 in config, `hook.<name>.command`), an fsmonitor. An agent can write the config of the repository it works in. So the git commands twr runs (snapshots, making and removing worktrees, the trash, reading files for the orchestrator) run none that the repository defines, but for the race below. twr turns hooks and fsmonitor off. Before each command that could run a filter or driver, it reads which ones the repository's own config defines (its local and worktree config, the files those include, also conditionally, as they apply where the command runs), and for a command that goes into submodules each initialized submodule's own, and overrides each so that it runs nothing (a hook defined in config is turned off and, should it run, does nothing). A new worktree is made without files first, and its files are checked out in it once the drivers its own config names (a branch-only `includeIf` too) are off. When twr can't tell them all (more than 64 repositories, submodules nested deeper than 4, a driver whose name has a `=`), the command doesn't run. What your global or system config defines, such as Git LFS's filter, you set yourself, so it still runs. A file with a merge driver the repository defines counts as conflicting. twr's git also uses no git transport: none is allowed (`ext::` included), and a partial clone fetches no missing object. Git LFS does its own fetching: as a filter from your config, it downloads LFS files from the LFS server the repository names, its `.lfsconfig` included, when twr checks out a worktree, as `git checkout` would. It does that over HTTPS, asking your credential helpers for that host when the server wants a login, or, for an `ssh://` URL, with your `ssh` and your keys. Where the repository's own config names a program for Git LFS to run (a custom transfer agent, an extension, a credential helper, an ssh command or askpass), twr turns LFS off for that repository's checkouts, which then hold LFS files as pointers, and logs why ([security-model.md](security-model.md#git-in-repositories-agents-control)). With git older than 2.26, which can't say where a setting comes from, every driver counts as the repository's, so your global ones, Git LFS's included, are off as well.

One gap remains. Reading the drivers and running the command are two git processes, so a driver named in the config between the two (by something that can write the repository's config, or a worktree's `config.worktree`) runs once. twr reads the config again after each such command: a new driver is logged as an error naming it and the directory, a snapshot or readiness check that raced is thrown away, and the next command turns the driver off. [security-model.md](security-model.md#git-in-repositories-agents-control) says who can use this gap.

## Working in your checkout instead

If you'd rather have the agents work in your checkout, turn worktrees off:

```toml
[threads]
worktree = false        # for every project

[projects.api]
repo = "/home/me/code/api"
worktree = false        # for this project only; beats [threads]
```

Those threads work in the repository directory itself. Parking never removes that directory, and snapshots and `twr attach` work as usual. The cost is that two threads on the same repo share one checkout and one branch, so they can overwrite each other's edits and see each other's uncommitted changes.

Per thread, `twr thread new --worktree` (or `--base <ref>`) still gives one thread its own worktree when they're off, and `--no-worktree` skips one when they're on.

twr refuses to start a thread in a directory inside another thread's worktree. That worktree belongs to its thread and goes away when the thread is parked.

## Park and resume

```sh
twr park api           # stop the runtime; remove the worktree if it's clean
twr park api --keep    # stop the runtime; keep the worktree
twr park api --force   # interrupt a running turn instead of refusing
twr resume api         # recreate the worktree if needed, same session
```

Parking stops the runtime the way an attach does. Then, unless you pass `--keep`, twr removes the worktree if it's clean. It never forces the removal, and the thread's recorded branch always stays. twr keeps the worktree when:

- it has uncommitted changes or untracked files (the command says how many);
- its checkout isn't the recorded branch, for example a detached `HEAD` or another branch the thread switched to, because commits made on a detached `HEAD` would only be remembered by the worktree's own reflog, and `twr resume` would bring the worktree back on the recorded branch rather than the one it was on (the command says which branch it is on);
- another thread's runtime works in it, or an attach holds it;
- it holds commits only its own reflog remembers.
- its path is a symlink or holds another checkout ([Delete](#delete) has the rule).

The command prints the reason when it keeps a worktree.

Ignored files are a separate case. Parking deletes the ones that match `[threads] park_disposable` along with the worktree: build output and caches such as `target/`, `node_modules/` and `.venv/` by default ([configuration.md](configuration.md)). Any other ignored file keeps the worktree, and the command names it.

Prompts sent to a parked thread queue up and wait. A hub restart leaves a parked thread parked.

`twr resume` recreates a removed worktree at the same path from its recorded branch, because runtimes such as Claude Code key their sessions by directory. Then it runs the queued prompts on the same session, or, if the thread moved to another runtime while parked, on a new session of that runtime given the handoff. If the worktree can't be recreated, the thread stays parked.

In the TUI, the `park` and `resume` rows of a card's action popup park and resume its thread after a `y`. `/park [thread]` and `/resume [thread]` take a thread name. The TUI's cards and `twr thread list` leave parked threads out unless you ask for them: `/parked on` in the TUI, `twr thread list --all` on the command line.

## Trim

```sh
twr trim api           # delete api's build output; its next build makes it again
twr trim api web       # several threads, one after the other
```

Trimming deletes a thread's build output and nothing else: the ignored entries that match `[threads] park_disposable` (`target/`, `node_modules/`, `.venv/` and the rest of the default list) and hold no git repository, the ones a park or a delete deletes, each checked again as it goes, never through a symlink or a mount point. Ignored files of other names (`.env`), tracked files, uncommitted work, the branch and the thread's session all stay. The thread keeps working, and its next build makes the output again: for twr itself a full build, several minutes on a busy machine.

A thread is trimmed only while it is idle: no turn or background work running, not attached, parked or resumed, not being deleted. A runtime that is up but idle is stopped first, its session kept: the thread doesn't count as stopped, and its next prompt resumes the session. Prompts sent meanwhile wait until the trim is over. A parked thread whose worktree was kept can be trimmed too. twr trims only worktrees it made, only on the hub's own box (not yet on a runner's), and only on Linux and macOS, where it can list the mount points inside a worktree. A worktree with a file system mounted in it isn't trimmed: unmount it first. In the TUI, `/trim [thread]` does the same. The orchestrator can't trim.

## Disk use

The hub measures each thread's worktree on its box, and how much of it is build output: a worktree that had a turn or a trim at most every 10 minutes, any other every 6 hours, one at a time. `twr thread list` shows it in its DISK column (`7.1 GB (6.8 GB build)`; `≥` when the walk stopped at 2 million entries or could not read part of it). It also reads, every 5 minutes, how much each box has free where it keeps its worktrees (`hub.info`'s `disk`).

When that falls under `[threads] disk_warn_gb` (10 GB by default; 0 turns it off), the hub raises a disk card (a remote box's names no threads to trim), on the `hub` thread: `Disk low on local: 4.2 GB free, under 10 GB. Trim deletes the build output of 3 idle thread(s): api 7.1 GB, web 3.0 GB, docs 0.4 GB (10 GB in all); their next builds make it again.` Only you answer it. `Trim` trims those threads that are still idle, one after the other, and the conversation says what it did; `Not now` (or dismissing it) closes it. Either way the box gets no new card until its free space has been back over the threshold, or the hub restarts. A card closes by itself once the free space is back over the threshold. Meanwhile the TUI's status bar says `disk low: local 4.2 GB free` ([tui.md](tui.md#machine-load)).

The walk counts build output by name alone, so a tracked directory named `build/` counts too; a trim deletes only what git ignores.

A project can have its idle threads trimmed without waiting for a disk card:

```toml
[projects.api]
repo = "/home/me/code/api"
trim_idle_after = "24h"           # 90m, 24h, 2d
```

The hub's disk round, every 5 minutes, then trims each of the project's threads that has been idle that long: no turn, background work or job, and nothing recorded for it since but the hub's measurements, trims and git snapshots. Only threads made with the project's name, on the hub's own box (Linux or macOS), in a worktree twr made, with build output as last measured, and not trimmed since they last worked. The trim is recorded as the hub's, and the thread's next build makes the output again. It's opt-in per project because rebuilding costs each project something different: minutes for a large Rust workspace, seconds for a small one.

An idle trim deletes less than `twr trim`. Nobody looks at it before it runs, so it takes only the entries with one path component named by both `park_disposable` and `[threads] idle_trim_disposable`: `target/`, `node_modules/`, `__pycache__/`, `.pytest_cache/`, `.mypy_cache/`, `.tox/`, `.gradle/` and `.next/` by default. A build or a test run always makes those again. `dist/`, `build/`, `.cache/`, `.venv/` and `coverage/` stay, because they can hold something no build regenerates (a release you built by hand, a virtualenv with packages installed one by one). `twr trim` and the disk card's Trim still delete them. A project can set its own list, which replaces the `[threads]` one for its threads:

```toml
[projects.web]
repo = "/home/me/code/web"
trim_idle_after = "24h"
idle_trim_disposable = ["node_modules/", ".next/", "dist/"]  # dist/ is safe to rebuild here
```

An entry still has to be in `park_disposable` to go: `idle_trim_disposable` only narrows it.

## Delete

```sh
twr delete api         # after a y/N: stop it, move its worktree and twr/api to the trash
twr delete old older   # several threads, parked ones too, on one card
```

Deleting a thread moves its worktree, whatever it holds (uncommitted changes, untracked and ignored files included, and on Linux and macOS all but its build output), and its recorded branch to the trash of its box, even when the branch has commits on no other branch, tag or remote. Nothing goes until you confirm the delete card, whose text counts the uncommitted files and those commits for each thread, and also the commits only the worktree's `HEAD` and its reflog keep, which nothing that stays (a branch, tag, remote, stash or another reflog) still reaches, and says when the branch existed before the thread ([usage.md](usage.md#delete-threads)). What it counts is what is lost once the trash is purged.

Build output means the ignored entries that match `[threads] park_disposable` (`target/`, `node_modules/`, `.venv/`, ...), the ones a park deletes. On Linux and macOS the deletion deletes them instead of moving them to the trash, unless one holds a git repository. On other systems they go to the trash with the rest and come back on restore. They're regenerated, and with several threads on a Rust repository each `target/` can be tens of GB that the trash would keep for a week. The card says what goes this way and its size.

The move goes in this order, and deletes nothing else on the way. First a trash item `<data dir>/trash/<item>/` is made, and refs under `refs/twr-trash/<item>/` in the repository keep every commit the branch (its tip and reflog), the worktree's `HEAD`, its reflogs and its own refs (`refs/worktree/*`, `refs/bisect/*`) reach, with the branch's and `HEAD`'s reflogs copied. Then the build output is deleted (on Linux and macOS), the worktree's directory moves into the item (`worktree/`), then its record (`<repo>/.git/worktrees/<name>`, with the repositories of its submodules) moves to `record/`, so git forgets the worktree, and last the branch is deleted with `git branch -D`. The branch and the path are free for a new thread right away. If a step fails, what didn't move stays where it was and what did stays in the item; asking again finishes the job. When the trash is on another file system, files are copied there and then removed one by one; anything that can't be removed stays at the old path, and the delete names it for you to remove by hand.

A parked thread whose worktree a park already removed only has the worktree's record (even a locked one) and its branch moved; the card counts the git repositories that record still holds, such as initialized submodules'. twr only ever moves a linked worktree of the thread's repository, or what is left of one (a directory without `.git`, or whose `.git` names a gone record of the repository, not inside a work tree or the repository): never the repository's main checkout, another repository or a plain directory. The worktree's directory and its record must name each other: when the thread's path is a symlink, or holds another checkout (its `.git` names another worktree's record, as after copying a checkout there), the deletion refuses and touches nothing, and so does a park (it keeps the worktree and says why).

A thread may switch its own worktree to other branches (a thread that integrates its own work makes a new branch for each step). Such a worktree is still the thread's: deleting it moves the worktree and the thread's recorded branch to the trash, and leaves the branch the worktree is on in the repository, with every other branch the thread made. twr doesn't know which branches a thread made, and the one it is on may be what you merge next, so none of them goes. The delete card names the branch the worktree is on and counts its commits on no other branch, tag, remote, stash or reflog. A restore puts the worktree back on that branch while it still points where the worktree left it, isn't checked out elsewhere and isn't another thread's recorded branch, and on the thread's recorded branch otherwise ([usage.md](usage.md#trash)). The worktree counts as the thread's own only when git's record of it shows it left the thread's branch: its `HEAD` reflog has a `checkout: moving from twr/api to ...` entry, or a restore of the thread made it. Otherwise it is another checkout, and the deletion refuses and touches nothing. So does a worktree on another thread's recorded branch in the same repository (the hub tells the box which those are), and one whose record git made for another path and that was moved there (`git worktree move` keeps the name a record was made with, after the path's last component). A worktree switched so long ago that git expired that reflog entry (`gc.reflogExpire`, 90 days by default) is refused too: switch it back to its branch and delete it again. A park keeps a worktree on any other branch either way. The other worktree, its record and its branch stay as they are. A thread without a worktree of its own keeps its directory. twr refuses the deletion while another thread is attached inside the worktree, and no attach can start there while it runs. It also refuses while the worktree holds a linked worktree whose record lives elsewhere (one of another repository, or another worktree of this one, checked out somewhere inside it): moving it would leave that record pointing at nothing, and `git worktree prune` plus `git gc` in its repository would then lose the commits only it has. The delete card says so, with the `git -C <repo> worktree move` / `remove` to run first, and nothing is moved; a park keeps such a worktree too.

The trash is purged after `[trash] keep_days` (7 days by default), or sooner with `twr trash empty`; `twr trash restore` makes the branch again from the trash's ref, has git add a fresh worktree on it and copies the saved files in; the item stays in the trash until it is purged ([usage.md](usage.md#trash)).

## Threads on other boxes

A thread on a remote box gets its worktree on that box, under the runner's data directory: `<runner data>/worktrees/<project>/<name>`, branch `twr/<name>`. See [runners.md](runners.md).
