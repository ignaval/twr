# Desktop QA and feature parity

QA date: 2026-11-06. Platform: this Apple Silicon Mac. Tests launch the actual packaged
Electron application through LaunchServices, with production socket authentication and a
private temporary `TWR_HOME`. Agents and account login use deterministic local fixtures.
The tests do not use real provider accounts or the user's existing workspaces.

## Coverage and intentional differences

All 74 runnable entries in the current CLI help tree have their arguments/options checked
against the generated form catalog. The desktop exposes 71 of those entries plus native
application actions. Counting entries alone is not proof of behavioral parity: regression
tests also exercise argument preservation, target selection and actual hub operations.

The three catalog exclusions are intentional: agent-only `mcp`, the foreground `hub`
(the application starts an independent hub), and `update` (the desktop and its bundled
engine update together through the application installer). Workspace selection replaces
global socket/workspace overrides. Generated forms retain interactive confirmations rather
than offering an automatic `--yes`. Advanced operations, including SSH setup and account
login, run the bundled CLI inside an embedded terminal rather than having bespoke screens.
Terminal-specific mouse capture and layouts are replaced by desktop text selection and
responsive panes.

The desktop and TUI now share the Rust slash parser and completion catalog. There is no
second TypeScript grammar for model/account flags, multi-thread deletion or workspace
commands. Relative project paths and usage windows also use shared Rust helpers. CLI
forms preflight through the exact clap grammar of the bundled executable.

## Reproducible checks

```sh
cargo test --locked -p twr-core -p twr-tui -p twr-app
cargo clippy --workspace --all-targets --locked -- -D warnings
cd clients
npm test
npm run test:integration
npm run package:mac
npm run test:qa:mac
```

| Area | Automated check |
| --- | --- |
| Packaged startup | Production user authentication, bundled engine startup, native PTY, hub survives app quit |
| Native interface | All manager pages, New menu, thread selection, orchestrator and direct-worker destinations |
| Slash commands | Model/effort/runtime, account-add routing, multi-delete targets, boolean flags, unknown workspace safety |
| Messages | Streaming, question/permission responses, revision checks, queued message take-back |
| Jobs | Launch, live output and cancellation |
| Memory/context | Creation/editing, unsaved note retention across navigation, revision-aware saves |
| Lifecycle | Rename, mode change, park/resume, hub-confirmed deletion, trash restoration |
| Workspaces | Create/switch, preserved drafts, commands in an empty workspace, duplicate-submit protection |
| Speech | A selected message reaches a real speaker process using a silent custom command engine |
| CLI forms | Invalid-argument preflight keeps input, leading-dash values, subprocess arguments, remote workspace defaults |
| Terminals | Native keyboard input, attach, login fixture, reload restoration, completed sessions do not consume active limit |

## Verification limits

Real Anthropic/OpenAI/Grok authentication and paid turns, live SSH/container/dial-in hosts,
and audible platform speech have not been tested end to end in this pass. Their existing
CLI implementations are bundled and reachable; the above fixtures validate the desktop
bridge and UI rather than those external services. An installed x86_64 Arch package passed the same integration suite under Cage/Wayland in a local QEMU VM. Both the installed Arch package and AppImage pass the full suite on native Hyprland 0.57.1 with 125% scaling, a stable `tools.twr.desktop` class and `xwayland: false`. Physical Omarchy GPU and IME behavior remain unverified. CI also includes Arch installation and Wayland tests. Local Mac installers use ad hoc signatures for testing. Developer ID signing and notarization remain release steps that require the owner’s Apple credentials.

Performance measurements and reproduction commands are in [performance.md](performance.md).
