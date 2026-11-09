# Pair machines as runners

Starting a local hub with the CLI, TUI or desktop app starts or reuses one independent runner service for that twr home. A new service stays private and does not publish a runner endpoint until you open pairing or it has saved incoming trust. Closing a hub or client leaves worker daemons and their agents running.

Linux and macOS on x86-64 and ARM64 are supported. A mobile client can connect and manage runners through its hub; the phone does not host a runner.

## Pairing

On the machine that will run agents, log in to the agent CLIs you want to use, then choose **Use this machine as a runner** in the desktop app's Boxes panel, type `/runner` in the TUI, or run:

```sh
twr runner up
```

On the other hub machine choose **Connect a runner**, or run:

```sh
twr box pair desk
```

Enter the code at the hidden prompt. It is never a command-line argument. Automation can explicitly use `twr box pair desk --code-stdin`. The installer accepts `sh install.sh --pair` to install and open pairing; it needs a release newer than v0.3.0 (one with `twr runner up`), and with an older one it installs twr and then says so instead of pairing. The code contains 60 random bits, works once, expires in 15 minutes and closes after three admitted failed proofs. Closing a client pairing panel cancels its own code. An interrupted final acknowledgement or box registration can be recovered with `twr box recover desk`, the client’s **Recover a saved pairing** action, or the user-only `runner.recover` RPC; no new code is needed. A second name reuses valid machine trust. Replacing an active grant after losing its saved receipt requires the explicit `--replace` flag or client checkbox and stops that machine’s old workers.

Paired hubs have owner access to the runner's OS account, agent credentials and runner access controls. They are trusted peers, not isolated tenants. Use separate OS accounts or containers for hubs you do not trust with that access. Machine public keys identify owners. Workspace identity, generation, workspace key and connection endpoint are bound by both the machine and workspace signatures.

## Threads and committed results

Configure the project on the hub as usual, then select the linked box:

```sh
twr thread new api --box desk
twr thread pull <thread>
```

The hub sends an exact commit through a named-ref Git bundle, and the worker creates a managed worktree. It does not send origin credentials or fetch from the origin. Submodules are not initialized and LFS objects are not downloaded. A result pull imports committed changes into a separate `refs/twr/results/…` reference; it does not change your checkout or index. Uncommitted runner edits remain on the runner. Worker executables are limited to 1 GiB; Git and source bundles are limited to 128 MiB, with a separate unpacked-object growth limit.

The default worker is the hub's exact executable. If it cannot run on the runner, the hub sends the source snapshot embedded when that executable was compiled, including nonignored local changes and untracked files. Source capture leaves the checkout and index intact, preserves raw bytes, respects Git ignore files and rejects hidden index flags (a sparse checkout, skip-worktree or assume-unchanged entries). A checkout it refuses, or whose snapshot exceeds the 128 MiB bundle limit, still builds: Cargo prints a warning and the build records no source, so a runner of another OS or CPU reports that this hub build has no recorded source. The runner needs rustup and the native build dependencies. It installs the recorded stable compiler through rustup when needed, and builds the recorded source, profile and Rust flags with `cargo --locked`. There is one low-priority native build at a time, with one Cargo job. Build-time source capture also works from a source archive and writes no objects or refs into the source repository. Source build scripts have the same OS-user authority as an agent.

A hub executable is retained in an immutable local slot so replacing an installation while a hub runs cannot silently change its worker source. Worker changes use the hub's existing quiet/drain/restart flow; running turns finish before the switch. Worker code and transport-service code have separate update lifecycles. Uploaded worker code never selects the shared service executable.

## Access and maintenance

```sh
twr runner status                 # local service, paired owners, launch mode and capacity
twr box access desk               # owners trusted by this runner
twr box invite desk               # a private code for another hub
twr box revoke desk <machine-key> # revoke from any trusted hub
twr runner revoke <machine-key>  # revoke directly on the runner
twr box remove desk               # forget locally and request remote revocation
twr runner cancel-pairing
twr runner enable --disable       # disable automatic startup
twr runner enable                 # restore automatic startup
twr runner stop                   # stop transport; independent workers remain
twr runner capacity 4             # global resident-runtime limit, 1–64
twr runner allow-sleep            # permit macOS sleep during work
twr runner keep-awake             # default: prevent idle sleep during work
twr runner prune                  # unused build caches only
```

Revocation writes a permanent per-grant fence before acknowledging it, closes that owner's connections, stops its workers and rejects late build/transfer publication. A startup reconciliation prevents an interrupted registry save from restoring retired authority. Worktrees, spools, repositories and committed results are retained. Re-pairing the same machine creates a new grant while retaining workspace data. Forgetting an offline runner first removes local connection authority, then durably queues an idempotent receipt-bound revocation request; status reports pending acknowledgements until confirmed. A delayed old request cannot revoke a replacement grant.

