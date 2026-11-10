# Desktop application

The Electron application lives in `clients/`. It targets Apple Silicon macOS and x86_64
Linux, including an Arch package for Omarchy. It uses the same hub, workspaces, accounts,
permissions and history as the CLI. You can use both clients at once. Closing the window
does not stop the hub; active interactive terminal sessions ask before closing.

## Development

Use the repository's pinned Rust toolchain, a current Node 24 or 26, npm, and the platform's
normal C/C++ build tools (Xcode command-line tools on macOS; `base-devel` on Arch).

```sh
cargo build --locked -p twr-cli -p twr-app
cd clients
npm ci
npm run dev
```

`npm start` opens the last production UI build. `npm run build` type-checks TypeScript and
builds the host, isolated preload and React UI. The development host takes a matched CLI
and companion from `clients/bin`, `target/debug`, or `target/release`. An explicit
`TWR_DESKTOP_BINARY_DIR` is accepted only in development. Packaged builds always use the
three bundled executables (CLI, companion and mobile gateway). `TWR_HOME` remains useful for an independent test installation.

The app recovers the login shell environment so that agents, git and developer tools are
found when launched from Finder or an Omarchy launcher. It preserves agent identity
markers. A desktop app launched by a twr agent cannot turn itself into a user client.
The window opens first: reading the login shell (at most 8 seconds, with no input, so an
rc file that prompts does not stall it) and staging the engines happen while it loads,
and the hub connection, CLI forms, terminals and other programs wait for them. A login
shell that fails or times out is recorded in the diagnostics log, and the app continues
with the environment it was launched with.

## Sidebar

The sidebar lists every workspace of the hub, as the TUI's does: `default` first, each with
`⚠` (requests waiting on you) and `●` (threads at work) counts, or `○` when its hub isn't
running. Under each workspace are its **Orchestrator** and its threads. The current
workspace shows its full thread cards (with the focus filter and parked toggle). Other
workspaces list up to six threads that aren't parked, as their hub last reported them:
those waiting on you first, then failed, then working, the last row `+N more` when there
are more. While the sidebar shows, the app re-reads the other workspaces every 15 seconds
over short-lived read-only connections; it never starts a stopped hub to read it.

Choosing an orchestrator or a thread in another workspace switches to that workspace and
opens it there, with no separate switch. A click selects a thread (its inspector), a
double-click talks to it, and `+N more` opens that workspace's grid. Each workspace's
`+` starts a new thread in it. Its name collapses or expands it, and the app remembers
that on this device. **New workspace** at the end of the list, and the folder button
beside the heading, create or remove workspaces. Needs you and the thread grid stay at
the top.

Drag the sidebar's right edge to resize it (200 to 440 px, default 260, and at most
about a third of the window). Double-click the edge to reset it. With the edge focused,
Left/Right change it by 10 px and Home/End jump to the limits. The width is saved with
the other layout settings (Settings › Layout & text) and restored on the next launch.

## Workflows and CLI coverage

The orchestrator conversation is primary. Selecting a thread opens its inspector; **Talk
to this thread** (or a double-click) deliberately changes the composer destination. The
composer always names its destination. Mentions reference threads without changing it.
Enter sends, Shift-Enter inserts a line, `/` offers commands, and `//` sends a literal slash.

Approval cards render requests literally, including command punctuation and paths. Every
answer names the exact displayed request revision. Requests with unavailable full payloads
cannot be answered from a preview. As in the TUI, a request's buttons and number keys answer
only once its options have been on screen for 600 ms: from when the card appears, scrolls
into view or the window is shown again, and from zero for a request raised again. Until then
they show dimmed and a click or key does nothing, so the second click of a double click, or a
key pressed as one card replaces another, can't answer the request that slid into place.
After an answer is accepted the card shows **Sent** and its buttons stay disabled until it
goes away. Answers need a live connection to the hub. Orchestrator messages use `say` with
the displayed control position, matching the TUI's relay semantics.

