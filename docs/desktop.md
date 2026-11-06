# Desktop application

The Electron application lives in `desktop/`. It targets Apple Silicon macOS and x86_64
Linux, including an Arch package for Omarchy. It uses the same hub, workspaces, accounts,
permissions and history as the CLI. You can use both clients at once. Closing the window
does not stop the hub; active interactive terminal sessions ask before closing.

## Development

Use the repository's pinned Rust toolchain, a current Node 24 or 26, npm, and the platform's
normal C/C++ build tools (Xcode command-line tools on macOS; `base-devel` on Arch).

```sh
cargo build --locked -p twr-cli -p twr-app
cd desktop
npm ci
npm run dev
```

`npm start` opens the last production UI build. `npm run build` type-checks TypeScript and
builds the host, isolated preload and React UI. The development host takes a matched CLI
and companion from `desktop/bin`, `target/debug`, or `target/release`. An explicit
`TWR_DESKTOP_BINARY_DIR` is accepted only in development. Packaged builds always use the
three bundled executables (CLI, companion and mobile gateway). `TWR_HOME` remains useful for an independent test installation.

The app recovers the login shell environment so that agents, git and developer tools are
found when launched from Finder or an Omarchy launcher. It preserves agent identity
markers. A desktop app launched by a twr agent cannot turn itself into a user client.

## Workflows and CLI coverage

The orchestrator conversation is primary. Selecting a thread opens its inspector; **Talk
to this thread** (or a double-click) deliberately changes the composer destination. The
composer always names its destination. Mentions reference threads without changing it.
Enter sends, Shift-Enter inserts a line, `/` offers commands, and `//` sends a literal slash.

Approval cards render requests literally, including command punctuation and paths. Every
answer names the exact displayed request revision. Requests with unavailable full payloads
cannot be answered from a preview. Orchestrator messages use `say` with the displayed
control position, matching the TUI's relay semantics.

| CLI capability | Desktop workflow |
| --- | --- |
| Conversations, send, orchestrator selection/reset | Conversation, explicit destination, orchestrator actions |
| New/list/show, mode/model/runtime/account, rename | New-thread form, sidebar/grid, inspector and typed action forms |
| Stop, park/resume, compact, trim, delete | Inspector actions; deletion uses the hub's confirmation card |
| Attention and responses | Needs you and inline request cards; revision-bound answers and notes |
| Message queues and dependencies | Queue with take-back/send-now, send-after and cancel-held-message forms |
| Attach, PTY fallback, arbitrary shell | Embedded xterm.js terminals using the matching real CLI or login shell |
| Account list/check/add/login/remove/native naming | Accounts page, typed actions and generated interactive forms |
| Boxes, SSH/container/dial-in setup, login, projects, tokens | Boxes page and generated interactive CLI forms |
| Jobs, slots, timeouts, output and cancellation | Jobs table, launch form, live log and cancellation actions |
| Usage and grouping/time windows | Usage meters and typed reporting form; CLI forms retain duration flags |
| Next-message suggestions, routine summaries | Shared hub small-model calls, first-use disclosure, Tab to accept, expandable routine updates |
| Context and memory | Revision-aware editors, note cards and standing-context editor |
| Speech, remote sessions, speaker controls | Read-aloud page and embedded CLI forms |
| Workspaces | Workspace selector, creation and removal forms |
| Trash list/restore/purge | Trash table and preview-first purge form |
| Log statistics/archive/checkpoints, info, events/control | Hub & storage, diagnostics and generated CLI forms |
| Runner provisioning and terminal setup | Advanced CLI forms; runner commands do not receive workspace flags |
| Update | App-level release check and platform installer; the CLI never overwrites the app's engine |

The command palette discovers commands, arguments and options from the **bundled binary's
own help**, including clap's multiline help. Arguments are preflighted with the bundled CLI’s actual clap parser before a terminal
opens; invalid arguments leave the form intact. CLI forms show their working directory,
and project paths in the New form use its explicit working directory. It generates forms rather than asking you to
construct a command line. Interactive operations run in embedded terminals, so login,
SSH host-key prompts, editors and confirmations behave as in the CLI. Message fields keep
quotes/newlines literally; command argument fields support grouping without shell expansion.
Automatic `--yes` flags and agent-only MCP entry points are deliberately not offered.
Global socket/workspace overrides cannot escape the selected workspace; subprocess arguments
after `--` remain literal. Every other user command and option remains discoverable.

Settings offers common defaults plus the complete TOML editor. Writes reuse the TUI's
comment-preserving, locked, crash-recoverable writer. Full-text saves check the text's hash
and the merged `boxes.toml`, so an unseen change is not overwritten. Unsaved editor drafts
stay in memory across navigation, not in browser storage. Closing/reloading asks before
discarding them. Hub settings require a deliberate hub restart. Suggestions and summaries share `[tui]`
preferences with the CLI and apply immediately on revisiting the conversation. Before the
first provider call, an in-app notice explains the exact text sent, models, providers and
usage cost. Only visible routine-update groups receive summaries; permission/question
updates remain visible and summaries are bound to their conversation and covered update.

