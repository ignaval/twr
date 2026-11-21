# Desktop QA and feature parity

QA date: 2026-11-06. Platforms: Apple Silicon macOS and x86_64 Ubuntu/Arch
Linux userspaces in QEMU. Mac tests launch the actual packaged
Electron application through LaunchServices, with production socket authentication and a
private temporary `TWR_HOME`. Agents and account login use deterministic local fixtures.
The tests do not use real provider accounts or the user's existing workspaces.

## Coverage and intentional differences

Every runnable entry in the current CLI help tree has its arguments/options checked
against the generated form catalog (`npm run catalog:check`). The desktop exposes all of
them except the intentional exclusions below, plus native application actions; the
integration suite reports how many CLI workflows it ran in its final `PASS:` line.
Counting entries alone is not proof of behavioral parity: regression
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

Every fixture works in a `TWR_HOME` of its own. A release hub starts a linked-runner
service for its home ([runner-links.md](runner-links.md)) that outlives `twr hub stop`, so
the QA scripts end with `scripts/fixture-cleanup.mjs`, on failure too: `twr hub stop`,
then `twr runner stop` with the same `TWR_HOME`, then, on macOS, `launchctl bootout` of
the `tools.twr.box.*` jobs whose files are inside that home (`twr runner stop` leaves its
job loaded). Debug hubs start no runner service for a `TWR_HOME`; packaged runs do.
`launchctl list | grep tools.twr.box` shows no new job after a run. `npm run package` needs cargo-about for the third-party notices; a local review
build can skip the Rust part with `TWR_NOTICES_WITHOUT_RUST=1` ([desktop.md](desktop.md#packages)).

| Area | Automated check |
| --- | --- |
| Packaged startup | Production user authentication, bundled engine startup, native PTY, hub survives app quit |
| Native interface | All manager pages, New menu, thread selection, orchestrator and direct-worker destinations |
| Appearance | Dark first launch, persisted Light/Dark/System, native window/menu colors, Omarchy light/dark palette and contrast; on Hyprland, every Omarchy v4.5.0 stock theme switched live by `omarchy-theme-set` and measured on the main screens, and broken themes ([below](#omarchy-themes-in-a-real-hyprland-session)) |
| Responsive UI | Narrow panes, long-message disclosure, errors above form fields, sticky composer, a message field that grows with wrapped text at 22 px in full and 390 px windows and shrinks back, 44 px mobile controls |
| Slash commands | Model/effort/runtime, account-add routing, multi-delete targets, boolean flags, unknown workspace safety |
| Messages | Streaming, question/permission responses, revision checks, queued message take-back |
| Jobs | Launch, live output and cancellation |
| Memory/context | Creation/editing, unsaved note retention across navigation, revision-aware saves |
| Lifecycle | Rename, mode change, park/resume, hub-confirmed deletion, trash restoration |
| Workspaces | Create, switch from the sidebar's workspace tree, preserved drafts, commands in an empty workspace, duplicate-submit protection |
| Speech | A selected message reaches a real speaker process using a silent custom command engine |
| CLI forms | Invalid-argument preflight keeps input, leading-dash values, subprocess arguments, remote workspace defaults |
| Terminals | Native keyboard input, attach, login fixture, reload restoration, completed sessions do not consume active limit |
| Loops | Review and implementation loops with a fake reviewer (`scripts/loops-integration.mjs`); on a dial-in runner box on the same machine: the card's warnings naming the box, a reviewer account the box does not use, the runner stopped during a review run (the wait said once, shown after a reload and a reconnect) and started again, an implementation loop that commits, no start while the box is away; an ACP preset as the reviewer (the scripted ACP agent of the hub's tests): the pickers, the card's warning, a loop to its end, the preset removed |

## Verification limits

Real Anthropic/OpenAI/Grok authentication and paid turns, live SSH/container/dial-in hosts on
other machines, and audible platform speech have not been tested end to end in this pass
(loops run through the app on a dial-in runner on the same machine, above). Their existing
CLI implementations are bundled and reachable; the above fixtures validate the desktop
bridge and UI rather than those external services. An installed Ubuntu 22.04 Debian package passed every CLI form workflow of the integration
suite with the default Chromium sandbox enabled. Its native terminal module was built against
Ubuntu 22.04 headers and verified with baud-rate changes, input, resize and exit.
The portable bundle passes an ELF symbol-version gate; a clean Ubuntu base with
only declared runtime dependencies has no unresolved libraries. This Ubuntu
userspace runs on the Arch VM kernel, so it does not certify an Ubuntu kernel.
An installed x86_64 Arch package also passed the integration suite under Cage/Wayland in that VM. Both the installed Arch package and AppImage pass the full suite on native Hyprland 0.57.1 with 125% scaling, a stable `tools.twr.desktop` class and `xwayland: false`. Physical Omarchy GPU and IME behavior remain unverified. CI also includes Arch installation and Wayland tests. Local Mac installers use ad hoc signatures for testing. Releases are signed with the maintainer's Developer ID and notarized; a locally signed, notarized 0.4.0 build passed Gatekeeper (`spctl`, `syspolicy_check distribution`, a quarantined copy from the disk image), `install.sh --desktop` and the packaged QA before the first signed release.

The frontend suite (`npm test`) includes engine cleanup with real live processes,
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
validated IPC. `node scripts/omarchy-qa.mjs`, run inside a Hyprland session after
`npm run build` and a debug `cargo build -p twr-cli -p twr-app`, checks actual theme-file
changes, the Light/Dark/System preferences and native Linux window colours. It then puts
Omarchy v4.5.0 in its disposable home as Omarchy's installer does (the repository at
`~/.local/share/omarchy`; it clones it, or uses `TWR_OMARCHY_SOURCE`), switches the running
app through every stock theme with Omarchy's own `omarchy-theme-set` in its headless mode,
then back to the first, and for each one checks that the app followed without a restart and
without passing through an unreadable or partial state, that it applied exactly the palette
`shared/omarchy.ts` derives, that the native window background followed, and measures in the
renderer the colours it draws on its main screens (`scripts/omarchy-audit.mjs`): the thread
list, the orchestrator's conversation with code, a thread's inspector with its Loops section,
a request card, a loop's setup card and its settings editor, Needs you, Settings, the New
thread dialog, an error notice and the composer, focused, for the orchestrator, a thread and
a shell command. Every text pair must read at 4.5:1 (3:1 for large text), every field edge
and focus ring at 3:1, and every thread colour's name at 4.5:1 on each surface. Last come
broken themes (foreground equal to background, missing keys, invalid colour strings, no
`colors.toml`) and no theme at all. It writes `test-results/omarchy-themes.json` (the lowest
pair of each theme and where it is), and in `test-results/omarchy-themes/` every measured
pair and a screenshot of each theme, one right after each switch and one of the whole output
with `grim` when it is installed. These are focused local checks in addition to the packaged
functional suites.
`tests/omarchy-contrast.test.ts` (part of `npm test`) runs on any machine: it resolves
`style.css` with the palette the app derives for each stock Omarchy theme (their colours
are fixtures in `tests/omarchy-themes.ts`, from Omarchy v4.5.0; `omarchy-qa.mjs` checks them
against the release) and for broken themes, and checks every text, fill, selection,
field-edge, focus-ring and thread-name pair the app draws, and that no field's own rule
draws its edge in an unchecked colour.

### Omarchy themes in a real Hyprland session

Checked on 2026-11-14 on commit 6850a84 with the fixes below, in the Arch Linux x86_64 QEMU
VM (kernel 7.3.4, software rendering on the virtio GPU, 1280×800 at 125%): Hyprland 0.57.2,
Electron 45.2.3, the app built from the commit in the VM and a debug `twr` cross-built for
x86_64 Linux. The VM has no Omarchy: Omarchy v4.5.0's repository (commit `ac2690cd`) stands
in for it in the script's disposable home, and its own `omarchy-theme-set` switches themes in
headless mode, which skips what needs the Omarchy shell (wallpaper, bar, terminals, other
apps' retints). The unchanged script also passed against the CI build of 6850a84
(`desktop.yml` run 43621526223, the Linux bundle's engines).

- Omarchy 4 keeps the selected theme in `~/.local/state/omarchy/current/theme`; the app read
  only `~/.config/omarchy/current/theme`, so it never followed a theme Omarchy 4 set. Fixed.
- All 23 stock themes, the 5 light ones included, then the first again: every text pair at
  4.51:1 or more (Retro-82's `waiting` label on the selected thread card is the lowest; most
  themes' lowest is the composer's "To:" name at 4.58:1 to 5.56:1), field edges 3.41:1 to
  3.78:1, focus rings 5.28:1 or more, thread names 4.60:1 or more. The app followed each
  switch in 0.6 to 4.7 seconds (it checks every 5), straight from the old palette to the new.
- The live measurement found three pairs under the line that the unit test did not model:
  the request card's note field (its own rule drew the edge in `--border`, about 1.4:1), the
  composer's "To:" name in shell mode (warning on its warning-tinted chip, 4.44:1 on three
  themes) and the composer's edge while focused (mixed from a thread's raw colour, down to
  2.1:1). All three are fixed and now in `tests/omarchy-contrast.test.ts`.
- The Linux menu bar stays light Adwaita on every dark theme: Electron 45 draws it from the
  GTK theme it started with, and neither `nativeTheme`, GNOME's colour scheme nor the desktop
  portal changes it, at launch or later ([desktop.md](desktop.md#workflows-and-cli-coverage)).
  The window background follows.
- Broken themes and none, each set with `omarchy-theme-set` as a theme of the user's: a
  foreground equal to the background and a theme with only `background` and `foreground`
  keep the theme and read at 4.59:1 and 4.58:1 or more; invalid colour strings, a theme
  without `colors.toml` and no theme at all fall back to the app's own colours (Light here,
  as GTK is light), which read at 4.51:1 or more once its warning colour was darkened (the
  Needs you count on the selected row was 4.01:1). The native window follows each time. The
  app logged nothing about themes; its only messages were that this VM has no unlocked
  keyring for saved hubs.

- The message field, in the same session at 22 px (the system sans-serif here is Adwaita
  Sans): in the 982×570 window Hyprland gave the app, with the keyring notice above the
  conversation, a long message grew the field to its 40% cap and pushed Send, the microphone
  and Upload 28 px below the window. Fixed: the field now stops where the composer fits (149
  px there), and at 390×844 it reaches its 240 px cap. The integration suite's check of this
  accepts either. The whole suite could not run here: under emulation the fixture hub took
  longer than the suite's 30-second wait to connect, so its message field part ran alone.

Not covered: a physical GPU, an input method and fractional scaling on real hardware, and a
real Omarchy session with its shell and retints.

### Omarchy on real hardware

Checked on 2026-11-21 on a MacBook Pro (Ivy Bridge HD 4000 and GeForce GT 650M, 2880×1800
panel at scale 2) running Omarchy 4.5.0 (package `4.5.0-3`, its shell's bar and tray, uwsm),
Hyprland 0.56.2 (Lua configuration), kernel 7.2.15, Electron 45.2.3, with the app and a debug
`twr` built from this repository on the machine. Agents were the user's own `claude`, `codex`
and `grok`, installed with mise.

- The installed Omarchy's 23 stock themes have the colours `tests/omarchy-themes.ts` holds.
  (Its `osaka-jade` folder holds only backgrounds, left by another package; it is not a theme.)
- `node scripts/omarchy-qa.mjs` passed: all 23 themes and the switch back, every text pair
  at 4.58:1 or more, field edges 3.41:1 or more, focus rings 5.28:1 or more, thread names
  4.60:1 or more, the app following each switch in 0.1 to 4.4 seconds; broken and missing
  themes fell back readably. `npm run test:integration` (both suites, `TWR_HYPRLAND_QA=1`) passed. The
  session's screen was locked for most of these runs: the renderer measurements stand, but
  the whole-output `grim` captures show the lock screen, so the menu bar colour they report
  is meaningless there.
- Hyprland tiles a new window beside those already on the workspace, so on this screen the
  app opened 699 px wide in its narrow layout, where the scripts' waits fail. The scripts now
  float the window at 1280×800 (`scripts/hyprland-window.mjs`, Lua dispatchers on Hyprland
  0.56 and later, the older dispatcher names before), and again after showing a hidden
  window, which Hyprland maps as a new one.
- Fixed: the sidebar, which a narrow window closes, stayed closed when Hyprland widened the
  window again. It now comes back as the wide layout had it.
- Fixed: a hub the app started (through the bundled `twr`) inherited dozens of
  Electron's descriptors, the crash reporter's socket among them, so the crash reporter
  outlived the app, held the app's stdout open, and Playwright's `close()` never returned.
  Both ways of starting a hub now mark every inherited descriptor above stderr
  close-on-exec (`twr_core::fork::close_inherited_on_exec`).
- Fixed: the Omarchy QA's wait for animations to end (`scripts/omarchy-audit.mjs`) had no
  limit, and an infinite animation never ends. Twice a run stopped for good on a page
  evaluation that never returned, with the renderer and the main process both answering;
  this wait is the one evaluation that can do that. It now gives up after 3 s and says so;
  no run after that stopped.
- `cargo test --workspace` and clippy pass. Two CLI tests failed here before, from the
  base commit on: the hub's first `hub.info` hashes its own executable for its build id,
  1.26 s for the 417 MB debug `twr` on this CPU (no SHA instructions), longer than the 1 s
  the tests give `twr hub stop --timeout 1` and `twr update`'s look at the running hub. The
  hub now hashes it as it starts, and those two tests wait for a first `hub.info`.
- The tray needs no Waybar: the Omarchy shell registers `org.kde.StatusNotifierWatcher`,
  which is what the app checks. uwsm runs XDG autostart entries, where Launch at login
  writes its own (not tried at a real login here).
- With Omarchy's login shell, the app's environment recovery (`bash -ilc`, a launcher's
  stripped environment) finds the agents through mise's command wrappers in under 0.1 s.
- On the CLI, a hub in a temporary `TWR_HOME` ran a Claude Code thread that wrote and
  committed a file, a Claude Code orchestrator that used its twr tools, and one-turn Codex
  and Grok Build threads; `usage`, `trim`, `park`, `resume`, `delete`, `trash restore` and the
  TUI's stack and grid views worked.

Not covered here: an input method, the packaged Arch build (not installed, to leave the
system unchanged) and a session that stayed unlocked throughout.
