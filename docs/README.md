# twr documentation

The [README](../README.md) has the short version: what twr is, install, the main commands and keys. These pages have the rest.

| Page | What's in it |
|---|---|
| [install.md](install.md) | the install script and its options, release builds and platforms, building from source, static builds, where twr keeps its files |
| [usage.md](usage.md) | a session from start to end: threads, `say` and `send`, answering cards, reports, attach, park, standing context, the event log |
| [tui.md](tui.md) | the stack, grid and decision views, keys, slash commands, notifications |
| [notifications.md](notifications.md) | the hub's notification command: what it's told and when, ntfy, desktop and Slack recipes |
| [configuration.md](configuration.md) | every key in `config.toml` with its default, and the environment variables |
| [runtimes.md](runtimes.md) | Claude Code, Codex, Grok Build, ACP agents and presets, PTY, permission modes, what's been checked against the real CLIs |
| [accounts.md](accounts.md) | several logins per runtime (two Claude subscriptions, a Codex account per client): adding them, pins, moves, labels, the providers' terms |
| [orchestrator.md](orchestrator.md) | what the orchestrator may do, the relay rule, reports, switching runtimes and the handoff |
| [worktrees.md](worktrees.md) | per-thread worktrees, working in your checkout, what parking removes and keeps |
| [workspaces.md](workspaces.md) | separate twrs on one machine (`twr ws`, `-w`): each with its own hub, orchestrator and threads, as tabs in the TUI |
| [runners.md](runners.md) | threads on other machines: tokens, tunnels, TLS, outages, attach over ssh |
| [architecture.md](architecture.md) | the event log, control stream, reducers, decision ladder, adapters, crates |
| [security-model.md](security-model.md) | what twr guarantees and how: agents on the socket, the orchestrator's limits, jobs, twr's git, the trash, runners, releases; and what it doesn't protect against |
| [recipes.md](recipes.md) | setups built from twr's pieces: a jobserver shared by every thread's builds, a review loop |
| [development.md](development.md) | tests, the fake runtime, the fuzz harness, CI, releases, screenshots |

[SKILL.md](../SKILL.md) at the repository root documents the hub's socket API, events and MCP tools, for agents and scripts that drive twr.

Also at the root: [CONTRIBUTING.md](../CONTRIBUTING.md) (building, testing and sending a change), [SECURITY.md](../SECURITY.md) (reporting a vulnerability), [CHANGELOG.md](../CHANGELOG.md) and the [code of conduct](../CODE_OF_CONDUCT.md).
