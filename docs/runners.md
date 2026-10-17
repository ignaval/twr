# Runners: threads on other machines

A box other than `local` is another machine, or a container, that runs threads for the hub. Adapters, git worktrees and snapshots all happen on the box. There are three kinds. A box the hub can reach over SSH needs one command and nothing else on your side: no token, listener, tunnel or service. A container box is the same, with `docker exec` (or podman, or `kubectl exec`) in place of ssh ([Container boxes](#container-boxes)). A box the hub can't reach either way, a dial-in box, runs `twr runner`, which connects out to the hub ([Dial-in boxes](#dial-in-boxes)).

## SSH boxes

```sh
twr box add gpu me@gpu-box
twr thread new api --box gpu
```

`twr box add <name> <destination>` takes any destination `ssh` understands: `me@host`, a host from `~/.ssh/config`, or options before it, as in `twr box add gpu -p 2222 me@host`. It does this:

1. It runs ssh to the box in your terminal, so ssh can ask about the host key or for a passphrase (for at most 2 minutes; what ssh and the box print shows with its control characters escaped), and prints what it found: the box's OS and CPU, its data directory (`$XDG_DATA_HOME`, else `~/.local/share`) and the twr builds already there.
2. It checks that ssh also works without prompting, the way the hub runs it. If it doesn't, it warns and goes on: the box can be added, but it won't connect until it does.
3. It says what will happen and asks `Add box gpu? [y/N]`. `--yes` skips the question; without a terminal, `--yes` is required.
4. It writes `[boxes.gpu]` with `transport = "ssh"` and the destination to `boxes.toml`, next to `config.toml`, and adds the box to the running hub (`box.add`), starting the hub when none runs.
5. It waits up to 90 seconds for the box to come online and prints each state on the way: `connecting`, then `online` with the twr version and how to start a thread there. On a timeout it prints the state, the last error and the box's notes, and exits non-zero.

Boxes belong to the default workspace's hub: `twr -w <name> box add` is refused.

### What the hub does

For each SSH box, the hub runs ssh itself, from its own environment:

- **Probe.** It runs a short script over ssh that prints the box's `uname -s` and `uname -m`, its data directory and the twr builds installed there. Whatever a login script prints around it is ignored.
- **Install.** On a box of the hub's OS and CPU, the hub runs its own build there. When that build is missing, the hub streams its own executable through ssh into `~/.local/share/twr/runner/builds/<build id>/twr` (mode 0700), and checks its size, its SHA-256 (with `sha256sum` or `shasum`, when the box has one; one that fails refuses the upload) and its `--version` before it moves it into place. A build id is the first 16 hex digits of the executable's SHA-256. twr uses the builds directory only when it and the directories above it are yours and no other user can write to them ([security-model.md](security-model.md#ssh-boxes)). The builds directory is shared by every twr instance that reaches the box as that user, so each hub records the builds it uses there, per box, in `builds/.refs/<hub id>-<box>`, and `twr box install` records the build it installs in a file of its own next to it (`<hub id>-<box>.install.<build id>`), which the hub removes once it runs that build. Writers take `builds/.refs.lock` (a directory, removed when they are done, or by the next writer two minutes later). After connecting, a hub keeps every build any of those records names, plus one more, any build an upload is writing and any build changed in the last 30 minutes, and removes the rest. A record no hub has rewritten for 90 days stops counting: a hub you no longer run doesn't keep its builds forever. A box of another OS or CPU needs `twr box install` ([below](#another-os-or-cpu)).
- **Connect.** It runs `ssh <box> -- <build> runner --stdio --box=<name> --hub-key <key>`. That relay finds the box daemon of this hub and box, or starts one, and passes bytes between ssh and the daemon. The daemon owns the box's runtimes, its spool and its worktrees, and keeps them running when the connection ends. It keeps its data in `~/.local/share/twr/box-daemons/<hub id>-<box>/` (its log is `daemon.log` there) and listens on a socket in `~/.local/state/twr/box-daemons/`. The hub id is the first 16 hex digits of the SHA-256 of the hub's public key, so two twr instances (`TWR_HOME`) that share a box get separate daemons.
- **Sealed channel.** The hub and the daemon talk through a channel only the hub's key can open, end to end: the relay and the socket only see encrypted records. See [security-model.md](security-model.md#ssh-boxes).
- **Reconnect.** When the connection drops (the network, laptop sleep, a killed ssh), the hub tries again 1 second later, then at most 5 seconds apart during the first minute and 30 seconds after that. The daemon and its agents run on meanwhile, and the next connection picks up where the last one left off, as a token runner's does ([When the connection drops](#when-the-connection-drops)). After a hub restart, the new hub connects to the same daemon. A relay whose connection ends before it relays (the hub gave up on the attempt and killed its ssh) starts or stops nothing more, though a daemon start already under way finishes.
- **A box reboot** ends the daemon and its agents. The hub's next connection starts a new daemon. Threads whose agents were running get an error card, as a token runner's do after a restart.

ssh runs with `BatchMode=yes`, so it never prompts, and with every kind of forwarding and local commands turned off, whatever your `~/.ssh/config` says. It reads your ssh config and keys otherwise. `TWR_SSH` names another ssh program. The options in `[boxes.<name>] ssh` can't turn forwarding back on or change how ssh runs: see [configuration.md](configuration.md#notes-on-some-keys) for what they may hold.

### What you need

- **ssh to the box without a prompt, from the hub's environment.** The hub never answers a prompt. Load the key into an ssh-agent the hub can use (a hub that `twr` starts inherits your shell's `SSH_AUTH_SOCK`; a hub started by a service has no agent unless you give it one), or use a key without a passphrase. Accept the box's host key once, which `twr box add` lets you do. A refused key shows in `twr box status <name>` as the last error.
- **A POSIX login shell** for your user on the box (`sh`, `bash`, `zsh`): the hub quotes its commands for one.
- **The agent CLIs on the box, logged in.** The daemon starts them with the environment of a non-interactive ssh command on the box, with your login shell's `PATH` put first ([Agents on a box](#agents-on-a-box)). `twr box doctor <box>` shows what the box finds, and `twr box login <box> <agent>` logs an agent in from your terminal. Set `TWR_CLAUDE_BIN`, `TWR_CODEX_BIN` or `TWR_GROK_BIN` where that shell sets its environment to point at another binary.
- **The repositories on the box.** `twr box project gpu api` finds a project's repository on the box, or clones it there, and records where it is ([Projects on a box](#projects-on-a-box)). You can also give a thread an absolute path on the box (`twr thread new /srv/code/api --box gpu`), or name a project whose `repo` is a path on the box. `boxes.toml` may hold the box's projects too: twr keeps every table of a box there when it rewrites the file, but not comments.

Threads on an SSH box get the branch names of threads on the hub's own box (`twr/<name>`). Two hubs on different machines that use one repository on one box can collide on them.

### Managing boxes

| Command | What it does |
| --- | --- |
| `twr box list [--json]` | the hub's boxes: name, transport (`ssh`, `command` or `token`), state, the twr version its runner reported, OS/CPU, and its agent CLIs as the box last reported them (`claude ✓ codex ✗ grok ?`: logged in, logged out, not known; `-` not installed) |
| `twr box status <name> [--json]` | everything the hub knows of one: state, version, the build it launched, a pending switch, OS/CPU, how its daemon was started, the last error, the latest notes (newest last), a connection the hub refused, and each agent CLI's path, version and login |
| `twr box install <name> [--binary <path> \| --from-box <path>] [--yes]` | install twr on an SSH or container box and have the hub connect again ([below](#another-os-or-cpu)) |
| `twr box restart <name> [--yes]` | have the hub restart the box daemon ([below](#restart-the-box-daemon)) |
| `twr box add <name> --dial-in --hub <url> [--ca <pem>] [--yes]` | add a dial-in box: a new token in `boxes.toml`, the command to run on the box ([Dial-in boxes](#dial-in-boxes)) |
| `twr box remove <name>` | take the box out of `boxes.toml` (a dial-in box: no longer accepted by the running hub) |
| `twr box token <name> [--rotate [--yes] \| --reload]` | print a dial-in box's token, replace it, or have the running hub read it again ([Tokens, rotation and removal](#tokens-rotation-and-removal)) |
| `twr box doctor <name> [--json]` | check the box and say how to fix what is wrong, with or without a hub ([Check a box](#check-a-box)) |
| `twr box login <name> <agent>` | start an agent CLI's own login on the box, in your terminal ([Log an agent in on a box](#log-an-agent-in-on-a-box)) |
| `twr box project <name> <project> [--path <path> \| --clone [--into <dir>] \| --remove] [--yes]` | find a project's repository on the box (or clone it there, in your terminal) and record where it is ([Projects on a box](#projects-on-a-box)) |

States: `connecting` (probing, installing, opening the connection), `online`, `offline` (between attempts), `needs_install` (run `twr box install <name>`), `switching` (see below) and `error` (the hub's box key can't be loaded).

`twr box remove` changes only `boxes.toml`. A box you configured in `config.toml` stays there: remove it yourself. The running hub keeps the box connected until it restarts (`twr hub stop`, then `twr`); a dial-in box's runner is refused at once instead ([Tokens, rotation and removal](#tokens-rotation-and-removal)). The box daemon, its agents and the box's worktrees stay on the box; `twr box remove` prints the command that stops the daemon: the hub's own ssh, with its options and forwarding off, running `<build> runner --stdio --box=<name> --hub-key <key> --stop` on the box (for a container box, the box's command with `sh -c`; twr never removes the container). Run it once the hub no longer connects to the box, or the hub starts a daemon again.

### Another OS or CPU

The hub can only install its own build on a box of its OS and CPU. For another one, `twr box add` says so and the box shows `needs_install`. `twr box install <name>` then:

- with `--binary <path>`, uploads that file, a twr you built for the box. It checks the size and hash on the box, runs its `--version` there and shows it. It warns when the version isn't the hub's: the two connect only if they speak the same runner protocol;
- with `--from-box <path>`, installs a twr that is already on the box, such as the one `docs/tools/dev-install.sh` builds there and puts in `~/.local/bin/twr`. The path is the box's, and absolute (a `~` is not: unquoted, your shell expands it to your home here, and quoted, it stays a `~`); on the box, `command -v twr` prints it, or `echo ~/.local/bin/twr` for the one `dev-install.sh` installs (twr warns when the path is under your home here and the box's data directory isn't). On the box, the file must be a regular file (or a link to one); twr reads its SHA-256 (the box needs `sha256sum` or `shasum`) and size, copies it into a temporary file in its build's folder under the builds directory (with the same safety checks as an upload), compares the copy's size and SHA-256 with the file's, makes it executable (mode 0700), runs its `--version` and shows it (with the same warning as `--binary`), then moves it into place;
- without `--binary`, when the hub runs a release, downloads that release's archive for the box (`twr-<target>.tar.gz`, for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin` or `x86_64-apple-darwin`) and its signed `sha256sums.txt`, checks them as `twr update` does, uploads the archive, and unpacks and checks `twr` on the box. The box needs `sha256sum` or `shasum`;
- without either, when the hub runs a development build, refuses and says how to build the same commit for the box: `TWR_BUILD=<commit> cargo zigbuild --release --locked -p twr-cli --target <target>` for Linux, then `--binary`, or `docs/tools/dev-install.sh` on the Mac, from a checkout of that commit, then `--from-box`.

It asks before it installs (`--yes` skips the question), records the build as the box's `build` in `boxes.toml`, and has the hub connect again. When the box is online on another build, the hub switches it to the new one the same way it switches a box after a hub upgrade ([A new twr build](#a-new-twr-build)): once the box has been quiet for 2 minutes. `twr box install` waits for that (up to about an hour and a half: the box quiet, then the hub's attempts, each of which may upload a build and wait for the old daemon to stop), saying `switch pending` and printing the box's notes meanwhile, and succeeds only once the box runs the build it installed. For a box in `config.toml` it prints the `build = "<id>"` line to add there; restart the hub after adding it. On a box of the hub's own OS and CPU, `twr box install` only has the hub try again now.

### Keep it running

The daemon runs in a session of its own, so it outlives the ssh connection. On Linux, when you linger on the box (`loginctl enable-linger`), the relay starts it as a transient systemd user service instead (`twr-box-<hash>-<random>`, a new name each start, so a daemon still stopping never collides with the next), outside the ssh session, and passes the relay's environment on by name. When `systemd-run` fails, the relay first checks whether the unit started anyway (no second daemon), then starts the daemon in a new session. When logind ends your processes with your last session (`KillUserProcesses=yes`) and the daemon runs in the ssh session (you don't linger, or `systemd-run` failed), the daemon and its agents end when the ssh connection does; the relay warns about it, and the warning shows among the box's notes. Run `loginctl enable-linger` on the box.

On a Mac the relay starts the daemon as a launchd job in your GUI session instead, when you are logged in at the Mac's screen ([Mac boxes](#mac-boxes)).

`TWR_BOX_LAUNCHER=setsid` in the box's environment makes the relay always start the daemon in a new session, without logind, systemd-run or launchd. `TWR_BOX_LAUNCHER=launchd` makes it always start the daemon as a launchd job, and fail rather than start it any other way (an error off macOS). Both are for tests and troubleshooting.

### Restart the box daemon

`twr box restart <box>` has the hub restart the box daemon: after you logged in at a Mac's screen, say, so that the new daemon runs in that GUI session. It says what that does and asks first (`--yes` skips the question; without a terminal, `--yes` is required), and only you can run it. The hub (`box.restart`) ends its connection to the box and connects again at once, asking the relay for a restart (`--restart`): the relay stops whatever daemon of this hub answers, whatever its build, and starts a new one, one relay at a time. The daemon's agents stop with it, as when a box's runner restarts: each thread whose agent was running shows a crash and an error card, a turn running then is cut short, and the thread's next prompt resumes its session on the new daemon. While it restarts, the box is offline and what needs it is refused with `box_offline`. Stopping the daemon may take minutes: it finishes a repository change it runs first (up to 330 seconds), and the hub gives a restart's connection time for that. The command waits as long as the hub's attempts may take (up to about 81 minutes: one under way when you ask, then the restart's own, each of which may upload a build and wait for the old daemon to stop) for the box to come back online on a new connection, printing each state and the box's notes as they come (the relay says what it waits for), then says how the new daemon was started (a launchd job, a systemd user service, or in a session of its own). On a timeout it prints the state, the last error and the box's notes, and exits non-zero.

### A new twr build

When the hub runs a new build, the box's daemon still runs the old one. If both speak the same runner protocol, the box keeps working on the old build and `twr box status` shows `switch pending`. Once the box has been quiet for 2 minutes (no turn running, no prompt queued, nothing waiting on you, no new thread being made, no attach, park, resume, deletion or trash restore), the hub sets the box `switching`: it stops the box's agents, which keeps their sessions and their queued prompts, replaces the daemon with the new build, and the next prompt to each thread resumes its session. Prompts sent meanwhile wait. New threads, parks, resumes, deletions and trash restores on the box are refused for that minute, with a message to try again. A hub restart during the switch leaves the stopped threads stopped, without a crash card; their queued prompts run. When the box still runs its old build after a switch (its daemon could not be replaced), `twr box status` says so as its last error, and the hub doesn't try that switch again until the box's build or the one to switch to changes, or the hub restarts.

When the new build speaks another protocol, the hub refuses the old daemon and replaces it at once. Threads whose agents were running get a crash card.

### Mac boxes

A Mac works as an SSH box (turn on Remote Login in System Settings, General, Sharing). What's different is where Claude Code keeps its login: in the login keychain. A process started from an ssh session can't read that keychain, so Claude Code run by a daemon started from ssh looks logged out. A process that launchd runs in your GUI session, the session of the user logged in at the Mac's screen, can.

So on a Mac, the relay starts the box daemon as a launchd job in your GUI session (`launchctl bootstrap gui/<uid>`, with a new label `tools.twr.box.<hash>.<random>` each start), when there is one. When no one is logged in at the Mac's screen, or launchd doesn't take the job, the relay starts the daemon from ssh instead, as on Linux, and says so among the box's notes: Claude Code can't read its keychain login there. Log in at the Mac, then run `twr box restart <box>`. When `launchctl` fails in another way, doesn't answer in time or isn't there, the relay can't tell whether anyone is logged in: it starts the daemon from ssh too, with a note that says so and why. When launchd doesn't take the job, the relay first checks whether the job is loaded all the same (then it waits for that daemon); when it can't tell, it boots the job out, and if it can't confirm the job is gone either, it stops with an error instead of starting a second daemon from ssh (the error names the `launchctl print` command to look at the job with).

What a Mac needs:

- **Someone logged in at its screen.** Turn on automatic login (System Settings, Users & Groups) if the Mac may restart with no one there. Logging out of the GUI session stops the daemon and its agents (launchd gives the daemon up to 330 seconds, its `ExitTimeOut`, to stop its agents and finish a repository change); the hub then connects again and, with no GUI session left, the relay starts a daemon from ssh. After you log in again, `twr box restart <box>` moves it back into the GUI session.
- **A twr build of the hub's commit for macOS.** `twr box add` says so, and the box shows `needs_install`. When the hub runs a release, `twr box install <box>` downloads that release's macOS archive. A development hub has none: build twr of the hub's commit on the Mac with `docs/tools/dev-install.sh`, then `twr box install <box> --from-box /Users/<you>/.local/bin/twr` ([Another OS or CPU](#another-os-or-cpu)).
- **The agents logged in at the Mac itself.** On a Mac, agents use their keychain or file logins. The launchd job gets only part of the ssh session's environment, by name: `PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TMPDIR`, `TZ`, `XDG_*`, `TWR_CLAUDE_BIN`, `TWR_CODEX_BIN`, `TWR_GROK_BIN`, `TWR_BOX_PROBE` and `TWR_AGENT_PROBE`. An API key set in the environment ssh commands get on the Mac (`ANTHROPIC_API_KEY`, say) doesn't reach a daemon launchd starts. A value a launchd job can't hold (not UTF-8, or a character XML forbids) is left out with a note. With `PATH` or `HOME` left out, the relay doesn't start the daemon with launchd, and starts it from ssh instead.

`twr box doctor <box>` checks a Mac's own needs in its **macOS** group ([Check a box](#check-a-box)). `twr box login <box> claude` on a Mac with no one logged in at its screen says first that the box daemon's Claude Code can't read a login made now until someone logs in there and the daemon is restarted. A login over ssh reaches the keychain only while it is unlocked, so if the login doesn't stick, log in at the Mac once. `twr attach` of a Claude Code thread on a Mac runs Claude over ssh, where it may not read its keychain login: if it asks you to log in, end the attach and continue the thread from the TUI, where the box daemon's Claude Code keeps its login ([Attach on a remote box](#attach-on-a-remote-box)).

## Container boxes

A container box is a container the hub reaches through a command from the config instead of ssh: `docker exec -i <container>`, `podman exec -i` or `kubectl exec -i`. The hub speaks the same box protocol over that command's stdin and stdout: the same probe, upload, box daemon and sealed channel. Use one to run agents on a repository you don't trust where they can't reach your files, your ssh-agent or the hub's socket ([security-model.md](security-model.md#container-boxes) says what a container does and doesn't keep out).

### Set one up

```sh
docker build -t twr-box docs/tools/container-box
docker volume create twr-sandbox-home
docker network create twr-sandbox
docker run -d --name twr-sandbox --init --restart unless-stopped \
  --network twr-sandbox -v twr-sandbox-home:/home/box \
  --env-file ~/.config/twr/sandbox.env twr-box
twr box add sandbox --command docker exec -i twr-sandbox
```

- `docs/tools/container-box/Dockerfile` is an Ubuntu 24.04 image with git, the tools twr needs and a plain user, `box`, whose home is the working directory. Build on it to add the agents' CLIs.
- `--init` runs a small init as PID 1, which reaps processes left behind (a box daemon that ends after its relay left). Without it they stay zombies.
- `--restart unless-stopped` brings the container back after a reboot or an engine restart.
- The named volume keeps the user's home: twr's builds, the box daemon's spool and worktrees, the repositories and the agents' logins. Recreating a container without one loses all of that.
- `--env-file` gives the container its secrets, such as the agents' API keys. Don't put them in `command` (`-e KEY=value`): `ps` on the hub's machine shows the command. `-e NAME` without a value copies the hub's own `NAME`.
- The network of its own gives firewall rules one place to attach ([Network](#network)).
- No bind mount of your home or of a repository, no `/var/run/docker.sock`, no `--privileged`, `--network host` or `--pid host`: each of them undoes the isolation.

`twr box add <name> --command <program> <args>...` takes the words after `--command` as the command, as they are (not through a shell), and works as for an SSH box: it runs the hub's probe through the command, says what it found and what will happen, asks (`--yes` skips the question), writes `[boxes.<name>]` with `transport = "command"` and `command = [...]` to `boxes.toml`, and waits for the box to come online. It runs the probe as the hub does, in a session of its own without a terminal, so a command that would prompt (`sudo` without `NOPASSWD`) fails here too. When the command is `docker`, `podman` or `nerdctl exec`, or `kubectl exec`, with `-i` (`--interactive`, `--stdin`), it also writes `terminal_command`: the same words with `-t` after the `-i`.

### How the hub runs the command

The hub runs `<command...> sh -c <line>`, where `<line>` is what it hands ssh for an SSH box: the probe, the upload, `exec <build> runner --stdio ...`. The words from your config never go through a shell on the hub's side, and one `sh` runs on the box, whatever the container user's login shell. So:

- The command must pass its input to the box and give the box no terminal: `-i`, and no `-t`. The probe checks both (the pipe check) on every attempt, and in `twr box add`, `twr box install` and `twr box doctor`, and says which one is wrong.
- It must not prompt. The hub runs it in a session of its own, without a terminal, from the hub's own environment: the hub's `DOCKER_HOST`, `DOCKER_CONTEXT` and groups, not your shell's, if they differ. When the hub can't reach the engine, the box's last error says so.
- The container needs `sh` and the usual tools (`uname`, `id`, `ls`, `readlink`, `wc`, `tr`, `head`, `cut`, `cat`, `mkdir`, `chmod`, `mv`, `rm`; `sha256sum` or `shasum` for the hash check), and git. Debian, Ubuntu and Busybox images have them; a distroless image doesn't.
- ssh-like programs are refused as `command` and as `terminal_command`: `ssh`, `tailscale ssh`, `az ssh`, `gcloud compute ssh`. They pass their arguments to a shell on the far end, which would split twr's scripts. Make those hosts SSH boxes: a Tailscale SSH host and a `gcloud compute config-ssh` host work with plain ssh.
- `TWR_SSH` and the ssh options don't apply.

Other engines:

```toml
command = ["podman", "exec", "-i", "twr-sandbox"]
command = ["kubectl", "exec", "-i", "-n", "sandbox", "twr-box-0", "--"]   # kubectl needs the `--`
```

For a container that runs as root, run the box as a plain user with `-u`. When the container's environment sets `HOME` (an image's `ENV HOME`, or `docker run -e HOME`), `docker exec -u` keeps that `HOME`, so pass the user's own, and its working directory:

```toml
command = ["docker", "exec", "-i", "-u", "box", "-e", "HOME=/home/box", "-w", "/home/box", "twr-sandbox"]
```

Add `-e XDG_DATA_HOME=...`, `-e XDG_STATE_HOME=...` and `-e XDG_CONFIG_HOME=...` when the image sets those for root. Without them, the hub's upload fails, and the box's last error says that its `HOME` isn't writable for its user.

A development build of twr is linked against the C library (glibc) of the machine that built it, so the hub's own build runs only in a container with that glibc or newer: ubuntu:24.04 has 2.39, Debian bookworm's 2.36 is older, and Alpine has none. The box's last error says so. A release build is static and runs in any Linux container. A container of another CPU (an arm64 image under emulation) needs `twr box install`, as an SSH box does ([Another OS or CPU](#another-os-or-cpu)): every upload goes through the command too.

### What twr does with the container

twr never creates, starts, stops or removes a container. Those are yours to do.

- **Stopped** (`docker stop`, a reboot, the engine restarting): the box daemon, its agents and the relay end with the container. The box goes offline, and its last error says the container is stopped. The hub tries again on its usual schedule ([What the hub does](#what-the-hub-does)). Once the container runs again, the next attempt starts a new daemon, and threads whose agents were running get a crash card, as after a box reboot. Their next prompt resumes the session.
- **Recreated** (`docker rm`, then `docker run`): what wasn't in a volume is gone. The hub uploads its build again and the daemon starts empty.
- **Gone:** the box's last error says the container doesn't exist. `twr box remove` takes the box out of `boxes.toml` and prints the command that stops the daemon (`docker exec -i twr-sandbox sh -c '...'`). It leaves the container as it is.
- **The hub ends a connection** by killing the command's process group. The `docker exec` client ends, the relay in the container reads the end of its input and ends, and the daemon runs on. There are no ssh keepalives, but the runner protocol's pings end a connection that hangs.

`twr box status` doesn't show the command. `twr box doctor` shows it to you, and to an agent only the program and how many words the command has.

### Agents in the container

The agents' CLIs and their logins live in the container. Install them in the image (or as the box's user in the running container), then log them in there: `twr box login sandbox claude` runs the login through `terminal_command` in your terminal, and `docker exec -it twr-sandbox claude auth login` does it directly. API keys go in the container's environment (`--env-file`). An agent working on the repository can read whatever logins and keys the container has, so use keys you can revoke and cap.

### What needs a terminal

`terminal_command` is used by `twr box login` and `twr box project --clone`. Without one, both are refused, saying how to set it. A `docker exec -t` client can go away while what it ran goes on: Docker's detach keys (Ctrl-P Ctrl-Q) end the client, and so does killing it.

- Detaching from `twr box login` leaves the login running in the container. It holds no twr state. Run `twr box login` again, or end it with `docker exec twr-sandbox pkill -f 'claude.*login'`.
- A clone whose client went away runs to its end in the container. If it fails, it removes its folder, as always. If it succeeds, the folder stays, unrecorded, and `twr box project <box> <project>` finds it (or record it with `--path`). Ctrl-C in your terminal still stops the clone and removes the folder.

`twr attach` to a thread on a container box is refused for now: twr can't tell when an attach through `docker exec -t` ends, since the client can go away while the agent's CLI runs on in the container. Continue the thread from the TUI or with `twr send`.

### Network

Without mounts, the box daemon and its agents can't reach the hub's socket, your files, your ssh-agent or the hub's box key. What they reach on the network depends on the engine and its settings:

- Docker Desktop (macOS, Windows, and Docker Desktop on Linux): `host.docker.internal` reaches services on the host's loopback, the hub's runner listener included (which still needs a box token).
- Docker Engine on Linux: the bridge's gateway (`172.17.0.1` on the default bridge) reaches host services that listen on all interfaces or on that address. `--add-host host.docker.internal:host-gateway` gives it a name.
- Podman: `host.containers.internal`; with pasta or slirp4netns, whether the host's loopback is reachable depends on their options.
- `--network host`: everything the host reaches.

twr doesn't check any of this. If keeping the host's services out matters, filter the traffic yourself. On Linux (Docker Engine) that takes two sets of rules, since the container's traffic takes two paths:

- **To the host itself** (the gateway address, or any other address of the host): this traffic goes through the host's input path, which Docker's `DOCKER-USER` chain never sees. Filter it on the network's bridge interface. Name the interface when you create the network (`docker network create -o com.docker.network.bridge.name=twr-sb0 twr-sandbox`), or find it (`br-` and the start of the network's ID). Then drop new connections from it: `iptables -I INPUT -i twr-sb0 -m conntrack --ctstate NEW -j DROP`, or the same in an nftables chain on the `input` hook. For anything the container needs from the host, insert an accept rule after that one (`-I` puts each new rule first).
- **Through the host to other machines** (your LAN, the private ranges): this traffic is forwarded. Add rules to Docker's `DOCKER-USER` chain, or an nftables chain on the `forward` hook, that drop traffic from the network's subnet to the private ranges and allow what the agents need (their APIs, a package mirror).

A network with IPv6 needs the same rules for IPv6 (`ip6tables`, or an `inet` table in nftables). These are starting points, not a complete firewall: try them from the container (`docker exec twr-sandbox curl <a host service>`) and make them persist across reboots your usual way. On Docker Desktop, use its settings, or give the container no network and the agents an egress proxy for their APIs.

## Dial-in boxes

A box the hub can't reach over SSH (behind NAT, or one you'd rather not give the hub an ssh key to) is a dial-in box: it runs `twr runner`, which connects out to the hub's runner listener over WebSocket and runs that box's threads. The hub needs no inbound access to the box. Hub and runner share a token, which both prove they know without ever sending it.

```sh
# on the hub machine (once: [hub] listen set and served to the box, below)
twr box add gpu --dial-in --hub wss://hub.tail1234.ts.net:7878

# on the box
twr runner install-token gpu --hub wss://hub.tail1234.ts.net:7878
```

### The listener, once

The box's runner connects to the hub's runner listener, which is off until you set it, in `~/.config/twr/config.toml`:

```toml
[hub]
listen = "127.0.0.1:7878"
```

Restart the hub (`twr hub stop`, then `twr`), and make the listener reachable from the box. `twr box add --dial-in` refuses without `listen`, before it writes anything, and says this. Three ways, the first one recommended:

- **Tailscale**, on both machines, with HTTPS certificates turned on for the tailnet: `tailscale serve --bg --tls-terminated-tcp=7878 tcp://127.0.0.1:7878` on the hub machine; the box then dials `wss://<hub machine>.<tailnet>.ts.net:7878` ([Over Tailscale](#over-tailscale)).
- **TLS on the listener** (`[hub] tls`, [Transport and TLS](#transport-and-tls)): the box dials `wss://<a name in the certificate>:7878`, with `--ca` for a private CA.
- **An ssh tunnel from the box** to the hub (`ssh -N -o ExitOnForwardFailure=yes -L 7878:127.0.0.1:7878 me@hub` on the box): the box dials `ws://127.0.0.1:7878`. The tunnel must stay up, as a service of its own.

### Add the box

`twr box add <name> --dial-in --hub <url>` on the hub machine:

1. Checks `--hub` with the runner's own rule, without a way around it: `wss://`, or `ws://` to a loopback address (an ssh tunnel's end). A plain `ws://` to any other address is refused, and so is `ws://` when the listener serves TLS. Without `--hub` it says which URLs fit the config. `--ca <path>` (with `wss://`) names the CA certificate the runner trusts, by its absolute path on the box.
2. Says what will happen and asks `Add box gpu? [y/N]` (`--yes` skips the question; without a terminal, `--yes` is required): where the runner connects, that a new token goes into `boxes.toml`, and which twr the box needs (the hub's version: `twr update --version v<version>` there; for a development hub, a build of its commit).
3. Writes `[boxes.gpu]` with `transport = "token"` and a new token (32 random bytes, in hex) to `boxes.toml` (mode 0600), and adds the box to the running hub (`box.add`), starting the hub when none runs. A hub that started before `listen` was set says so: restart it.
4. Prints the command to run on the box, `twr runner install-token gpu --hub <url>`, and, only when its output is a terminal, the token to paste there. Otherwise it says that `twr box token gpu` prints it.
5. On a terminal, waits up to 10 minutes for the box's runner to connect, printing each state and each refusal with what to do: a refused proof is a bad paste (`twr box token gpu` prints the token again), a refused protocol means twr on the box needs updating. Ctrl-C stops the wait; the box stays added.

### On the box

`twr runner install-token <box> [--hub <url>] [--ca <pem>] [--token-file <path>] [--yes]` stores the box's token for its runner:

- On a terminal it asks for the token at a prompt that does not show it (`/dev/tty`, echo off before the prompt appears, so nothing typed ahead is taken); paste it there. Ctrl-C cancels, and nothing is stored. Without a terminal it reads the token from stdin: on a box that can ssh to the hub, `ssh me@hub twr box token gpu | twr runner install-token gpu --yes`. The token must be 16 to 1024 printable ASCII characters without spaces. It is never shown, logged or put on a command line.
- It writes the token to `~/.config/twr/runner-tokens/gpu.token` (`$TWR_HOME/runner-tokens/` with `TWR_HOME`), mode 0600 in a directory of mode 0700, atomically: a runner reading it sees the old token or the new one. `--token-file` writes it elsewhere (its directory must exist). The same token again is "unchanged". A file that holds another token is replaced only once you confirm, or with `--yes`: it is usually the same hub's box with a new token, but two hubs that both name a box `gpu` would collide on that machine (its token file, spool, service name), so give the second hub's box another name (`twr box add gpu-work --dial-in ...`).
- With `--hub`, it prints the command that runs the runner, and a service to paste that keeps it running: a systemd user unit on Linux (when `systemctl` is there), a LaunchAgent on a Mac. Both name the token file and, when the data directory is not the standard one (`TWR_HOME`, `XDG_DATA_HOME`), `--data-dir`, by absolute paths. twr writes no service file and runs no `systemctl` or `launchctl` itself: what runs is what you pasted. When a path can't be written safely into the unit or the plist (a newline, a quote), it says to write the service yourself.

`twr runner --hub <url> --box=gpu` reads the token from `--token-file`, else `TWR_RUNNER_TOKEN`, else the box's default file, `runner-tokens/<box>.token`. A token from a file is read again after the hub refuses the runner (or can't prove it knows the token): while the runner waits to try again (60 seconds), it looks at the file every 2 seconds, and connects at once with a new token there. So after a rotation, installing the new token on the box is enough; no restart. A file that fails the checks (another owner, group or other permission bits) is logged, and the old token kept.

The systemd unit, as pasted, runs `systemctl --user enable --now twr-runner-gpu.service` and `loginctl enable-linger`, so the runner runs without a login session. `systemctl --user restart twr-runner-gpu.service` restarts it; `journalctl --user -u twr-runner-gpu.service` shows its log. On a Mac, the LaunchAgent (`tools.twr.runner.gpu`, its log in `~/Library/Logs/twr-runner-gpu.log`) runs in your GUI session, where Claude Code can read its keychain login, so it runs while someone is logged in at the Mac's screen, as [Mac boxes](#mac-boxes) need; `launchctl kickstart -k gui/$(id -u)/tools.twr.runner.gpu` restarts it. Pasting the snippet again replaces the service: the Mac snippet boots the old LaunchAgent out first (its agents stop with it). On Linux, a runner that is running already keeps its old settings (`--hub`, `--ca`, the token file) until it restarts, so the snippet doesn't restart it: it says so, with the `systemctl --user restart` command to run when the box's agents may stop.

### Tokens, rotation and removal

| Command | What it does |
| --- | --- |
| `twr box token <name>` | prints the box's token (from `boxes.toml`, or `config.toml` for a box configured there), and nothing else on stdout, to a pipe too |
| `twr box token <name> --rotate [--yes]` | writes a new token to `boxes.toml` (only while the box there is the one it read, its token included) and has the running hub take it at once: the hub stops accepting the old token and drops the box's connection. The box stays offline until you install the new token there (`twr runner install-token gpu`); its agents keep running meanwhile. Prints the new token on a terminal only |
| `twr box token <name> --reload` | has the running hub take the box's token from the files again, after you edited `config.toml` |
| `twr box remove <name>` | takes the box out of `boxes.toml`, and the running hub stops accepting its runner at once and drops its connection (the hub goes by the box it runs, whatever the removed table's `transport` says). It prints how to stop the runner on the box (on a Mac, also removing its LaunchAgent's plist, which launchd would load again at the next login) and delete its token. When the box is in `config.toml` too, the hub keeps accepting it and the command says so |

The hub's token for a box is always the one the files give it, or none: rotation and removal never need a hub restart, and a handshake that proved the old token is refused once the token changed. The hub keeps the box's threads until it restarts. A box in `config.toml` is yours to edit: `--rotate` refuses it and says to put a new token there (`twr runner token` prints one), then run `twr box token <name> --reload`.

When the running hub doesn't take a rotation or a removal (it can't read the files, say, or is older than `box.token`), the command fails, its change to `boxes.toml` kept, and says what to run: `twr box token <name> --reload` once the files read, or a hub restart. A hub that is starting doesn't miss the change: it reads the files under `boxes.toml`'s lock and holds it until its socket is bound, so these commands wait for it and then ask it. Nor does a hub that is shutting down: it removes its socket first and accepts runners until it exits, so these commands, finding no socket, wait for its exit (at most 30 seconds, then they fail, the change kept).

### Versions and updates

The hub can't reach a dial-in box, so it can't install its build there. The runner must speak the hub's runner protocol; a runner of another protocol is refused, and the box shows the refusal (`twr box status`, `twr box doctor`). After a hub upgrade that changes it, update twr on the box (`twr update --version v<the hub's version>`, or for a development hub a build of its commit with `docs/tools/dev-install.sh`) and restart the runner. Threads whose agents were running get an error card, as after any runner restart. Another build of the same protocol works; `twr box doctor` warns about it, with the same fix.

`twr box doctor <box>` on a dial-in box checks what the hub can see: its view of the box (state, version, a refusal, each with the fix on the box's side), whether the hub listens for runners, the box's token (there, and long enough; never shown), and the agent CLIs as the box's runner sees them, asked again through the hub while it is connected. The box's projects are not checked: twr can't look there. `twr box project <box> <project> --path <path>` records a repository's path on a dial-in box without checking it; a wrong one fails the thread at its start. `twr box login` says to log in on the box itself. `twr box install` and `twr box restart` say what to run on the box.

### By hand

A token box you set up yourself, in `config.toml`. On the hub machine, turn on the listener and give the box a token:


```toml
[hub]
listen = "127.0.0.1:7878"

[boxes.gpu]
token = "..."                  # from `twr runner token`; hub and runner prove they both have it
ssh = "me@gpu-box"             # used by `twr attach` for threads on that box

[boxes.gpu.projects.api]
repo = "/srv/code/api"         # where project api lives on the box
```

`twr runner token` prints a new token: 32 random bytes, in hex. Put the same token in the hub's config and on the box, with `twr runner install-token gpu` (which `twr runner` finds by default) or in a file of your own (`--token-file`). A token shorter than 16 characters keeps the hub from starting (other commands, `twr hub stop` included, still work), and one shorter than 32 gets a warning in its log and in `twr info`; the runner refuses and warns the same way. If an upgrade finds a short token, replace it in both places with one from `twr runner token`, then restart the hub (`twr hub stop`, then `twr`) and the runner. The config file now holds a secret, so make it private with `chmod 600 ~/.config/twr/config.toml`. The hub warns at start when a config holding a box token or a `[notify]` command can be accessed by other users (any group or other permission bit).

Restart the hub, then connect the box. The simplest transport keeps the listener on loopback and tunnels it over SSH:

```sh
# on the hub machine
ssh -o ExitOnForwardFailure=yes -R 7878:127.0.0.1:7878 me@gpu-box

# on the box: the token at install-token's prompt, then the runner (which reads it from
# ~/.config/twr/runner-tokens/gpu.token; or --token-file, or TWR_RUNNER_TOKEN)
twr runner install-token gpu
twr runner --hub ws://127.0.0.1:7878 --box=gpu

# anywhere
twr thread new /srv/code/model --box gpu
twr thread new api --box gpu
```

`-o ExitOnForwardFailure=yes` makes `ssh` give up when it cannot listen on the box's port. Without it, `ssh` only warns, and whatever already listens on that port gets the runner's connection: another user's process, say. If that process can't reach the hub's listener, it learns nothing. Hub and runner never send the token. Each proves it has it by signing the other's random challenge (HMAC-SHA256), and the runner checks the hub's proof before it sends anything else. A runner that gets no valid proof logs that the hub could not prove it knows the token and tries again later. But your box stays offline until the tunnel is up. If the process can reach the hub's listener, for example one that serves TLS to the network for other boxes, it can pass the handshake back and forth between runner and hub. The proofs don't name the connection they travel on, so both sides accept, and the process can then send the runner commands. So use `ExitOnForwardFailure`, and keep the listener reachable from as few machines as you can.

The runner refuses a token file other users can access (any group or other permission bit) or that is not a regular file; `chmod 600` it. It never takes the token on the command line, which every local user can see. The runner keeps its spool and worktrees in its data directory, or in `--data-dir`.

One runner per box runs on a machine. A runner locks its spool, its worktrees directory and a lock file for its hub and box in `$XDG_STATE_HOME/twr/runner-locks/` (by default `~/.local/state/twr/runner-locks/`), which stays put whether the runner was started from a login session, a systemd service or cron. A second runner of the same box for the same hub on that machine refuses to start, whatever its `--data-dir`. Two of them would kill each other's runtimes as leftovers and run repository changes over each other. The lock goes by the `--hub` URL as written, so give every runner of a box the same one.

#### Keep the runner running

The tunnel and the runner above end when you log out. To keep a box connected, run both as systemd user services. On the hub machine, the tunnel (it needs a key ssh can use without asking, from `ssh-agent` or without a passphrase):

```ini
# ~/.config/systemd/user/twr-tunnel-gpu.service
[Unit]
Description=twr runner tunnel to gpu

[Service]
ExecStart=/usr/bin/ssh -N -o BatchMode=yes -o ExitOnForwardFailure=yes -o ServerAliveInterval=15 -o ServerAliveCountMax=3 -R 7878:127.0.0.1:7878 me@gpu-box
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
```

On the box, the runner:

```ini
# ~/.config/systemd/user/twr-runner-gpu.service
[Unit]
Description=twr runner for box gpu

[Service]
ExecStart=%h/.local/bin/twr runner --hub ws://127.0.0.1:7878 --box=gpu --token-file %h/.config/twr/runner-tokens/gpu.token
# A service reads no shell startup file. The runner asks your login shell for its PATH
# (when SHELL is set and is sh, bash, zsh, ksh or fish); otherwise put every agent CLI's
# folder on PATH here, or set TWR_CLAUDE_BIN, TWR_CODEX_BIN or TWR_GROK_BIN to its full path.
Environment=PATH=%h/.local/bin:/usr/local/bin:/usr/bin:/bin
# Stop the runner first and let it stop its runtimes and finish a repository change (up to
# 5 minutes) before systemd kills what is left.
KillMode=mixed
TimeoutStopSec=330
Restart=always
RestartSec=10

[Install]
WantedBy=default.target
```

Enable each with `systemctl --user daemon-reload` and `systemctl --user enable --now <name>`, and run `loginctl enable-linger` on both machines so user services run without a login session. `systemctl --user stop` stops the runner the way Ctrl-C does: it stops its runtimes and lets a repository change in progress finish. `KillMode=mixed` makes systemd signal only the runner at first; the default would signal the runtimes and git at the same moment. Check that the agents start with `systemctl --user status twr-runner-gpu` and `journalctl --user -u twr-runner-gpu`: an agent the service can't find fails its thread's start with "not found on PATH". `twr box list` shows which agents the runner found once it is connected.

#### Over Tailscale

With [Tailscale](https://tailscale.com) on both machines, the hub's listener can stay on loopback while Tailscale serves it to your tailnet with a TLS certificate. On the hub machine, with HTTPS certificates turned on for the tailnet:

```sh
tailscale serve --bg --tls-terminated-tcp=7878 tcp://127.0.0.1:7878
```

The runner then connects with `--hub wss://<hub machine>.<tailnet>.ts.net:7878` and needs no tunnel. It checks the certificate against the public web roots, which Tailscale's certificates chain to. Every device on the tailnet can reach that port, so the token is what keeps others out; narrow it with the tailnet's access rules. The hub sees these connections as coming from loopback, so only the overall limit on unauthenticated connections (16) applies to them, not the per-address one. `tailscale serve --tls-terminated-tcp=7878 off` stops it.

A project whose threads should always run on the box can say so with `[projects.<name>] box = "gpu"`. Without a `[boxes.<box>.projects.<name>]` entry, the project's `repo` is taken as a path on the box.

### Transport and TLS

The hub refuses a non-loopback `listen` address unless you choose how to protect it:

```toml
[hub]
listen = "0.0.0.0:7878"
tls = { cert = "/etc/twr/hub.pem", key = "/etc/twr/hub.key" }   # serve wss://
# or, on a network you trust:
# allow_insecure_remote = true   # plain ws://; events cross in clear text
```

With TLS the runner connects to `wss://host:7878`. It trusts the public web roots, or only the CA you give it with `--ca ca.pem`, for a private or self-signed CA. An SSH tunnel or Tailscale with a loopback listener is the recommended setup.

The runner has a matching rule. It connects over plain `ws://` only to a loopback host: `localhost`, an address in `127.0.0.0/8`, `[::1]`, or an IPv4-mapped loopback address such as `[::ffff:127.0.0.1]`, which is where an SSH tunnel ends. For any other host, every event and command would cross the network in clear text, and someone on the path could take over the connection once it is authenticated, so the runner refuses to start and suggests `wss://` or a tunnel. It only checks the host as written: a name that resolves to a loopback address doesn't count. On a network you trust, `--allow-insecure` lifts the refusal.

## Agents on a box

A box tells the hub which agent CLIs it has (`claude`, `codex`, `grok`), at which version, and whether each is logged in: `twr box list`, `twr box status <box>`, and `hub.info` for the TUI and the orchestrator. The runner probes them as it starts and every 10 minutes, and the hub asks again 30 seconds after the box connects and every 10 minutes after that. Each probe runs `<cli> --version` and the CLI's own status command (`claude auth status`, `codex login status`; Grok Build has none, so its login shows as not known), each for at most 10 seconds, without a terminal. twr keeps whether it is logged in and, for Claude Code, how (`claude.ai`, `console`, `api-key`, `bedrock`, `vertex`). It never keeps or shows the account's email, organization or plan.

A thread started on a box that reports its runtime's CLI missing is still made, with a warning: "box gpu reports no `claude` on its PATH; the thread may fail to start: `twr box doctor gpu`". The same warning comes with whatever starts or restarts the runtime there: a move or new model (`twr model`), a new permission mode (`twr mode`), and a prompt to a thread whose runtime isn't running (`twr send`). A thread or project `env` can give it a `PATH` of its own.

### The login shell's PATH

A runner started by ssh, systemd or cron has a thin `PATH`: no `~/.local/bin`, no npm or nvm folder, and many `.bashrc` files return early when the shell is not interactive. So the runner (an SSH box's daemon, or `twr runner`) asks your login shell for its `PATH` once, in the background as it starts: `$SHELL -lic` with a `printf` of `$PATH` for `sh`, `dash`, `bash`, `zsh`, `ksh` and `mksh` (interactive too, so a `.bashrc` that stops early for other shells runs), and `fish -lic` with `string join : $PATH`. The shell gets no terminal and no input, and 5 seconds. It prints the `PATH` between two markers made of a fresh random value; anything else your startup files print is ignored, and a value with a control character is refused. Its absolute entries (at most 128) go before the runner's own `PATH`, each once, in every runtime's environment. A thread's or project's `env` `PATH` still wins, and `TWR_CLAUDE_BIN`, `TWR_CODEX_BIN` and `TWR_GROK_BIN` still come first. The runtimes, the agent probe, `twr box login` and `twr attach` on an SSH box all resolve the CLI the same way. On a token box, `twr attach` runs the CLI from the `PATH` an ssh command gets there; so does an SSH box whose runner did not say which twr build it runs (a runner of an older twr): the hub logs that once for the box.

Its limits:

- `csh` and `tcsh` are not asked (their syntax differs), nor is a shell twr doesn't know. The runtimes then get the runner's own `PATH`: put the agents there, or name them with `TWR_*_BIN`.
- Without `SHELL` set (some service managers and `env -i` leave it out), nothing is asked.
- A login shell that takes longer than 5 seconds, or prints more than 16 KiB before the `PATH`, is not waited for.
- It is read once per runner start. An agent installed in a folder already on it is found at once; for a new folder, restart `twr runner`. An SSH box's daemon reads it again when it next starts (after a new twr build, or a reboot of the box). `twr box doctor` asks the login shell afresh, so it can find an agent the daemon doesn't yet.

The hub's own box (`local`) asks no login shell: its runtimes get the hub's environment, which a hub started from your shell already has.

### Check a box

`twr box doctor <box>` says what is wrong with a box and how to fix it. It works without a hub and with the box offline: each check prints as it ends, `✓` when fine, `!` when worth a look, `✗` when it failed, with a `fix:` line where there is one. It exits 1 when a check failed. `--json` prints the checks (`{check, item, result: ok|warn|fail, message, fix}`), the build it ran (`{id, source: hub|configured|newest}`), the login shell's `PATH` report and the agents.

- **hub.** When a hub runs, what it says of the box: its state and last error, its twr version and build against the hub's, a pending switch, a refused connection.
- **ssh without prompting.** ssh to the box as the hub runs it (`BatchMode`).
- **ssh in this terminal.** The hub's probe, run with ssh in your terminal, so ssh may ask about the host key or for a passphrase. It finds the box's OS and CPU, its data directory, whether the builds folder is safe to use, and the builds there.
- **build.** The twr build on the box that runs the checks below: the hub's when it's there, else the box's configured `build`, else the newest one there. Doctor says which. When something answers on the hub's socket but does not say which build it runs, the hub's build is unknown: the configured or newest build runs the checks, and doctor says why.
- **command** and **pipe** (container boxes, instead of the two ssh checks). The same probe, run through the box's command as the hub runs it: on a failure, the command's last line and its fix (a container that doesn't exist or is stopped, an engine the command can't reach, no `sh`). Then whether the command passes its input to the box without a terminal (`-i`, no `-t`).
- **daemon** (SSH and container boxes). Whether this hub's box daemon runs (`twr runner --stdio --status`), with its build and runner protocol.
- **logind** (Linux). Whether you linger, whether logind ends your processes with your last session, and whether the running daemon is in its systemd user service (an active `twr-box-<hash>-*` unit). Lingering alone is not enough: when `systemd-run` failed, the daemon runs in a session, and if logind ends your processes it ends with the hub's ssh.
- **macOS** (a Mac). Whether someone is logged in at the Mac's screen (`launchctl print gui/<uid>` over ssh): if not (it exits 113), it fails, with the fix to log in there or turn on automatic login, then `twr box restart <box>`; a `launchctl` that fails otherwise, or isn't there, is a warning that it couldn't tell. Then how the running daemon was started, as it told the hub (`hub.info` `launch`), only while the box is connected (not known without a hub, or while the box is offline: the hub keeps what the last daemon said): a launchd job is fine, a daemon started from ssh is worth a look, since Claude Code can't read its keychain login there (`twr box restart <box>`). For Claude Code on a Mac, the agent check below takes its login from the box daemon over the probe's over ssh, while the box is connected: the hub has the box probe its agents again first (as `twr box login` does, at most 45 seconds; when that fails, as the daemon last reported them, said so), and the check says when the two differ: the keychain answers a daemon in the GUI session, and ssh only while it is unlocked. A box that isn't connected gets the probe's over ssh, saying the daemon's own view isn't known. A daemon that doesn't find Claude Code while ssh does (installed since the daemon started) is a warning too, with `twr box restart <box>` as the fix. For a token box these speak of the runner you start on the Mac, not of a box daemon.
- **login shell** and each agent. `twr runner --agents` on the box says whether the login shell gave its `PATH`, and each CLI's path, version and login. The fix says how to install it or log it in.
- **git** and **disk.** `git --version` on the box, and the free space where it keeps its data, against `[threads] disk_warn_gb`.
- **project.** Each project configured for the box (`[boxes.<box>.projects.*]`, and `[projects.*]` with `box = "<box>"`) has its repository there, and it is a git repository.

What ssh and the box print is bounded and shows with its control characters escaped, as for `twr box add`. Each command doctor runs on the box (`loginctl`, `busctl`, `launchctl`, `git`, `df`, `systemctl`) gets at most 10 seconds, and the box's twr at most 20 seconds for the daemon's status and 40 for the agent probe (with the box's `timeout`, else killed in the background), so one that hangs ends with doctor. What the box reports of its agents is filtered as the probe filters it (see `hub.info` `agents`), whatever twr build reported it.

### Log an agent in on a box

`twr box login <box> <agent>` (`claude`, `codex` or `grok`) starts the agent's own login on the box, in your terminal: `claude auth login`, `codex login --device-auth` or `grok login --device-auth` (a device code works over ssh, where no browser opens). twr never runs a login by itself and never sees what you type: ssh runs with a terminal (`-t`), with forwarding off as for the hub's ssh, and your terminal talks to the agent's login directly. On the box, the twr build `twr box doctor` would pick runs `twr runner --login <agent>`, which finds the CLI as the box's runner would (`TWR_*_BIN`, then the login shell's `PATH`) and runs its login in place of itself.

It needs a terminal on stdin and stdout, and only you can run it: an agent's `twr box login` is refused. When the login ends, it asks whether the agent is logged in: the box daemon, through the hub (`box.agents` with `refresh`), when a hub runs and has the box, else the box over ssh. It says when that disagrees with how the login ended, and exits 0 once the agent is logged in, or, for an agent without a status command (Grok), once a login that ended without an error leaves the CLI found. It exits 1 when the daemon doesn't find the CLI. That happens when you installed it, or changed your startup files, after the daemon started: the daemon reads the login shell's `PATH` only when it starts. The message then says to run `twr box restart <box>`, whose new daemon reads the `PATH` afresh (a token box's runner you restart yourself).

On a Mac, Claude Code keeps its login in the login keychain, which a login over ssh reaches only while it is unlocked. If the login doesn't stick, log in at the Mac once. With no one logged in at the Mac's screen, `twr box login` says first that the box daemon's Claude Code can't read a login made now until someone logs in there and the daemon is restarted (`twr box restart <box>`); see [Mac boxes](#mac-boxes).

## Projects on a box

A thread on a box needs the project's repository on that box. `twr thread new api --box gpu` uses `[boxes.gpu.projects.api] repo`, else `[projects.api] repo` taken as a path on the box, else an absolute path you give. It never guesses where else the repository might be: when the box doesn't have it, the error says to run `twr box project gpu api`.

```sh
twr box project gpu api                        # find it on the box, ask, record it
twr box project gpu api --path /srv/code/api   # record that repository
twr box project gpu api --clone                # clone its origin on the box, in your terminal
twr box project gpu api --clone --into /srv/api-copy   # ... into that new folder
twr box project gpu api --remove               # forget it
```

`twr box project <box> <project>` takes a project you configured (`[projects.api]`) or a repository folder name under `project_roots`, as `twr thread new api` finds it on the hub's machine. It reads that repository's `origin` with twr's git settings (no network). An origin with a password or a token in it is compared without them, and they are never sent to the box; one with more than one `@` before its host, or an `@` after it, is not compared at all (twr cannot tell where a password holding `@` or `/` ends), and twr never shows any part of it but the redacted URL.

Then it probes the box over ssh in your terminal, as `twr box doctor` does, and has the box's twr build (the one doctor would pick) search the box, with no network access:

1. the path the repository has under your home here, under the box's home: `~/code/api` here is looked for at `~/code/api` there, whatever each home is;
2. then `<folder>` and `<org>/<folder>` under the box's `~/code`, `~/src`, `~/projects`, `~/dev`, `~/Developer`, `~/repos`, `~/git` and `~/workspace`, by the rules of the hub's `project_roots` (git repositories only, hidden folders skipped, at most 2048 folders).

It lists each repository it found with its `origin` (credentials taken out, by the box's twr and again here; what the box says shows with its control characters escaped). When exactly one has the project's origin and the search was complete, it asks `Record <path> as project api on box gpu? [y/N]` (`--yes` skips the question) and records it. Otherwise it records nothing, says why (several match, none does, the search was incomplete, the project has no origin), and you pick one with `--path`. A search is incomplete when it stopped early (too many folders or repositories, or 90 seconds passed: the box side then stops, its git killed, and reports what it found), when the box has no absolute `HOME` (its project folders can't be searched: the command says the search couldn't run), when a folder could not be read (or could be listed but not entered, or its name isn't UTF-8), when git could not tell whether a folder is a repository (a folder with a `.git` that git finds no repository in, a broken `.git` file say) or could not read a repository's `origin` (listed as such: it may be the project's), or when the list was too long to send back in full. A box without git (none on the `PATH` ssh gives the box's twr) is said to be one, and nothing is recorded. git's own message is never shown or logged then: it may quote the repository's config, credentials included. A very long origin is shown cut. When it finds none, it suggests `--clone`.

Two origins match when they name one repository: the same host (in any case), port and path, a trailing `/` or `.git` aside. `user@host:path` is relative to that user's home on the host, so it differs from `ssh://host/path`, except on github.com, gitlab.com and bitbucket.org, where `git@github.com:o/r`, `ssh://git@github.com/o/r` and `https://github.com/o/r` are one repository.

That match is by text, a limit to know. The host is compared as written, not as it resolves: an ssh alias from `~/.ssh/config`, or a short DNS name, may name one machine here and another on the box, and two names of one machine don't match. And `repo` and `repo.git` count as one repository, which a server could serve as two. So a match is a strong hint, not a proof: the question before recording shows the path and its origin for you to check, and `--yes` skips that question, so use it where you trust the box's origins.

`--path <path>` records that path once the box says it is a repository's top level. A folder inside a repository is refused, and so is a path that isn't absolute (write the box's home out: a `~` would be your shell's home here).

`--clone` clones the project's origin on the box. It needs a terminal on stdin and stdout, and an `https://`, `ssh://` or `user@host:path` origin without credentials: it refuses `file://`, a local path, any other scheme, an origin with a password or a token, and one twr cannot tell from one (more than one `@` before the host, an `@` after it), and says to clone the repository on the box yourself and record it with `--path`. `--into <dir>` names the clone's own folder, as `git clone <url> <dir>` does (`--into /srv/api-copy` clones into `/srv/api-copy`; its name is letters, digits, `-`, `_` and `.`). Without it, the clone goes to the repository's home-mapped path (`~/code/api` for `~/code/api` here), else `~/code/<the repository's folder name>`. The folder that holds the clone must exist, be yours and not be writable by others; the clone's own folder must not exist yet, and twr makes it with mode 0700 (yours alone, whatever your umask). It asks first (`--yes` skips the question, not the terminal). Then ssh runs with a terminal (`-t`) and forwarding off: git runs on the box in your terminal, with the box's own git credentials (its ssh keys, its credential helpers), and asks you there for what it needs. twr forwards neither your ssh agent nor any credential, lets git use only the https and ssh transports (a `url.<base>.insteadOf` in the box's git config that rewrites the URL to another one is refused by git), and never fetches into the clone afterwards. Git LFS fetches nothing during the clone (`GIT_LFS_SKIP_SMUDGE=1`: its server is one the repository's own `.lfsconfig` may choose, outside the https/ssh allowlist), so LFS files are pointers until you run `git lfs pull` in the clone yourself. git runs as the terminal's foreground process group, so Ctrl-C stops it and the helpers it started. A hangup (the ssh connection drops, the terminal closes) or a SIGTERM to the box's twr stops the clone too: git and its helpers get the hangup (forwarded) and SIGTERM, 5 seconds to end, then SIGKILL. A clone that fails, or is stopped, has git and every helper it started ended first, then removes the folder it made with all git wrote in it, as long as it is still that folder (the same one, yours, mode 0700; symbolic links in it are removed, not followed); a folder put in its place is left alone. A clone git says it made is recorded only while it is still that folder and a repository's top level.

Recording writes `[boxes.<box>.projects.<project>] repo = "<path>"` to `boxes.toml`, under its lock, and only while the box's settings and that entry are what they were when the command started: when another command changed either meanwhile, it refuses, and you run it again. For a box you configured in `config.toml`, it prints the lines to add there. Then the running hub reads the box's projects again (`box.projects`), and the next `twr thread new api --box gpu` uses the repository: the command says `recorded and active` when the hub's repository for the project is the one it wrote. When the hub has another one (the file changed again before the hub read it: another `twr box project`, an edit), it says `recorded`, what the hub has, and why. `recorded` alone means no hub runs (a hub reads it when it starts) or the hub didn't take it, and says why; when the box's ssh destination changed in the files since the hub added the box, restart the hub. `--remove` takes the entry out the same way.

Only you can run it: an agent's `twr box project` is refused, and so is one in a named workspace. `twr box add` ends by naming `twr box project <box> <project>` for each project configured with `box = "<box>"`.

## When the connection drops

The runner spools events to disk while it's disconnected and replays them in order when it reconnects. The hub records each event exactly once, at the time the runner captured it, so a replayed backlog keeps its original times.

The runner retries at most 5 seconds apart during the first minute of an outage, then backs off to 30 seconds. While a box is offline its threads keep their state and their runtimes keep running on the box. Prompts queue until it's back. `twr stop` is recorded and goes out when the box reconnects. Creating, parking, resuming or attaching a thread on an offline box, and answering its cards, fail with `box_offline`. A change to a repository on a box (creating, parking, deleting or restoring a thread's worktree) waits up to 10 minutes for the box's answer; one left unanswered (the box went offline or did not answer in time) has an unknown outcome, and no other git command runs on that box until the runner answers it or reconnects. A runner reconnects only once the repository changes it was running have finished.

After a reconnect the hub and the runner reconcile. A thread the log shows running whose runtime the runner no longer has is closed with an error card. A session the runner runs but the hub doesn't know is stopped. After a hub restart the hub takes over the sessions the runner still runs. A hub shutdown stops local runtimes only.

A second runner for the same box, on another machine, replaces the first one, which is told so and disconnected. On the same machine it doesn't start at all (see above).

When a runner starts, it kills the runtimes its predecessor left behind (one killed with SIGKILL, or that lost power) and records their ends. It records nothing for one it can't show gone, and stops with an error that names the thread and process: one that still runs after the kill, one whose process it can't identify, a process of yours that started since, hides its environment and is tied to the runtime (in its process group, or below the runtime or a process marked for its thread: a non-dumpable process there could be one of the runtime's), or a process table it can't read. A hidden process that isn't tied to the runtime, such as an `ssh-agent` or a `sudo` you started in the same terminal, doesn't count. The hub then sees the box offline instead of starting another runtime next to the survivor. Stop the process, then start the runner again.

`twr info` lists each box, whether it's online and the twr version its runner last reported. While a box is offline, it also shows the latest connection naming the box that the hub refused, and why: a runner of another protocol version, a runner that did not accept the hub's proof (its token differs from the hub's), or a bad proof. Anyone who can reach the listener can name a box, so this is a hint, not proof of who connected.

A runner the hub refuses, or whose hub can't prove it knows the token, waits at least a minute before trying again, since one side's twr or token has to change first.

## Attach on a remote box

`twr attach <thread>` on a remote thread pauses the runtime through its runner and runs the agent's CLI over `ssh -t <ssh> -- <command>`, in the thread's directory on the box. That ssh forwards nothing either, whatever your `~/.ssh/config` says: no agent, X11, Kerberos credentials, ports or tunnel (`-o ClearAllForwardings=yes -o ForwardAgent=no -o ForwardX11=no -o PermitLocalCommand=no -o Tunnel=no` before the `ssh` words, `-a -x -k` after them). The CLI you attach to is an agent's, and must not reach your ssh agent any more than the box's runtimes can: a `git push` from the attached session uses the box's own keys. It needs `[boxes.<name>] ssh`, and a POSIX login shell for that user on the box (`sh`, `bash`, `zsh`): the hub quotes the command for one, and a `fish` or `csh` login shell would read it differently. The `ssh` value may start with extra options, as in `-p 2222 me@host`. The hub builds that command with the thread's mode, model and effort as they are. It never trims them to what the hub's own `claude` supports, since the box may run another version. A `claude` on the box that lacks an option (an old one without `--effort`) refuses the command line in your terminal: run `claude update` there.

On a container box `twr attach` is refused for now ([Container boxes](#what-needs-a-terminal)).

On a Mac, `twr attach` of a Claude Code thread says, before it pauses the runtime, that Claude then runs over ssh, where it may not read its keychain login: if it asks you to log in, end the attach (exit it) and continue the thread from the TUI. The box daemon's Claude Code keeps its login. twr knows a box is a Mac from the hub's probe of an SSH box; for a token box it doesn't know, and says nothing.

On an SSH box the command runs the box's own twr, the build its daemon runs: `sh -c` finds it in the box's builds folder and runs `twr runner --exec <runtime> -- <arguments>`. It looks for the agent's CLI the way a new thread's runtime would: `TWR_CLAUDE_BIN` (or `TWR_CODEX_BIN`, `TWR_GROK_BIN`) first, then the thread's `PATH` when its `env` sets one, else your login shell's `PATH` before the ssh session's. The arguments (session id, mode, model, effort) are still the hub's, and the runner's answer only offers the working directory. On a token box the hub's command runs as it is, so the CLI has to be on the `PATH` an ssh command gets there. Runtimes without a CLI of their own (an `acp` preset's template) run as the hub builds them on either kind of box.

## Protocol

The runner protocol lives in `twr_core::runner`. The hub refuses a runner that speaks another protocol version, so the box stays offline until twr on it is updated to the hub's version (the refusal, logged on both sides, says which side is older). Protocol 6 brought the challenge-response handshake: a runner from before it is refused, and a runner of 6 or later facing an older hub is refused as if its token were wrong. Protocol 7 added the agents op (`Op::Agents`), which the hub sends every box: an SSH box of the hub's OS and CPU switches to the new build by itself, a box of another OS or CPU needs `twr box install <box>` (it shows `needs_install` until then), and a token runner must be updated by hand, `twr update` on the box, then restart it. Update both sides together.

Before a connection has authenticated, the hub takes small messages only, gives it 15 seconds from the accept for the TLS and WebSocket handshakes and the proofs, and keeps at most 16 such connections at once, 4 per address that isn't loopback. After that the runner sends its inventory (running sessions, spooled frames), which may be large. A connection that sends no protocol message for 90 seconds is closed. WebSocket pings don't count; the runner's own pings, every 20 seconds, do. [SKILL.md](../SKILL.md#runners-and-remote-boxes) describes what a hub client sees: online and offline events, delivery and acknowledgement, reconciliation, and attach.