Links in messages name their full address on hover. Web and email links open in the
system browser at once, unless their text names another host or address than the one they
open, or they use plain `http:`: those first show the full address and ask. So does every
terminal hyperlink (OSC 8) whose text differs from its address, since a program sets the
two separately. A link to a path or another scheme (`[App.tsx](src/App.tsx:42)`) shows
as code with a copy button instead of a link that does nothing.

| CLI capability | Desktop workflow |
| --- | --- |
| Conversations, send, orchestrator selection/reset | Conversation, explicit destination, orchestrator actions |
| New/list/show, mode/model/runtime/account, rename | New-thread form, sidebar/grid, inspector and typed action forms; model and effort are pickers of the runtime's known models and efforts, with the runtime's default first and a custom model id last |
| Stop, park/resume, compact, trim, delete | Inspector actions; deletion uses the hub's confirmation card |
| Attention and responses | Needs you and inline request cards; revision-bound answers and notes |
| Message queues and dependencies | Queue with take-back/send-now, send-after and cancel-held-message forms |
| Attach, PTY fallback, arbitrary shell | Embedded xterm.js terminals using the matching real CLI or login shell |
| Account list/check/add/login/remove/native naming | Accounts page, typed actions and generated interactive forms |
| Boxes, SSH/container/dial-in setup, login, projects, tokens | Boxes page and generated interactive CLI forms |
| Jobs, slots, timeouts, output and cancellation | Jobs table, launch form, live log and cancellation actions |
| Usage and grouping/time windows | Usage meters and typed reporting form; CLI forms retain duration flags |
| Next-message suggestions, routine summaries | Shared hub small-model calls (`suggest.next`, `summary.thread`), first-use disclosure, the suggestion as the empty message box's placeholder taken with Tab or → (tap it on a phone), expandable routine updates |
| Context and memory | Revision-aware editors, note cards and standing-context editor |
| Speech, remote sessions, speaker controls | Read-aloud page and embedded CLI forms |
| Workspaces | Sidebar workspace tree (every workspace, its threads nested), creation and removal forms |
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
that calls count toward your plan's usage; it names no price. As in the TUI, a suggestion
shows greyed as the empty message box's placeholder: `Tab` or `→` puts it in the box (once
it has been on screen 300 ms, so a key pressed as it appears isn't taken for it), typing
types over it, and it is sent only when you press Enter. Only visible routine-update groups receive summaries; permission/question
updates remain visible and summaries are bound to their conversation and covered update.

Right-click offers Cut, Copy, Paste and Select All in text fields, Copy for selected text,
and Copy Link Address on a link (links are copied, not opened).
macOS uses Command shortcuts; Linux uses Control. Terminal keys pass to the terminal,
including Ctrl-K/Ctrl-W. Ctrl/Cmd-K opens the palette outside terminals; Ctrl/Cmd-B toggles
the sidebar; Ctrl/Cmd-G toggles the grid. The focus filter is Cmd-Shift-F on macOS
and Ctrl-Shift-F on Linux, preserving normal text-editing Control keys. Narrow windows expose Chat / Needs you / Threads / More;
all manager pages remain accessible through More. In short windows, expanding a terminal
temporarily gives it the main area. Dark is the default. Light and System are available in Appearance; System follows
the selected Omarchy theme on Linux. Native menus and window backgrounds follow
the client preference, which is restored on the next launch.

## Tray, login and background mode

The app adds a menu bar item on macOS and a tray icon on Linux. It shows how many
items need you in every workspace of the connected hub, lists up to five of them (the
shown workspace's first, under their workspace's name when there are several) and has
quick actions for a new thread, Launch at Login, background mode, updates and Quit.
Choosing an item shows the window, switches to its workspace and opens it in Needs you.
The Dock icon (macOS) and Unity-style launchers (Linux) show the same count as a badge.
The shown workspace's items are live; the others' are read every 15 seconds over
short-lived read-only connections, as the sidebar reads them, also while the window is
hidden. A stopped hub is not started to ask it.
On Linux the icon needs a StatusNotifierItem host: Waybar's `tray` module on Omarchy,
KDE Plasma, or GNOME with the AppIndicator extension. Without one, Settings says so
and background mode stays off, so the window can always be brought back.

