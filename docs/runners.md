# Runners: threads on other machines

A box other than `local` is another machine that runs threads for the hub. Adapters, git worktrees and snapshots all happen on the box. A box the hub can reach over SSH needs one command and nothing else on your side: no token, listener, tunnel or service. A box the hub can't reach over SSH runs `twr runner`, which connects out to the hub ([below](#boxes-the-hub-cant-reach-over-ssh)).

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
- **Reconnect.** When the connection drops (the network, laptop sleep, a killed ssh), the hub tries again 1 second later, then at most 5 seconds apart during the first minute and 30 seconds after that. The daemon and its agents run on meanwhile, and the next connection picks up where the last one left off, as a token runner's does ([When the connection drops](#when-the-connection-drops)). After a hub restart, the new hub connects to the same daemon.
- **A box reboot** ends the daemon and its agents. The hub's next connection starts a new daemon. Threads whose agents were running get an error card, as a token runner's do after a restart.

ssh runs with `BatchMode=yes`, so it never prompts, and with every kind of forwarding and local commands turned off, whatever your `~/.ssh/config` says. It reads your ssh config and keys otherwise. `TWR_SSH` names another ssh program. The options in `[boxes.<name>] ssh` can't turn forwarding back on or change how ssh runs: see [configuration.md](configuration.md#notes-on-some-keys) for what they may hold.

### What you need

- **ssh to the box without a prompt, from the hub's environment.** The hub never answers a prompt. Load the key into an ssh-agent the hub can use (a hub that `twr` starts inherits your shell's `SSH_AUTH_SOCK`; a hub started by a service has no agent unless you give it one), or use a key without a passphrase. Accept the box's host key once, which `twr box add` lets you do. A refused key shows in `twr box status <name>` as the last error.
- **A POSIX login shell** for your user on the box (`sh`, `bash`, `zsh`): the hub quotes its commands for one.
- **The agent CLIs on the box, logged in.** The daemon starts them with the environment of a non-interactive ssh command on the box, with your login shell's `PATH` put first ([Agents on a box](#agents-on-a-box)). `twr box doctor <box>` shows what the box finds, and `twr box login <box> <agent>` logs an agent in from your terminal. Set `TWR_CLAUDE_BIN`, `TWR_CODEX_BIN` or `TWR_GROK_BIN` where that shell sets its environment to point at another binary.
- **The repositories on the box.** Give a thread an absolute path on the box (`twr thread new /srv/code/api --box gpu`), or name a project whose `repo` is a path on the box. `boxes.toml` may hold the box's projects too: twr keeps every table of a box there when it rewrites the file, but not comments.

Threads on an SSH box get the branch names of threads on the hub's own box (`twr/<name>`). Two hubs on different machines that use one repository on one box can collide on them.

### Managing boxes

