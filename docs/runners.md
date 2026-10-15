# Runners: threads on other machines

A box other than `local` is a machine whose runner, `twr runner`, connects out to the hub over WebSocket and runs that box's threads. Adapters, git worktrees and snapshots all happen on the box. The hub needs no inbound access to the box.

## Set it up

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
twr runner --hub ws://127.0.0.1:7878 --box gpu --token-file ~/.config/twr/gpu.token

# anywhere
twr thread new /srv/code/model --box gpu
twr thread new api --box gpu
```

`-o ExitOnForwardFailure=yes` makes `ssh` give up when it cannot listen on the box's port. Without it, `ssh` only warns, and whatever already listens on that port gets the runner's connection: another user's process, say. If that process can't reach the hub's listener, it learns nothing. Hub and runner never send the token. Each proves it has it by signing the other's random challenge (HMAC-SHA256), and the runner checks the hub's proof before it sends anything else. A runner that gets no valid proof logs that the hub could not prove it knows the token and tries again later. But your box stays offline until the tunnel is up. If the process can reach the hub's listener, for example one that serves TLS to the network for other boxes, it can pass the handshake back and forth between runner and hub. The proofs don't name the connection they travel on, so both sides accept, and the process can then send the runner commands. So use `ExitOnForwardFailure`, and keep the listener reachable from as few machines as you can.

The runner refuses a token file other users can access (any group or other permission bit) or that is not a regular file; `chmod 600` it. It never takes the token on the command line, which every local user can see. The runner keeps its spool and worktrees in its data directory, or in `--data-dir`.

One runner per box runs on a machine. A runner locks its spool, its worktrees directory and a lock file for its hub and box in `$XDG_STATE_HOME/twr/runner-locks/` (by default `~/.local/state/twr/runner-locks/`), which stays put whether the runner was started from a login session, a systemd service or cron. A second runner of the same box for the same hub on that machine refuses to start, whatever its `--data-dir`. Two of them would kill each other's runtimes as leftovers and run repository changes over each other. The lock goes by the `--hub` URL as written, so give every runner of a box the same one.

A project whose threads should always run on the box can say so with `[projects.<name>] box = "gpu"`. Without a `[boxes.<box>.projects.<name>]` entry, the project's `repo` is taken as a path on the box.

## Transport and TLS

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

## When the connection drops

The runner spools events to disk while it's disconnected and replays them in order when it reconnects. The hub records each event exactly once, at the time the runner captured it, so a replayed backlog keeps its original times.

The runner retries at most 5 seconds apart during the first minute of an outage, then backs off to 30 seconds. While a box is offline its threads keep their state and their runtimes keep running on the box. Prompts queue until it's back. `twr stop` is recorded and goes out when the box reconnects. Creating, parking, resuming or attaching a thread on an offline box, and answering its cards, fail with `box_offline`. A change to a repository on a box (creating, parking, deleting or restoring a thread's worktree) waits up to 10 minutes for the box's answer; one left unanswered (the box went offline or did not answer in time) has an unknown outcome, and no other git command runs on that box until the runner answers it or reconnects. A runner reconnects only once the repository changes it was running have finished.

After a reconnect the hub and the runner reconcile. A thread the log shows running whose runtime the runner no longer has is closed with an error card. A session the runner runs but the hub doesn't know is stopped. After a hub restart the hub takes over the sessions the runner still runs. A hub shutdown stops local runtimes only.

A second runner for the same box, on another machine, replaces the first one, which is told so and disconnected. On the same machine it doesn't start at all (see above).

When a runner starts, it kills the runtimes its predecessor left behind (one killed with SIGKILL, or that lost power) and records their ends. It records nothing for one it can't show gone, and stops with an error that names the thread and process: one that still runs after the kill, one whose process it can't identify, a process of yours that started since, hides its environment and is tied to the runtime (in its process group, or below the runtime or a process marked for its thread: a non-dumpable process there could be one of the runtime's), or a process table it can't read. A hidden process that isn't tied to the runtime, such as an `ssh-agent` or a `sudo` you started in the same terminal, doesn't count. The hub then sees the box offline instead of starting another runtime next to the survivor. Stop the process, then start the runner again.

`twr info` lists each box and whether it's online.

## Attach on a remote box

`twr attach <thread>` on a remote thread pauses the runtime through its runner and runs the agent's CLI over `ssh -t <ssh> -- <command>`, in the thread's directory on the box. It needs `[boxes.<name>] ssh`, and a POSIX login shell for that user on the box (`sh`, `bash`, `zsh`): the hub quotes the command for one, and a `fish` or `csh` login shell would read it differently. The `ssh` value may start with extra options, as in `-p 2222 me@host`. The hub builds that command with the thread's mode, model and effort as they are. It never trims them to what the hub's own `claude` supports, since the box may run another version. A `claude` on the box that lacks an option (an old one without `--effort`) refuses the command line in your terminal: run `claude update` there.

## Protocol

The runner protocol lives in `twr_core::runner`. The hub refuses a runner that speaks another protocol version, so the box stays offline until twr on it is updated to the hub's version (the refusal, logged on both sides, says which side is older). Protocol 6 brought the challenge-response handshake: a runner from before it is refused, and a runner of 6 or later facing an older hub is refused as if its token were wrong. Update both sides together.

Before a connection has authenticated, the hub takes small messages only, gives it 15 seconds from the accept for the TLS and WebSocket handshakes and the proofs, and keeps at most 16 such connections at once, 4 per address that isn't loopback. After that the runner sends its inventory (running sessions, spooled frames), which may be large. A connection that sends no protocol message for 90 seconds is closed. WebSocket pings don't count; the runner's own pings, every 20 seconds, do. [SKILL.md](../SKILL.md#runners-and-remote-boxes) describes what a hub client sees: online and offline events, delivery and acknowledgement, reconciliation, and attach.