Settings › Application has four switches:

- **Launch at login.** On macOS this registers a login item (macOS may ask you to
  allow it in System Settings › General › Login Items). On Linux it writes
  `~/.config/autostart/tools.twr.desktop.desktop`, which Omarchy (through uwsm),
  GNOME and KDE start at login. AppImages are started through the image itself. twr
  only removes an autostart entry that it created.
- **Show in menu bar / Show tray icon.**
- **Notify me of new requests** (on by default). A new permission or question, or a
  confirm or delete card, in any workspace posts a system notification while twr's window
  isn't in front: hidden in background mode, minimized, or behind another app. It names the
  thread and the workspace (`api · client-x`, *Asks for permission*) and, for the hub's
  cards, what kind of card it is (`twr · client-x`, *Asks you to confirm what the
  orchestrator proposed*, or *Asks you to confirm a deletion*), never what is asked or which
  threads a card would delete. Clicking it
  shows the window in that workspace at the request; it never answers it, and the
  request's buttons wait 600 ms after the window shows, as always. A notification goes
  away once its request is answered, anywhere, or when you bring the window to the front.
  What waits when twr starts or reconnects is not news, nor is what comes while you look
  at the window. Requests that come within a second go out together, and more than three
  as one (*4 things need you*). The system's Do Not Disturb and Focus settings apply.
  macOS asks whether twr may notify the first time it does (System Settings ›
  Notifications changes the answer later); a build macOS won't let notify, such as some
  ad hoc signed ones, shows nothing. The diagnostics log records the first notification
  posted, a system without notification support and notifications that failed. How this relates to the hub's `[notify]`
  command and the TUI's notifications: [notifications.md](notifications.md#the-desktop-app).
- **Keep running when the window is closed.** Closing the window hides it instead,
  so terminals, the hub connection and the badge stay live. A login launch then starts
  hidden on Linux. macOS 13 and later do not always tell an app it was opened at login, so
  the window may appear; close it and twr stays in the menu bar. Open twr again from the tray, the Dock or your launcher (a second launch
  shows the existing window). Quit from the tray menu, File › Quit (Ctrl-Q on Linux) or
  Cmd-Q on macOS.

## Updates

| Install | How it updates |
|---|---|
| macOS, Developer ID signed | Downloads in the background; **Restart to update**, or it installs when you quit |
| macOS, ad hoc review build | Notifies and links the new disk image (macOS only swaps in signed updates) |
| AppImage | Downloads in the background and replaces the AppImage file |
| Debian/Ubuntu package | Notifies with the `sudo apt install ./…deb` command and a download link |
| Arch package, or AUR (`twr-desktop-bin`) once published | Notifies; update with `pacman -U` and the new package, or your AUR helper (`yay -Syu twr-desktop-bin`) |

The app checks the latest GitHub release 45 seconds after launch and then every six
hours. Turn this off, or check by hand, in Settings › Updates or Help › Check for Updates.
It only reacts to a release once that release has the installers and update metadata for
this platform. Self-updating builds verify the download against the SHA-512 in the
release's `latest-*.yml`, and on macOS against the app's code signature. An AppImage also
needs the image's SHA-256 in `desktop-sha256sums.txt`, checked with its signature
(`desktop-sha256sums.txt.sig`, the release key `twr update` uses) when the download
completes and again before the image is replaced: someone who can change the release's
files but does not hold the release key cannot get an AppImage installed. A release without
that signature (one published before signing existed, or one still being attached) is never
installed automatically: the AppImage offers it as a manual download instead. The hub and its
agents keep running through an app update. Each app build keeps its engine in its own
content-addressed directory, so a running hub's executable is never replaced.

## Diagnostics