| Command | What it does |
| --- | --- |
| `twr box list [--json]` | the hub's boxes: name, transport (`ssh` or `token`), state, the twr version its runner reported, OS/CPU, and its agent CLIs as the box last reported them (`claude ✓ codex ✗ grok ?`: logged in, logged out, not known; `-` not installed) |
| `twr box status <name> [--json]` | everything the hub knows of one: state, version, the build it launched, a pending switch, OS/CPU, the last error, the latest notes (newest last), a connection the hub refused, and each agent CLI's path, version and login |
| `twr box install <name> [--binary <path>] [--yes]` | install twr on an SSH box and have the hub connect again ([below](#another-os-or-cpu)) |
| `twr box remove <name>` | take the box out of `boxes.toml` |
| `twr box doctor <name> [--json]` | check the box and say how to fix what is wrong, with or without a hub ([Check a box](#check-a-box)) |
| `twr box login <name> <agent>` | start an agent CLI's own login on the box, in your terminal ([Log an agent in on a box](#log-an-agent-in-on-a-box)) |

States: `connecting` (probing, installing, opening the connection), `online`, `offline` (between attempts), `needs_install` (run `twr box install <name>`), `switching` (see below) and `error` (the hub's box key can't be loaded).

`twr box remove` changes only `boxes.toml`. A box you configured in `config.toml` stays there: remove it yourself. The running hub keeps the box connected until it restarts (`twr hub stop`, then `twr`). The box daemon, its agents and the box's worktrees stay on the box; `twr box remove` prints the command that stops the daemon: the hub's own ssh, with its options and forwarding off, running `<build> runner --stdio --box=<name> --hub-key <key> --stop` on the box. Run it once the hub no longer connects to the box, or the hub starts a daemon again.

### Another OS or CPU

The hub can only install its own build on a box of its OS and CPU. For another one, `twr box add` says so and the box shows `needs_install`. `twr box install <name>` then:

- with `--binary <path>`, uploads that file, a twr you built for the box. It checks the size and hash on the box, runs its `--version` there and shows it. It warns when the version isn't the hub's: the two connect only if they speak the same runner protocol;
- without `--binary`, when the hub runs a release, downloads that release's archive for the box (`twr-<target>.tar.gz`, for `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `aarch64-apple-darwin` or `x86_64-apple-darwin`) and its signed `sha256sums.txt`, checks them as `twr update` does, uploads the archive, and unpacks and checks `twr` on the box. The box needs `sha256sum` or `shasum`;
- without `--binary`, when the hub runs a development build, refuses and says how to build the same commit for the box: `TWR_BUILD=<commit> cargo zigbuild --release --locked -p twr-cli --target <target>` for Linux, or `docs/tools/dev-install.sh` on the Mac, from a checkout of that commit.

It asks before it uploads (`--yes` skips the question), records the build as the box's `build` in `boxes.toml`, and has the hub connect again. When the box is online on another build, the hub switches it to the new one the same way it switches a box after a hub upgrade ([A new twr build](#a-new-twr-build)): once the box has been quiet for 2 minutes. `twr box install` waits for that, up to 6 minutes, saying `switch pending` meanwhile, and succeeds only once the box runs the build it installed. For a box in `config.toml` it prints the `build = "<id>"` line to add there; restart the hub after adding it. On a box of the hub's own OS and CPU, `twr box install` only has the hub try again now.

### Keep it running

The daemon runs in a session of its own, so it outlives the ssh connection. On Linux, when you linger on the box (`loginctl enable-linger`), the relay starts it as a transient systemd user service instead (`twr-box-<hash>-<random>`, a new name each start, so a daemon still stopping never collides with the next), outside the ssh session, and passes the relay's environment on by name. When `systemd-run` fails, the relay first checks whether the unit started anyway (no second daemon), then starts the daemon in a new session. When logind ends your processes with your last session (`KillUserProcesses=yes`) and the daemon runs in the ssh session (you don't linger, or `systemd-run` failed), the daemon and its agents end when the ssh connection does; the relay warns about it, and the warning shows among the box's notes. Run `loginctl enable-linger` on the box.

`TWR_BOX_LAUNCHER=setsid` in the box's environment makes the relay always start the daemon in a new session, without logind or systemd-run. It is for tests and troubleshooting.

### A new twr build

When the hub runs a new build, the box's daemon still runs the old one. If both speak the same runner protocol, the box keeps working on the old build and `twr box status` shows `switch pending`. Once the box has been quiet for 2 minutes (no turn running, no prompt queued, nothing waiting on you, no new thread being made, no attach, park, resume, deletion or trash restore), the hub sets the box `switching`: it stops the box's agents, which keeps their sessions and their queued prompts, replaces the daemon with the new build, and the next prompt to each thread resumes its session. Prompts sent meanwhile wait. New threads, parks, resumes, deletions and trash restores on the box are refused for that minute, with a message to try again. A hub restart during the switch leaves the stopped threads stopped, without a crash card; their queued prompts run. When the box still runs its old build after a switch (its daemon could not be replaced), `twr box status` says so as its last error, and the hub doesn't try that switch again until the box's build or the one to switch to changes, or the hub restarts.

When the new build speaks another protocol, the hub refuses the old daemon and replaces it at once. Threads whose agents were running get a crash card.

## Boxes the hub can't reach over SSH

A box the hub can't reach over SSH (behind NAT, or one you'd rather not give the hub an ssh key to) runs `twr runner`, which connects out to the hub over WebSocket and runs that box's threads. The hub needs no inbound access to the box.

### Set it up

On the hub machine, turn on the listener and give the box a token:

```toml
[hub]
listen = "127.0.0.1:7878"

[boxes.gpu]
token = "..."                  # from `twr runner token`; hub and runner prove they both have it
ssh = "me@gpu-box"             # used by `twr attach` for threads on that box

[boxes.gpu.projects.api]
repo = "/srv/code/api"         # where project api lives on the box
```

`twr runner token` prints a new token: 32 random bytes, in hex. Put the same token in the hub's config and in a file on the box. A token shorter than 16 characters keeps the hub from starting (other commands, `twr hub stop` included, still work), and one shorter than 32 gets a warning in its log and in `twr info`; the runner refuses and warns the same way. If an upgrade finds a short token, replace it in both places with one from `twr runner token`, then restart the hub (`twr hub stop`, then `twr`) and the runner. The config file now holds a secret, so make it private with `chmod 600 ~/.config/twr/config.toml`. The hub warns at start when a config holding a box token or a `[notify]` command can be accessed by other users (any group or other permission bit).

Restart the hub, then connect the box. The simplest transport keeps the listener on loopback and tunnels it over SSH:

```sh
# on the hub machine
ssh -o ExitOnForwardFailure=yes -R 7878:127.0.0.1:7878 me@gpu-box

# on the box: the token from a file only you can access, or from TWR_RUNNER_TOKEN
twr runner --hub ws://127.0.0.1:7878 --box=gpu --token-file ~/.config/twr/gpu.token

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
ExecStart=%h/.local/bin/twr runner --hub ws://127.0.0.1:7878 --box=gpu --token-file %h/.config/twr/gpu.token
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
- **daemon** (SSH boxes). Whether this hub's box daemon runs (`twr runner --stdio --status`), with its build and runner protocol.
- **logind** (Linux). Whether you linger, whether logind ends your processes with your last session, and whether the running daemon is in its systemd user service (an active `twr-box-<hash>-*` unit). Lingering alone is not enough: when `systemd-run` failed, the daemon runs in a session, and if logind ends your processes it ends with the hub's ssh.
- **login shell** and each agent. `twr runner --agents` on the box says whether the login shell gave its `PATH`, and each CLI's path, version and login. The fix says how to install it or log it in.
- **git** and **disk.** `git --version` on the box, and the free space where it keeps its data, against `[threads] disk_warn_gb`.
- **project.** Each project configured for the box (`[boxes.<box>.projects.*]`, and `[projects.*]` with `box = "<box>"`) has its repository there, and it is a git repository.

What ssh and the box print is bounded and shows with its control characters escaped, as for `twr box add`. Each command doctor runs on the box (`loginctl`, `busctl`, `git`, `df`) gets at most 10 seconds (with the box's `timeout`, else killed in the background), so one that hangs ends with doctor. What the box reports of its agents is filtered as the probe filters it (see `hub.info` `agents`), whatever twr build reported it.

### Log an agent in on a box

`twr box login <box> <agent>` (`claude`, `codex` or `grok`) starts the agent's own login on the box, in your terminal: `claude auth login`, `codex login --device-auth` or `grok login --device-auth` (a device code works over ssh, where no browser opens). twr never runs a login by itself and never sees what you type: ssh runs with a terminal (`-t`), with forwarding off as for the hub's ssh, and your terminal talks to the agent's login directly. On the box, the twr build `twr box doctor` would pick runs `twr runner --login <agent>`, which finds the CLI as the box's runner would (`TWR_*_BIN`, then the login shell's `PATH`) and runs its login in place of itself.

It needs a terminal on stdin and stdout, and only you can run it: an agent's `twr box login` is refused. When the login ends, it asks whether the agent is logged in: the box daemon, through the hub (`box.agents` with `refresh`), when a hub runs and has the box, else the box over ssh. It says when that disagrees with how the login ended, and exits 0 once the agent is logged in, or, for an agent without a status command (Grok), once a login that ended without an error leaves the CLI found. It exits 1 when the daemon doesn't find the CLI. That happens when you installed it, or changed your startup files, after the daemon started: the daemon reads the login shell's `PATH` only when it starts. The message then gives the command that stops the daemon; the hub starts a new one when it connects again, which reads the `PATH` afresh.

On a Mac, Claude Code keeps its login in the login keychain, which a login over ssh reaches only while it is unlocked. If the login doesn't stick, log in at the Mac once.

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

On an SSH box the command runs the box's own twr, the build its daemon runs: `sh -c` finds it in the box's builds folder and runs `twr runner --exec <runtime> -- <arguments>`. It looks for the agent's CLI the way a new thread's runtime would: `TWR_CLAUDE_BIN` (or `TWR_CODEX_BIN`, `TWR_GROK_BIN`) first, then the thread's `PATH` when its `env` sets one, else your login shell's `PATH` before the ssh session's. The arguments (session id, mode, model, effort) are still the hub's, and the runner's answer only offers the working directory. On a token box the hub's command runs as it is, so the CLI has to be on the `PATH` an ssh command gets there. Runtimes without a CLI of their own (an `acp` preset's template) run as the hub builds them on either kind of box.

## Protocol

The runner protocol lives in `twr_core::runner`. The hub refuses a runner that speaks another protocol version, so the box stays offline until twr on it is updated to the hub's version (the refusal, logged on both sides, says which side is older). Protocol 6 brought the challenge-response handshake: a runner from before it is refused, and a runner of 6 or later facing an older hub is refused as if its token were wrong. Protocol 7 added the agents op (`Op::Agents`), which the hub sends every box: an SSH box of the hub's OS and CPU switches to the new build by itself, a box of another OS or CPU needs `twr box install <box>` (it shows `needs_install` until then), and a token runner must be updated by hand, `twr update` on the box, then restart it. Update both sides together.

Before a connection has authenticated, the hub takes small messages only, gives it 15 seconds from the accept for the TLS and WebSocket handshakes and the proofs, and keeps at most 16 such connections at once, 4 per address that isn't loopback. After that the runner sends its inventory (running sessions, spooled frames), which may be large. A connection that sends no protocol message for 90 seconds is closed. WebSocket pings don't count; the runner's own pings, every 20 seconds, do. [SKILL.md](../SKILL.md#runners-and-remote-boxes) describes what a hub client sees: online and offline events, delivery and acknowledgement, reconciliation, and attach.