Runtime admission is machine-wide, round-robin across owners and FIFO within an owner. The default is at most four resident runtimes, reduced on low-memory machines; an explicit capacity setting overrides it. An idle, resumable built-in runtime can be retired when another request waits. The service rate limits retirements across all hubs and checks that capacity is full; canceled waits are removed by generation and expire after five seconds without a refresh. Active turns, background tasks, queued prompts, attention items and lifecycle/repository operations prevent retirement. The next message resumes the session. Disk admission preserves at least 5 GiB or 10% of the filesystem, whichever is larger, and transfers/builds have a 10 GiB per-owner budget. The budget counts what a paired hub sends or has built: worker executables, native build caches (recorded source, objects and materialized checkout), incoming bundles and the managed repositories' Git data. It does not count agent work (worktrees, trash, spools, logs), so a thread's `target/` or `node_modules` never stops new threads, nor the running native build's compiler output, which is removed after publication; the free-space reserve still covers both, and a running build re-checks it every 15 seconds. `twr runner prune` reclaims build caches when the budget is full. These are admission controls, not OS filesystem quotas on trusted agents.

Kernel leases remain held by independent workers across transport-service restarts. Durable lease records let the next admission find and retire a crashed daemon's orphan runtime generation before reusing its capacity. Unknown process liveness fails closed. Native compiler children retain the build lease across service crashes; startup and grant revocation retire the recorded build marker. Revocation also stops recorded runtime generations when their daemon has already died. Automatic cache maintenance retains current worker builds, live hubs, a rollback slot and a day of grace. Explicit `twr runner prune` can reclaim younger unused build slots as well. Compiler scratch output is removed after native artifact publication, while a build lease protects the copy and cleanup. Neither path deletes work or trust fences.

The service and its worker daemons listen on sockets in a directory only you can write to: `$XDG_RUNTIME_DIR` (else `/run/user/<uid>`) on Linux, the per-user temporary directory on macOS, or `/tmp/twr-runner-<uid>-<hash>` when there is none (or its path is too long for a socket). A service or worker an earlier release started under `/tmp` stays reachable there until it is replaced. A service whose socket disappears (a runtime directory removed at logout) stops within 30 seconds, leaving its workers running, so the next client can start it again.

The service uses the macOS user launchd session or Linux user systemd manager when available, with a detached fallback. Linux user-manager lifetime still depends on the login session and linger policy; a fallback cannot promise restart after logout or reboot. Status reports the launch mode. Whichever way it starts, the service and the agents it runs for every paired hub get only part of the starting process's environment, by name: `PATH`, `HOME`, `SHELL`, `USER`, `LOGNAME`, `LANG`, `LC_*`, `TMPDIR`, `TZ`, `XDG_*`, `TWR_CLAUDE_BIN`, `TWR_CODEX_BIN`, `TWR_GROK_BIN`, `TWR_BOX_PROBE` and `TWR_AGENT_PROBE` (the list a Mac box daemon's launchd job gets). An API key, `TWR_WORKSPACE` or any other variable of the shell that first started a hub never reaches them. On Linux, a `systemd-run` service also gets the user manager's own environment. Agents log in with their own CLIs' file or keychain logins. Proxy and CA bundle variables, `CARGO_HOME`, `RUSTUP_HOME` and agent config-directory overrides (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`) are not passed on either: set what threads need with the hub's `[threads] env` or `[projects.<name>] env` ([configuration.md](configuration.md)). Agents need the runner awake and their own CLIs logged in. macOS keeps an idle-sleep assertion during worker/build activity unless disabled.

The initial transport service comes from the locally executed installation. Unattended service updates use signed, newer official stable releases, the fixed release key and a compatible state schema. Startup readiness checks retain the previous executable for rollback. No development mirror or test verification key is accepted by this updater. A local development service can be refreshed explicitly with `twr runner stop` followed by `twr runner up`.

## Discovery and protocol

The service uses iroh QUIC with authenticated endpoint identities, direct UDP when available and an encrypted relay path otherwise. Endpoint discovery and pairing locators use Mainline DHT plus the explicitly configured `https://dns.iroh.link/pkarr` service. No public record is needed for an unused service. Pairing records contain a temporary endpoint/relay locator, not your code or private addresses.

The code derives a locator key and confirmation key with Argon2id (64 MiB, three passes) and HKDF. Mutual ring HMAC proofs cover the complete identity transcript and the actual QUIC exporter; a fresh machine signature proves possession of the machine key. This is a code-authenticated handshake, not a PAKE. The signed public locator is an offline code verifier. The finite, high-entropy, short-lived code is part of that security boundary.

After pairing, every new connection also proves possession of the current durable grant receipt. The workspace binding and accepted grant epoch stay fixed for that connection. Worker traffic remains sealed end to end with the workspace key. JSON controls are limited to 32 KiB; binary/source/Git transfers use independent bounded streams and never enter the worker result spool.

This change requires client protocol 2 because capacity retirement adds a restart reason that older clients cannot fold correctly. Older clients must update; runner protocol 8 and its spool/event formats are inherited from the merged client baseline.