macOS uses Command shortcuts; Linux uses Control. Terminal keys pass to the terminal,
including Ctrl-K/Ctrl-W. Ctrl/Cmd-K opens the palette outside terminals; Ctrl/Cmd-B toggles
the sidebar; Ctrl/Cmd-G toggles the grid. On macOS the focus filter is Cmd-Shift-F, preserving
normal text-editing Control keys. Narrow windows expose Chat / Needs you / Threads / More;
all manager pages remain accessible through More. In short windows, expanding a terminal
temporarily gives it the main area. Themes follow the system; Omarchy colors are re-read
when its current theme changes. No tray, multiwindow or launch-at-login integration is
required for this release.

## Architecture and extension points

- `crates/twr-app`: Rust application projection and `twr-desktop-bridge` stdio companion.
  The companion uses `twr-core::Client`, peer-credential checks, protocol negotiation,
  `ClientFold` snapshots and gap-free subscriptions. Snapshot fold mismatches fall back
  to replay. Missing blobs leave a visibly incomplete, non-actionable request, rather than
  repeatedly breaking the entire stream. A mutation is retried only when the hub protocol proves it was not executed; uncertain outcomes are never blindly replayed.
- `twr-core::slash`, `routine`, `card_order`, `config_edit`, `blob::hydrate_item` and `paths::project_param`:
  behavior shared by TUI and graphical clients. Rendering is separate from reducers.
- `desktop/electron`: window lifecycle, isolated and validated IPC, terminal processes,
  login environment, packaging and the companion transport. The renderer has no Node API.
  Only the main frame can use the preload API; model Markdown has no raw HTML or remote images.
- `desktop/src`: React components, typed action catalog and presentation of normalized
  events. It does not decide permissions or maintain a second thread-state reducer.

The bundled executables are copied to a content-addressed private engine directory before
starting a hub. A hub therefore keeps a stable executable path after an AppImage mount or
macOS translocated app disappears. Existing engine versions are retained while hubs may
still use them. Restarting or updating the desktop never silently stops an existing hub.

The iOS and Android clients share this React UI and Rust projection through an
authenticated gateway. Native vaults keep device credentials out of the web renderer.
See [mobile development, trust model and verification limits](mobile.md).

## Verification

```sh
cargo test --locked -p twr-app -p twr-core -p twr-tui
cargo clippy --workspace --all-targets --locked -- -D warnings
cd desktop
npm test
npm run build
npm run test:integration
npm audit
```

Integration launches **real Electron and a real temporary hub**, with twr's deterministic
fake adapter and a disposable login executable. It covers request revisions, streaming,
first-turn streaming, small-model disclosure and suggestion acceptance,
terminal input and attach, terminal keys/reload, interactive account login, config conflicts,
memory, renderer boundaries, narrow navigation, and the independent hub lifetime. Debug
fixtures use the existing debug-only user token inside their own private `TWR_HOME`; release
builds ignore that token. No real account or existing workspace is used.

`npm run test:qa:mac` exercises the packaged app through LaunchServices. It adds manager
navigation, slash-command target/flag checks, queue take-back, jobs and cancellation, memory
and context editing, park/resume, workspace switching, deletion/restoration, orchestrator
routing, and single-message read-aloud through a silent command engine. See
[the QA and parity record](desktop-qa.md) for the tested scope and limits.

`desktop.yml` adds Apple Silicon builds and an Arch Linux package job. The Arch job exercises
the app on a headless native Wayland compositor and builds the PKGBUILD. A headless compositor
cannot certify every GPU, fractional-scaling or Omarchy configuration: run the app on an actual
Omarchy desktop with your normal input method and monitor scale before publishing a release.

## Packages

Build on the target platform (Apple Silicon macOS or x86_64 Linux):

```sh
cd desktop
npm run package:mac        # release/*.dmg
npm run package:linux      # release/*.AppImage and linux-unpacked/
npm run package:arch       # release/arch/PKGBUILD and its checksummed source archive
cd release/arch
makepkg                   # twr-desktop-*.pkg.tar.zst
```

The Arch package includes its own matching Electron runtime and native terminal module,
so a system Electron upgrade cannot change the app's ABI. Its launcher uses the stable
`tools.twr.desktop` class for Hyprland rules. Chromium's sandbox stays enabled.

macOS packaging supports normal electron-builder signing/notarization credentials when
provided by the release environment. After packaging on macOS, `npm run test:packaged:mac` launches the actual app through
LaunchServices in a temporary installation to check production user authentication, the
bundled engine and native terminal. Local and PR installers are unsigned review builds;
they must be signed/notarized before public distribution. The CI workflow uploads review
artifacts and does not publish or install them automatically.