twr keeps a local log (`twr-desktop.log`, rotated at 2 MiB) in the folder shown by
**Help › Open Log Folder** and in Settings › Diagnostics (`~/Library/Logs/twr-desktop`
on macOS; under the app's configuration folder on Linux). The log records startup, update checks, hub connection failures, renderer errors
and crashed helper processes. Chromium crash reports are kept on disk and never uploaded.
If the window's page ends unexpectedly (out of memory, a GPU failure), twr reloads it, up to
three times in five minutes, then asks whether to reload or quit. A window that stops
responding offers Wait or Reload; the question closes itself if the window recovers.
Reloading keeps the hub, its agents and open terminals.
**Settings › Diagnostics › Copy diagnostics** (or **Help › Copy Diagnostics**) copies the
version, install kind, platform and session details, GPU status, memory use and the recent
log. Tokens, keys, URL credentials and your home folder path are removed first.

## Architecture and extension points

- `crates/twr-app`: Rust application projection and `twr-desktop-bridge` stdio companion.
  The companion uses `twr-core::Client`, peer-credential checks, protocol negotiation,
  `ClientFold` snapshots and gap-free subscriptions. Snapshot fold mismatches fall back
  to replay. Missing blobs leave a visibly incomplete, non-actionable request, rather than
  repeatedly breaking the entire stream. A mutation is retried only when the hub protocol proves it was not executed; uncertain outcomes are never blindly replayed.
- `twr-core::slash`, `routine`, `card_order`, `config_edit`, `blob::hydrate_item` and `paths::project_param`:
  behavior shared by TUI and graphical clients. Rendering is separate from reducers.
- `clients/desktop`: window lifecycle, isolated and validated IPC, terminal processes,
  login environment, packaging and the companion transport. The renderer has no Node API.
  Only the main frame can use the preload API; model Markdown has no raw HTML or remote images.
- `clients/shared`: React components, typed action catalog and presentation of normalized
  events. It does not decide permissions or maintain a second thread-state reducer.

The bundled executables are copied to a content-addressed private engine directory before
starting a hub. A hub therefore keeps a stable executable path after an AppImage mount or
macOS translocated app disappears. Linux packages ship the engines' SHA-256 digests in
`resources/bin/digests.json` (written by `npm run build:rust`), so a launch does not hash
the engines again, even from an AppImage's new mount path; each copy is verified against
those digests, and a manifest that does not match its engines is ignored. The Mac app
leaves the manifest out, because signing the app changes its engines after the digests
were taken: it hashes its engines once and keeps their digests, keyed by the bundle's paths. Existing engine versions are retained while hubs may
still use them. Restarting or updating the desktop never silently stops an existing hub.

Packaged builds strip `--remote-debugging-port`, `--remote-debugging-pipe` and Chromium's
fake-media switches, and their Electron fuses refuse `--inspect`, `NODE_OPTIONS` and
`ELECTRON_RUN_AS_NODE`, so another process running as you cannot drive the window, which can
answer requests and open terminals. The test suites need those switches:
`TWR_DESKTOP_TEST_AUTOMATION=1` keeps them, but a packaged build honours it only when its own
resources folder holds the QA marker `twr-qa-automation` (`desktop/automation.ts`); otherwise
it logs that it ignored the variable and removes it. Release packaging never writes the marker
and the release jobs fail if an installer carries it. `npm run test:qa:mac` adds it to a copy
of the app; Linux CI adds it, as root, to its test installs. The marker is only as protected
as the app's files:

- Debian and Arch packages install root-owned files: only root can add it there.
- On macOS, the marker inside the bundle is covered by its code signature. An app in
  `/Applications` is also protected by macOS's App Management privacy setting (macOS 13 and
  later), which stops a process without that permission (or Full Disk Access) from changing it,
  even when the user owns the files. A copy elsewhere with the marker added has a broken
  signature, which should make macOS ask before it reads the app's keychain items (saved remote
  hubs and offline history); this was not tested.
- An AppImage, or a portable archive in your home folder, is yours to rewrite, and on Linux any
  process running as you can already read the secret service and the hub socket: there the
  marker keeps a stray environment variable from opening remote debugging, not a determined
  process running as you.

The iOS and Android clients share this React UI and Rust projection through an
authenticated gateway. Native vaults keep device credentials out of the web renderer.
See [mobile development, trust model and verification limits](mobile.md).

