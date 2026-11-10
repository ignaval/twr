# Desktop QA and feature parity

QA date: 2026-11-06. Platforms: Apple Silicon macOS and x86_64 Ubuntu/Arch
Linux userspaces in QEMU. Mac tests launch the actual packaged
Electron application through LaunchServices, with production socket authentication and a
private temporary `TWR_HOME`. Agents and account login use deterministic local fixtures.
The tests do not use real provider accounts or the user's existing workspaces.

## Coverage and intentional differences

All 74 runnable entries in the current CLI help tree have their arguments/options checked
against the generated form catalog. The desktop exposes 71 of those entries plus native
application actions. Counting entries alone is not proof of behavioral parity: regression
tests also exercise argument preservation, target selection and actual hub operations.

The catalog exclusions are intentional: agent-only `mcp`, the foreground `hub`
(the application starts an independent hub), `update` (the desktop and its bundled
engine update together through the application installer), and the terminal-only
`completions` (a script for a shell's startup files). Workspace selection replaces
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

Packaged builds ignore `TWR_DESKTOP_TEST_AUTOMATION`, which the suites need for remote
debugging, unless their resources folder holds the QA marker `twr-qa-automation`
([desktop.md](desktop.md#architecture-and-extension-points)). `npm run test:qa:mac` first
checks that the release app ignores the variable and keeps its debugging port shut, then runs
a copy of the app with the marker added; the release app is never changed. To run
`npm run test:integration` against an installed Linux package (`TWR_DESKTOP_EXECUTABLE`), add
the marker as root, as CI does: `sudo touch /opt/twr/resources/twr-qa-automation`. Remove it
afterwards.

| Area | Automated check |
| --- | --- |
| Packaged startup | Production user authentication, bundled engine startup, native PTY, hub survives app quit |
| Native interface | All manager pages, New menu, thread selection, orchestrator and direct-worker destinations |
| Appearance | Dark first launch, persisted Light/Dark/System, native window/menu colors, Omarchy light/dark palette and contrast |
| Responsive UI | Narrow panes, long-message disclosure, errors above form fields, sticky composer, 44 px mobile controls |
| Slash commands | Model/effort/runtime, account-add routing, multi-delete targets, boolean flags, unknown workspace safety |
| Messages | Streaming, question/permission responses, revision checks, queued message take-back |
| Jobs | Launch, live output and cancellation |
| Memory/context | Creation/editing, unsaved note retention across navigation, revision-aware saves |
| Lifecycle | Rename, mode change, park/resume, hub-confirmed deletion, trash restoration |
| Workspaces | Create, switch from the sidebar's workspace tree, preserved drafts, commands in an empty workspace, duplicate-submit protection |
| Speech | A selected message reaches a real speaker process using a silent custom command engine |
| CLI forms | Invalid-argument preflight keeps input, leading-dash values, subprocess arguments, remote workspace defaults |
| Terminals | Native keyboard input, attach, login fixture, reload restoration, completed sessions do not consume active limit |

## Verification limits

Real Anthropic/OpenAI/Grok authentication and paid turns, live SSH/container/dial-in hosts,
and audible platform speech have not been tested end to end in this pass. Their existing
CLI implementations are bundled and reachable; the above fixtures validate the desktop
bridge and UI rather than those external services. An installed Ubuntu 22.04 Debian package passed all 71 CLI form workflows with the
default Chromium sandbox enabled. Its native terminal module was built against
Ubuntu 22.04 headers and verified with baud-rate changes, input, resize and exit.
The portable bundle passes an ELF symbol-version gate; a clean Ubuntu base with
only declared runtime dependencies has no unresolved libraries. This Ubuntu
userspace runs on the Arch VM kernel, so it does not certify an Ubuntu kernel.
An installed x86_64 Arch package also passed the integration suite under Cage/Wayland in that VM. Both the installed Arch package and AppImage pass the full suite on native Hyprland 0.57.1 with 125% scaling, a stable `tools.twr.desktop` class and `xwayland: false`. Physical Omarchy GPU and IME behavior remain unverified. CI also includes Arch installation and Wayland tests. Local Mac installers use ad hoc signatures for testing. Developer ID signing and notarization remain release steps that require the owner’s Apple credentials.

The frontend suite has 70 checks, including engine cleanup with real live processes,
conservative handling of inaccessible engine identities, normal Linux session processes
and the portable ELF ABI gate.

Performance measurements and reproduction commands are in [performance.md](performance.md).

## Visual QA

The shared interface uses native sans-serif text, monospace for code and metadata,
neutral thread labels and a restrained cyan accent. Dark is the first-launch default.
Appearance is a local preference, including on devices without hub admin access.

To capture the actual packaged Mac application and its disposable fixture:

```sh
cd clients
TWR_UI_CAPTURES=1 npm run test:qa:mac
```

Captures and measured typography/layout details are written to `test-results/ui`.
The set includes Dark and Light conversations, grid, inspector, permission cards,
palette, new-thread validation, settings, long messages, tool disclosures and narrow
windows. Android's native integration harness accepts the same `TWR_UI_CAPTURES=1`
flag and captures the drawer, management navigation, local appearance controls,
thread menu and keyboard/toast layout. iOS records application snapshots as XCTest
attachments, with friendly PNG copies when the local export succeeds.

`node scripts/native-appearance-qa.mjs` checks native Mac appearance persistence and
validated IPC. `node scripts/omarchy-qa.mjs`, run inside a Hyprland session, checks
actual theme-file changes, text contrast and native Linux window colors. These are
focused local checks in addition to the packaged functional suites.