## Verification

```sh
cargo test --locked -p twr-app -p twr-core -p twr-tui
cargo clippy --workspace --all-targets --locked -- -D warnings
cd clients
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

`desktop.yml` builds Apple Silicon packages and produces portable Linux bundles on
Ubuntu 22.04. The Arch job consumes that same bundle, exercises it on a headless
native Wayland compositor and builds the PKGBUILD. A headless compositor
cannot certify every GPU, fractional-scaling or Omarchy configuration: run the app on an actual
Omarchy desktop with your normal input method and monitor scale before publishing a release.

## Packages

The macOS app requires Apple Silicon and macOS 13 or later. Linux bundles target
x86_64 with glibc 2.35 or later (Ubuntu 22.04 and current Arch/Omarchy). Release
packaging rejects ELF dependencies above GLIBC 2.35, GLIBCXX 3.4.30 or CXXABI 1.3.13.

Build macOS packages on an Apple Silicon Mac. Build portable Linux packages on
Ubuntu 22.04, including the native terminal module; building it on a newer Arch
system can introduce newer glibc requirements. The CI producer/consumer jobs show
the complete cross-distribution flow:

```sh
cd clients
npm run package:mac        # release/*.dmg (+ *-mac.zip and latest-mac.yml for updates)
npm run package:linux      # on Ubuntu 22.04: release/*.AppImage, *.deb, linux-unpacked/
node scripts/arch.mjs       # same portable linux-unpacked/ → checksummed Arch source
# Transfer release/arch to an Arch machine, then:
cd release/arch
makepkg                   # twr-desktop-*.pkg.tar.zst
```

Every package carries twr's `LICENSE` and `NOTICE` and `THIRD-PARTY-LICENSES.html` in its
resources folder (and, for Arch, in `/usr/share/licenses/`); **Help › Third-Party Licences**
opens the last in your browser. `npm run package` writes it with `node scripts/notices.mjs`:
the Rust crates in the bundled engines, from cargo-about with the CLI release's settings
(`about.toml`, `about.hbs`), then every npm package bundled into the app or copied into it.
Packaging fails without cargo-about (`cargo install --locked cargo-about --version 0.9.2`).
For a local review build, `TWR_NOTICES_WITHOUT_RUST=1` packages notices without the Rust
crates, marked incomplete at the top of the file; never distribute such a build.

The Arch package includes its own matching Electron runtime and native terminal module,
so a system Electron upgrade cannot change the app's ABI. The Arch package reuses
the portable producer's native terminal module without recompiling it. Its launcher uses the stable
`tools.twr.desktop` class for Hyprland rules. Chromium's sandbox stays enabled.

The AUR package `twr-desktop-bin` is not on the AUR yet: each release builds and checks it,
and pushes it once the repository has the AUR key (`AUR_SSH_PRIVATE_KEY`, below). It installs
the same files from the release's
`twr-desktop-<version>-linux-x64.tar.gz`. `node scripts/aur.mjs <archive>` writes its
`PKGBUILD` and `.SRCINFO`. CI builds that package from the identical archive, checks
`.SRCINFO` against `makepkg --printsrcinfo`, and installs it over the release package.

### Releases

Pushing a stable `v<version>` tag runs `release.yml`. That publishes the CLI release,
calls `desktop.yml` for the same commit (with all of its QA), then attaches the installers,
update metadata and `desktop-sha256sums.txt` to the release. Prerelease tags skip the
desktop. Optional secrets, all in the protected `release` environment (never repository-wide
secrets, which pull-request runs can read):

- `CSC_LINK`, `CSC_KEY_PASSWORD`, `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`,
  `APPLE_TEAM_ID`: a Developer ID certificate (base64 `.p12`) and notarization
  credentials. With them, the Mac app is signed, notarized and stapled, and
  `latest-mac.yml` is published so Macs update themselves. Without them, the release
  carries the ad hoc review disk image only.
- `AUR_SSH_PRIVATE_KEY`: the key registered on the
  `twr-desktop-bin` AUR account. With it, the release pushes the new PKGBUILD to the AUR.
  Otherwise, push the `twr-desktop-aur` workflow artifact by hand.

macOS packaging supports normal electron-builder signing/notarization credentials when
provided by the release environment. After packaging on macOS, `npm run test:packaged:mac` launches the actual app through
LaunchServices in a temporary installation to check production user authentication, the
bundled engine and native terminal. `package:mac` produces an ad hoc signed local review
installer. For Developer ID signing and notarization, use `npm run package -- --mac --arm64`
with the release environment’s signing credentials; that path retains hardened runtime. The CI workflow uploads review
artifacts and does not publish or install them automatically.
Review builds select the ad hoc identity in pull-request CI. Before uploading the
installer, CI verifies the complete app signature and checks that it is ad hoc,
with no signing team. These signatures require no Apple account; Developer ID
signing and notarization remain the distribution path for public releases.


### Needs you across workspaces

The Needs you inbox groups requests by hub and workspace. Filter it to permissions,
questions, completed work that needs review, or other issues. J/K and the up/down arrow
keys move between current requests outside text fields and dialogs; Previous and Next provide
the same controls on touch screens. Number keys answer only the focused request's
current revision, once it has been on screen for 600 ms. Inputs keep their normal typing behavior.

Other workspaces show a summary with an Open action. Open the workspace before
answering so the full live request and its current revision can be reviewed. Saved
summaries for disconnected hubs are labeled; they cannot authorize a response.
Refresh reads each running workspace without moving your active conversation.
Only one refresh runs at a time. Listing the workspaces may take up to 5 seconds (it asks
each hub in turn); if it takes longer or fails, the refresh uses the last list that worked.

### Search saved conversations

Use the search button in the header or Cmd/Ctrl-F. Search matches saved messages,
tool input and output summaries, reports, and recorded decisions in the active
workspace, including archived agent output and text moved to the blob store. Filter
by conversation to narrow the results. Search older events continues through older
pages, including pages without matches. The first archive read after an upgrade may take
longer while an existing archive receives its search index.

Selecting a result opens its exact event and surrounding history, labeled with the
hub, workspace, thread and event sequence. It keeps your current message destination
and draft. Large context fields carry a display-truncated marker. The bounded view
retains the event id and sequence and omits runtime extras and blob references.
Use `twr events THREAD --hydrate` to read the full recorded data. Search runs on
the hub, so it requires a connection to that workspace.

## Layout and offline recovery

The window reopens with the size, position and maximized or full-screen state it had when
it was last moved, resized or closed (saved in the app's `desktop.json`). It is moved and
shrunk to fit the display that shows most of it; if that display is gone, it keeps its size
and opens in the default place.

Settings → Layout & text offers Compact and Comfortable density, independent interface and code text sizes, and sidebar/inspector width sliders. Drag the sidebar's right edge or the inspector's header edge, or focus the separator and use Left/Right, Home or End. Widths are constrained to fit a tiled window and saved on this device. Mobile conversation text follows the system text-size setting; chrome and terminal sizes use a bounded scale so their controls remain usable.

Recent conversations and message drafts are encrypted locally using the macOS keychain, a Linux secret service, iOS Keychain or Android Keystore. Linux sessions without an unlocked keyring keep live operation available and report that encrypted persistence is unavailable; no plaintext fallback is used. History is bounded, retained for up to seven days and can be cleared in Settings. Configuration/editor drafts stay in memory because they may contain credentials.

When a saved hub is offline, the app shows dated cached history with request buttons disabled. Plain messages can be saved in the outbox for their original workspace and destination. Reopening the app preserves the original request ID; reconnecting retries that ID within 23 hours. An expired receipt or an unknown delivery outcome requires review in the conversation before a new message is sent. Removing an outbox entry never recalls a message already delivered to the hub. Shell commands, slash commands and permission answers require a live connection. Forgetting a hub removes its local encrypted history, drafts, outbox and bootstrap metadata.

See [remote setup and QR pairing](remote-clients.md) and [thread attachments](attachments.md) for the shared desktop/mobile flows.
