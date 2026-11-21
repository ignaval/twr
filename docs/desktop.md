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

`npm run test:conversation-scroll` checks reading position during delayed history loads,
scroll-relative user-message shortcuts, repeated keyboard jumps, short histories,
history paging (including responses during jump settlement), live updates, and thread
switches on desktop and phone layouts in a browser, using a disposable host. It needs
Playwright's Chromium or `TWR_CHROMIUM_PATH`.
Synthetic touch and momentum cases select the virtualizer's iOS path and verify deferred
prepend adjustments and shortcut selection while those adjustments are held. They do not
reproduce native WebKit or physical iOS scrolling.

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
are more. While the sidebar shows, the app re-reads the other workspaces every 15 seconds,
all of them with one `twr ws list --json --detail`, which asks each running hub once; it
never starts a stopped hub to read it.

Choosing an orchestrator or a thread in another workspace switches to that workspace and
opens it there, with no separate switch. A single click opens a thread's conversation,
and `+N more` opens that workspace's grid. Each workspace's
`+` starts a new thread in it. Its name collapses or expands it, and the app remembers
that on this device. **New workspace** at the end of the list, and the folder button
beside the heading, create or remove workspaces. Needs you and the thread grid stay at
the top.

Drag the sidebar's right edge to resize it (200 to 440 px, default 260, and at most
about a third of the window). Double-click the edge to reset it. With the edge focused,
Left/Right change it by 10 px and Home/End jump to the limits. The width is saved with
the other layout settings (Settings › Layout & text) and restored on the next launch.

## Workflows and CLI coverage

The orchestrator conversation is primary. Selecting a thread opens its conversation; the
**Show thread details** button beside its heading toggles its inspector. The
composer always names its destination. Mentions reference threads without changing it.
Enter sends, Shift-Enter inserts a line, `/` offers commands, and `//` sends a literal slash.
A line that starts with `!` runs a shell command; start it with `\!` to send a message that
begins with `!`, as in the TUI. A message you take back from the queue, move back from the
device's saved messages, put back after an orchestrator switch, or take as a suggestion
comes back escaped this way when it starts the box, so Send sends it as a message again,
never as a command. After text already in the box it comes back unescaped, as it was sent:
Send reads `//` and `\!` only at the start of the box. When the box already holds a
`/` command or a `!` line, that line is escaped too, so Send sends both as one message
rather than running the command and clearing the text that came back.

Above the history, the orchestrator and each thread's conversation show the first line
of your preceding message, on desktop and phone. At the bottom, this is your latest
message. While browsing, it is the last user message whose start lies above the history
viewport's top edge, with a 1 CSS pixel tolerance. Reaching or passing its start while
scrolling upward changes the shortcut to the previous user message. Scrolling downward
updates it in the other direction.
The clickable link fits its displayed contents; the empty remainder of the row is not
clickable. Long previews truncate within the conversation width as the window narrows.

Click the shortcut (or focus it and press Enter or Space) to jump and focus that message,
with a visible keyboard focus outline. Each activation steps backward, even if a short
history prevents the destination from aligning at the top. Manual scrolling resumes
selection from the reading position; **Jump to latest** resumes bottom-follow. At the
earliest boundary, the shortcut says **No earlier messages from you** and does nothing.
It stays focusable while loading and at that boundary so changing its target does not
discard keyboard focus. Scrolling and background loads never transfer focus.

Recent history and live updates appear first. Older turns, including all their pages,
load in the background when the predecessor is not yet known, skipping automatic turns.
If an older page fails, recent history stays available and **Load earlier turn** retries
it. Offline lookup uses history already saved on this device and asks you to connect
when older history is needed. Thread switches reset the shortcut and ignore delayed
results from the previous conversation.

Loading older turns keeps the message you are reading at the same position, including
when you scroll while a background or manual history request is pending.
History arriving during a shortcut jump also preserves the message you clicked as its
destination.

Approval cards render requests literally, including command punctuation and paths. Every
answer names the exact displayed request revision. Requests with unavailable full payloads
cannot be answered from a preview. As in the TUI, a request's buttons and number keys answer
only once its options have been on screen for 600 ms: from when the card appears, scrolls
into view or the window is shown again, and from zero for a request raised again. Until then
they show dimmed and a click or key does nothing, so the second click of a double click, or a
key pressed as one card replaces another, can't answer the request that slid into place.
In a conversation, its requests take at most about a third of its height and scroll in their
own area, so the composer stays in view; while a request goes on below that area's edge
(its buttons, in a short window), the edge fades out, and Tab brings each button into view.
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
| Loops (`twr loop`, `/review`, `/implement`, `/loop`, `twr thread new --loop implement`) | The inspector's Loops section, Send as implementation loop, the Review once and Review loop buttons on a finished turn with changes, the New thread form's switch, the loop chip on a thread's sidebar card, the setup card and Settings › Loops ([Loops](#loops)) |
| Message queues and dependencies | Queue with take-back/send-now, send-after and cancel-held-message forms |
| Attach, PTY fallback, arbitrary shell | Embedded xterm.js terminals using the matching real CLI or login shell |
| Account list/check/add/login/remove/native naming | Accounts page, typed actions and generated interactive forms |
| Boxes, SSH/container/dial-in setup, login, projects, tokens | Boxes page and generated interactive CLI forms |
| Jobs, slots, timeouts, output and cancellation | Jobs table, launch form, live log and cancellation actions |
| Usage and grouping/time windows | Usage limits (`/usage` above the message box, the header's gauge, the Usage page) and typed reporting form; CLI forms retain duration flags |
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

Settings › Orchestrator also sets the orchestrator's [access](orchestrator.md#full-access)
(`orchestrator.access`): **Limited** or **Full**, each described in one line. Full asks
first, in words, what it gives up; Limited doesn't. The answer says when it applies (at
once, when the orchestrator's current turn ends, or at its next start) and warns when
`config.toml` couldn't be updated, so the level lasts only until the hub restarts. The
palette's **Orchestrator access** does the same. A hub too old to report the level shows
neither, and on a phone the choice needs a device with admin access. The orchestrator's
conversation shows each change of a setting made through the hub as a line, as the TUI does.

Right-click offers Cut, Copy, Paste and Select All in text fields, Copy for selected text,
and Copy Link Address on a link (links are copied, not opened).
macOS uses Command shortcuts; Linux uses Control. Terminal keys pass to the terminal,
including Ctrl-K/Ctrl-W. Ctrl/Cmd-K opens the palette outside terminals; Ctrl/Cmd-B toggles
the sidebar; Ctrl/Cmd-G toggles the grid. The focus filter is Cmd-Shift-F on macOS
and Ctrl-Shift-F on Linux, preserving normal text-editing Control keys. Narrow windows expose Chat / Needs you / Threads / More;
all manager pages remain accessible through More. In short windows, expanding a terminal
temporarily gives it the main area. Dark is the default. Light and System are available in Appearance; System follows
the selected Omarchy theme on Linux. In every theme, every control's edge and state reads
at 3:1 against what is beside it: the edges of fields, search boxes, the message field,
checkboxes and switches; a slider's track and thumb; a checkbox's check; the selected
segment, tab, thread card and list item; a pressed toggle; and the focus ring. The app draws
checkboxes, switches and sliders itself for this, the same in every engine. Lines that only
separate or frame content (dividers, card and dialog outlines) stay light. On Omarchy the app
reads the theme's `colors.toml` (its `background`, `foreground`, `accent`, `selection`,
`red`, `yellow` and `mode`) from `~/.local/state/omarchy/current/theme`, where Omarchy 4
keeps it, or from `~/.config/omarchy/current/theme` on an older Omarchy, and derives every
colour it draws from them. It checks the file every 5 seconds and follows a theme switch
without a restart; the moment in a switch when `omarchy-theme-set` has removed the old theme
and not yet moved the new one in is not taken for "no theme". Text in every colour reads at
4.5:1 on every background it sits on, and the controls at 3:1 as above: a colour that
already does is used exactly as the theme gives it, and one that does not keeps its hue and
moves its lightness only as far as it takes. Thread names in their own colour get an ink
for the theme's surfaces the same way. A theme without a valid `background` and
`foreground` leaves the app's own colours in place. Disabled controls are dimmed below
these ratios, which WCAG allows for inactive controls. Native menus and window backgrounds follow
the client preference, which is restored on the next launch. On Linux the menu bar is the
exception: Electron draws it from the GTK theme the app started with, whatever the
preference or the Omarchy theme, so under the default GTK theme it stays light over a dark
theme.

The message field grows with what you write, wrapped lines included, at every text size,
up to 240 px or 40% of the window, then scrolls inside. In a window with less room than
that (a short one at a large text size, or with a notice above the conversation) it stops
where the composer still fits, so Send, the microphone and Upload stay in view. It shrinks
again when the text is deleted or sent.

Files you attach with the paperclip show inside the message box, above the text: an image as
a 64-pixel tile, any other file as a chip with its name and size, each with a ✕ that cancels
or removes it. No path shows, and nothing is written into the draft: the message names the
conversation's uploaded files (`Uploaded file: <name>` and the path) as it is sent, and a sent
message shows them as chips with the name only. Send waits, disabled and saying why, while a
file is still uploading. Uploaded files are saved with the draft, so they are still attached
after a restart. See [thread attachments](attachments.md).

Drafts are saved on this device a moment (350 ms) after you stop typing; a file is saved as
soon as it is uploaded, removed or put back. Closing the window or quitting saves what is still
waiting first: the window waits for that save, up to 2 seconds, before it closes. The phone
app saves it as it goes to the background.

### Model and effort

The message box's footer row shows, at its right before the microphone and Send, what its
destination runs: the orchestrator, or the thread you opened. It names the runtime by a
short mark (`Cl` Claude Code, `Cx` Codex, `Gk` Grok Build), the model (the one the runtime
reports it runs, else the one you asked for, else **Default**) and the reasoning effort
(**High**, or **Default**). The command hints beside it shorten, then hide, as the window
narrows. Click it, or press Ctrl+Shift+M (⌘+Shift+M on a Mac), to open the picker above the
message box:

- On the left, the runtimes twr lists models for (Claude Code, Codex, Grok Build), the
  current one checked. A thread on another runtime, such as an ACP agent, lists its own
  first. For the orchestrator, a runtime whose program the hub doesn't find says **not
  installed** under its name (`orchestrator.show` `installed`, read as the picker opens).
- In the middle, **Search models…**, then the highlighted runtime's models with a few words
  on each, the current one checked: **Default** (the runtime's own), the models twr knows,
  and **Custom model id…**, where you type an id and press Enter or **Use**. The row then
  shows the id, chosen, and opens again with it.
- On the right, **Effort** with the model it is for, and a row for each effort the runtime
  takes, after **Default**.

The focus starts in the search. ↑ and ↓ move, Enter picks a model, and the focus moves on
to the efforts with the popup still open; Enter or a click on an effort applies both at
once. ← and → move between the columns, ← from an empty search to the runtimes. In the
models the focus stays in the search, the highlighted model its active row; in the runtimes
and the efforts it is on the highlighted row, so a screen reader says each one, and Tab
reaches each column at its highlighted row. Typing on a runtime or an effort goes on in the
search. **Done**
applies what is picked so far, an id typed in the custom field included, as an effort picked
while the field is open does; with the field empty, the picker stays open and asks you to type
an id or pick a model. A long list scrolls to keep the highlighted row in view, in
every column. Esc or **Cancel** closes the picker with nothing changed and
puts the focus back in the message box; so does a click outside, or the focus leaving it
(Tab past **Done**), leaving the focus where you clicked or tabbed. Esc works wherever the focus is in the picker, **Done** and **Cancel** too. The
popup opens above the message box, as tall as the room there within the conversation
column; with a tall draft in a short window that room is too small, and it opens over the
message box instead. On a
`pty` thread there is no search: the focus starts on its runtime in the rail and follows the
highlight, so ↑, ↓ and Enter pick another runtime. In a narrow window the columns narrow
to fit the message box and each scrolls its own rows. On a phone the picker is a sheet
at the bottom of the screen, its columns stacked at their full height, which the sheet
scrolls through, every row a full-size touch target.

Only what changed is sent. For a thread on its own runtime that is `thread.set_model` with
the model, the effort, or both; picking the runtime's default sends `default`. Another
runtime moves the thread, so the app asks first: its next message starts a new session of
that runtime with a handoff, or, back on a runtime the thread left a session of, resumes that
session ([Moving a thread back](runtimes.md#moving-a-thread-back)). A model or effort you
pick is sent, **Default** as `default`; one you leave as it is (**Done** before picking it)
is left out, so a resumed session goes on with the one it had there, and a new one starts
with the runtime's default. `default` for a setting the runtime keeps on resume starts a new
session instead. The question says which of these can happen. A `pty` thread runs its own command line, so the picker offers
it only another runtime. The orchestrator on its own runtime changes the same way, at once:
`thread.set_model` on its thread with what changed, and its runtime restarts on its session
with them, after its turn when it is busy, with no handoff
([orchestrator.md](orchestrator.md#changing-the-orchestrators-model-and-effort)). Nothing
names its account or agent, so it stays on the login and ACP agent it runs. Another runtime
is a switch (`orchestrator.use`), a new orchestrator session with a handoff, so the app asks
first: **Switch with summary** first asks the current orchestrator for its own summary,
**Switch without summary** hands over only what the log shows
([orchestrator.md](orchestrator.md#switching-the-orchestrator)). To a runtime that isn't
installed, the question says the new orchestrator can't start until it is. After a switch,
the hub's warning and a start that failed (`warning`, `start_error`) show as a notice, as
does a thread move's warning. A switch drops the messages
queued for the orchestrator, so with messages waiting the question says how many. When the
switch dropped some, the app says so, and when some were yours it offers **Put back**: they
return to the orchestrator's message box, their files as attachments, to send to the new
orchestrator. A switch gives back at most 4 MiB of them; the app says how many more of yours
it dropped that can't be put back.

What you pick is planned against what runs as the picker opens. If that changes underneath
it (another client switches the orchestrator, sets or moves the thread, or deletes it), the
picker closes and the screen reader is told why; open it again to pick from what runs now. A
confirmation it asked is taken back the same way, and one answered anyway changes nothing and
says to pick again. So is a choice whose confirmation the picker hasn't asked yet, or has,
when the picker goes away first: you leave the conversation, or open another workspace. The
app's calls go to the workspace that is open, so nothing it asked for another one is sent.

While the change is under way the button shows a spinner; a refusal from the hub shows as
any other error. The button is disabled while the hub is disconnected and on a device
without the `operate` scope, and hidden while the conversation has no destination. Switching
the orchestrator and moving a thread to another runtime need the `admin` scope, so on a paired
device without it the picker lists the other runtimes greyed out, the orchestrator's and a
thread's alike: picking one says it needs a device with admin access, while the models and
efforts of the runtime it runs still change, the orchestrator's too. The desktop
app on its own hub has every scope. A change
made elsewhere (by the orchestrator, `twr model`, the TUI) shows as soon as the thread list
updates. The command palette's **Change model, runtime or account** and **Switch
orchestrator** forms still work, and are where accounts are chosen; **Switch orchestrator**
has a reasoning effort field too. Before the first orchestrator, **As configured** leaves it to `[orchestrator] effort` and **Runtime default** is the runtime's own; once one runs, the configured effort no longer applies, and the unset choice is **Runtime default**. An effort the runtime chosen there doesn't take is cleared, so the form never sends one you can't see.

### Usage limits

How much of each subscription window is left, per runtime and account, as the TUI's status
bar shows it ([where the numbers come from](runtimes.md#subscription-usage)). Each window is a
row: its name (`5-hour`, `Weekly`, `Monthly`) and the share left, a bar filled to that share,
and when it resets (`resets in 2h`; the local time is in its tooltip). A thin tick on the bar
marks even pace, the share of the window's time still to run, where the share left would be
if you spent it evenly; it shows only for a window whose length and reset time the runtime
reports. The bar turns amber at 25% left or less and red at 10% or less. Data 10 minutes old
or more is dimmed and says its age (`as of 12m ago`). A runtime with several accounts names
each as threads do (`Claude:work`; a native login that shares its name with a named account
says `(native login)`). With no window to show (no session has reported one yet, or every one reported has reset, which `usage.get` leaves out), it says there are no current limits.

- **`/usage`** in the message box opens the limits in a panel above it. Close it with its ✕
  or Escape. In a short window the panel gives way before the message box: it shrinks and
  its rows scroll inside it, so Send stays on screen. On a phone with very little height
  left (300 points or less, the keyboard open) it steps aside while you type and comes back
  when the keyboard closes.
- **The gauge** in the conversation's header, beside the read-aloud and tool-call buttons,
  opens them in a popover under the header's right edge, kept inside the conversation however
  narrow the window. Escape (in the popover or on the gauge), a click outside it or the focus leaving it closes it. On a phone, **Usage limits** in the conversation's menu opens the
  panel.
- **The Usage page** in the side navigation shows them in full, with the usage by thread,
  project or runtime. The panel and the popover link to it.

Resets, countdowns and the pace tick go by the hub's clock (`usage.get` says what time it is
there), so a device whose clock is off still shows the windows that haven't reset. The clock
goes on from the hub's last answer, also while reading it again fails: the countdowns move
and a window that resets meanwhile goes. The panel
and the popover read `usage.get` when they open, again when the hub's live view
says an account's usage changed (an agent reported new numbers), and once a minute while they
are open; never while closed.

### Loops

A review loop has a reviewer model check a thread's changes round by round while the thread
fixes what holds up. An implementation loop gives a thread a task: it plans it, implements it,
the same review runs on its work, and the loop commits it at the end ([the
design](design/loops.md)). The app starts and follows both kinds in threads on the hub's own
machine.

- **Send as implementation loop.** When you talk to a thread, Send has a caret (a `↻`
  button beside Send on a phone) with **Send as implementation loop**: what you wrote is the
  loop's task. A click outside the choice, or the focus leaving it, closes it. Sent with nothing written, it asks first, since the task is then what your
  conversation with the thread already settled. It needs a live connection, like every loop
  action: without one, or on a read-only device, the choice says why and nothing is saved
  for later.
- **Start one.** Select a thread: its inspector has a **Loops** section with **Review
  loop**, **Review once** (a [single review pass](usage.md#a-single-review-pass): one review,
  the thread's triage, the report) and **Implementation loop…** (which asks for the task, empty meaning what the
  conversation settled) and, under them, the settings a loop would run on (profile,
  implementer, reviewer, and whether they are this project's) or `first run: you will be
  asked to choose models`, with **Change…**. When a thread's turn ends with changes in its worktree (files not committed, or
  commits ahead of its base) and no loop has reviewed them, its sidebar card, its
  conversation and the orchestrator's **Latest thread reports** also show **Review once** and
  **Review loop** buttons (the grid's tiles do too in a narrow window such as a phone's, where the grid is
  the thread list).
  `[loops] offer_review = false` turns those buttons off, and it hides while the thread's setup
  card is open and on a thread someone stopped (send it a message first; a thread stopped
  only because the hub shut down keeps the button). `/review` in the composer does the same for the thread you talk to or have
  selected, and takes the CLI's options (`--setup`, `--once`, `--scope`, `--profile`, `--reviewer`,
  `--implementer`, `--workers`, `--commit`). `/implement` starts an implementation loop with
  the same rule as the terminal UI: in a thread's conversation every word is the task;
  elsewhere a first word that names a thread is the thread, else the selected one, and the
  rest is the task (`--setup`, `--profile`, `--reviewer`, `--implementer`, `--workers`,
  `--commit`, `--plan-check`; after `--` every word is the task's). `/loop` only controls a
  running loop and opens the settings; `/loop implement` says to use `/implement` or
  `/review`.
- **A new thread with a loop.** The New thread form has a **First prompt** and, under it, a
  **Run as an implementation loop** switch with the settings line and **Change…**. On, the
  first prompt is the loop's task, and required (**Show the loop's setup card first** asks
  for the card even with saved settings); off, the first prompt is sent to the thread once
  it exists. `/new <project> --implement <task>` opens the form with the switch on.
- **The setup card.** The first loop of either kind, a loop you ask to check
  (`/review --setup`, `/implement --setup`), or one whose saved settings no longer hold,
  raises a setup card instead of starting. It shows in Needs you, in the thread's
  conversation and in its inspector, ordered with questions, titled for its kind
  (**Implementation loop setup**). It lists what the loop would run on (an implementation
  loop's card adds its plan check and commit rows; there is no Workers row when the
  implementer's runtime runs no subagents), the hub's warnings, and anything to change
  first. An implementation loop's card holds its task, whole, in a box that scrolls
  inside the card; the files the task names show there as chips with their names only.
  **Start and save** starts the loop and saves its settings; it waits while
  the card lists something to change first. **Change…** opens the settings editor on the
  card; saving there raises the card again with the new values. A card that holds a task has
  **Run without a loop**, which sends the task to the thread as an ordinary message and
  saves nothing. **Cancel** starts and saves nothing (a held task is dropped). Every answer
  applies to the revision shown, under the same 600 ms rule as other requests. The card has
  no Dismiss. On a phone it opens as a sheet.
- **While it runs.** The thread's sidebar card (and in a narrow window its grid tile) shows the hub's loop badge and line
  (`REVIEW LOOP high · ladder m3 · ledger 2`, `IMPLEMENT LOOP high · plan`, wrapping on a
  narrow card rather than cut);
  a click on it opens the inspector at its Loops section. There you see every phase with
  the current one marked (and a plan check, sweep or verification the loop passed over as
  skipped), what it runs on and whose settings they are, each round's findings and
  new-class count (the round whose review still runs reads `under review`; open a round for
  its findings and the thread's verdicts; an implementation loop's plan check is round
  `p1`), the ledger, and whether it waits on the thread, the review or you. A sweep the hub
  audits (`sweep = "runs"`) lists its rules like rounds, each with what its audit found
  (`2 findings`, `no place breaks it`, `auditing`, `waiting for a slot`, `not run: …`) and
  the thread's verdict, open for the audit's findings; while they run the card reads
  `auditing 3 of 5 rules` and the loop line `sweep · auditing 2/5`. An
  implementation loop also shows its plan (revision, contract risk yes or no with the reason,
  and where `plan.md` is), its implement turn's result and gates, and what it commits.
  **Pause** or **Resume**, **Skip to verify** and **Stop** (after a confirmation) control
  it, as do `/loop pause|resume|skip|stop`; Skip to verify waits for the ladder in an
  implementation loop, which has built nothing to verify before it, and says so. A stuck
  loop raises a card with **Resume** and **Stop**. A loop started from the app says what it
  runs on (`api started a review loop: low · implementer … · reviewer … (saved
  settings)`), and a screen reader hears a setup card, a stuck loop and a loop that ends
  (`api's review loop ended: clean`; `api's implementation loop ended: clean; committed
  abc1234`, `not committed: …`, or both when some repositories were committed and others
  not), in any thread.
  Once it ended, the inspector shows how, an implementation loop's commits (a short sha per
  repository), then why any other repository was not committed, and what the last commit
  check noted that is no problem (the thread wrote no `twr-commit` block, say) in quiet
  text. **Open report** reads the loop's `report.md` through the hub and shows it in a
  dialog (a sheet on a phone), with where the hub wrote it, small, and a copy button; a
  report over 256 KiB ends with the hub's marker line, and a running loop's says it is not
  written yet. A `read` device can open it. While the loop's turn is the thread's last, its
  sidebar card and **Latest thread reports** show the same in a line (`IMPLEMENT LOOP
  clean · committed abc1234`, `clean · committed abc1234 · not committed: …`).
- **On a remote box.** A loop in a thread on a remote box runs as on the hub's own. Its
  setup card checks what the box has and names it (`codex isn't installed on box devbox:
  reviewing with claude-code · claude-opus-5-5 instead`), and the editor says `Runtimes as
  found on box devbox`. The box runs every runtime on its own login, so a saved reviewer
  account other than that brings the card back on the default account and says why (`box
  devbox runs codex on its own login, not account work`). When the box goes away during a
  review run, the loop line says `waiting on box devbox`, the inspector `waiting on box
  devbox` and the round `m1 waits for box devbox`, and a screen reader hears it once; the
  loop's controls still work. When the box is back the round runs again (`m1 under review
  again`) and the loop goes on, and that is said once too. While a box is away nothing
  offers to start a loop on its threads: the buttons wait and say `Box devbox is away: a
  loop can start once it is back.`
- **Settings.** **Change…** in the Loops section and `/loop settings` open the editor:
  the four presets, the implementer (the thread's own, or a runtime, model and effort), its
  workers where its runtime has subagents, the reviewer (runtime, model, and account where
  it has several), the profile, an implementation loop's plan check and commits, what a
  review loop commits, **Sweep** (the thread audits the rules, or a reviewer run per rule:
  one reviewer call per rule, the hint under it says), and **Save for** (all projects
  or this project). Only runtimes that are installed and logged in can be chosen; the
  others show why not. The reviewer's list also has the ACP presets: one that names its
  read-only mode reads `ex (acp, read-only mode)`, and one that doesn't is disabled with the
  reason ([usage.md](usage.md#acp-reviewers)); the implementer's list has none. A line
  under the reviewer's picker names every reviewer that can't be picked, with why (`Can't
  review: bare (acp preset bare names no read-only mode)`), and an ACP preset picked says
  the mode its read-only rests on and the efforts every round of the profile must use; a
  screen reader hears both with the picker. A hub that does not check its agent CLIs (`TWR_AGENT_PROBE=off`)
  can't say, so every runtime can be chosen, marked `not checked`. **Settings › Loops** shows the same editor, the projects with their
  own settings (open or remove them) and **Reset all**.

Starting, controlling, answering a setup card and saving settings need a live connection
and, on a paired device, the `operate` scope: none of them goes to the outbox. A read-only
device sees the chip, the progress and the report path, with the actions disabled.

## Tray, login and background mode

The app adds a menu bar item on macOS and a tray icon on Linux. It shows how many
items need you in every workspace of the connected hub, lists up to five of them (the
shown workspace's first, under their workspace's name when there are several) and has
quick actions for a new thread, Launch at Login, background mode, updates and Quit.
Choosing an item shows the window, switches to its workspace and opens it in Needs you.
The Dock icon (macOS) and Unity-style launchers (Linux) show the same count as a badge.
The shown workspace's items are live; the others' are read every 15 seconds, as the
sidebar reads them, also while the window is hidden. A stopped hub is not started to ask it.
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
- **Notify me of new requests** (on by default). A new permission or question, a confirm
  or delete card, or a loop's setup card or stuck loop, in any workspace posts a system
  notification while twr's window isn't in front: hidden in background mode, minimized, or behind another app. It names the
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
  posted, a system without notification support and notifications that failed. A loop that
  ends notifies once too, with how it ended (`api · client-x`, *Review loop clean after 6
  rounds*; an implementation loop adds *committed abc1234*, *not committed: …*, or both), under the
  same rules: not when it ended while you looked or before twr connected, and more than three
  at once as one (*4 loops ended*); it goes when clicked or when the window comes to the
  front. How this relates to the hub's `[notify]` command and the TUI's notifications:
  [notifications.md](notifications.md#the-desktop-app).
- **Keep running when the window is closed.** Closing the window hides it instead,
  so terminals, the hub connection and the badge stay live. A login launch then starts
  hidden on Linux. macOS 13 and later do not always tell an app it was opened at login, so
  the window may appear; close it and twr stays in the menu bar. Open twr again from the tray, the Dock or your launcher (a second launch
  shows the existing window). Quit from the tray menu, File › Quit (Ctrl-Q on Linux) or
  Cmd-Q on macOS.

## Updates

| Install | How it updates |
|---|---|
| macOS, Developer ID signed (also from Homebrew's cask) | Downloads in the background; **Restart to update**, or it installs when you quit |
| macOS, ad hoc review build | Notifies and links the new disk image (macOS only swaps in signed updates); or run `install.sh --desktop` again |
| AppImage | Downloads in the background and replaces the AppImage file |
| Debian/Ubuntu package | Notifies with the `sudo apt install ./…deb` command and a download link; or run `install.sh --desktop` again |
| Arch package | Notifies; update with `pacman -U` and the new package, or run `install.sh --desktop` again |
| AUR (`twr-desktop-bin`) | Your AUR helper updates it with the system (`yay -Syu`, Omarchy's **Update**); the app also notifies |

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
memory, renderer boundaries, narrow navigation, and the independent hub lifetime. Its second
script, `scripts/loops-integration.mjs`, drives a review loop through the app with a fake
reviewer: the Review loop offer and `[loops] offer_review`, the setup card and its 600 ms
rule, the editor, the chip, the inspector's progress and controls, the loop's end, a second
run from `/review`, `/loop`, a stuck loop, the notifications, Settings › Loops, and the
keyboard and screen-reader text of each. Then an implementation loop: Send as
implementation loop with no saved settings raises the card holding the task, **Run without
a loop** sends it as a message, **Start and save** runs the plan, the implement turn, the
review rounds, the report and the commit (the script commits for the fake thread), its end
notifies, and the New thread form's switch starts one that ends `not committed`. Then loops
on a remote box: a dial-in `twr runner` of box `lab` on the same machine, with its own fake
Codex and Claude Code, which its agent probe finds. The setup card names the box when its
reviewer is not installed there, and comes back on the default account for a saved reviewer
account; the runner stopped during a review run makes the loop wait on its box, said once,
with its controls working and its wait shown again after a reload and a reconnect; started
again, the loop goes on from the same round and ends; an implementation loop on the box
commits; with the box away no start is offered. Then an ACP preset as the reviewer (the
hub tests' scripted ACP agent, beside a preset with no review mode and one whose mode is
`default`): the pickers of the setup card, Settings › Loops and a project's editor, the
card's warning, a loop on it to its end, and the preset removed from the config. With `TWR_LOOPS_SHOTS=<dir>` it also saves each
step in both themes, at the default size and the minimum width, with its accessibility tree.
`TWR_LOOPS_WINDOW=<width>x<height>` runs it in a window of that size, such as a CI runner's
`1024x674`, where the requests in the conversation scroll in their own area and the script
scrolls to their buttons as a person would. It runs against the debug build only. Debug
fixtures use the existing debug-only user token inside their own private `TWR_HOME`; release
builds ignore that token. No real account or existing workspace is used.

The hub the app starts for the main suite's second workspace (`side`) doesn't take that
token: like a real hub, it tells the app from an agent by its processes
([security-model.md](security-model.md#agents)). So the suite first asks its own hub,
without the token, whether it counts the suite's processes as yours, and stops with the hub's
reason when it doesn't. On macOS 27 that happens when the suite runs as a background job of
a shell that has already exited (`( npm run test:integration ) &`). Run it from a shell that
stays open until it ends.

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
Packaging fails without cargo-about (`cargo install --locked cargo-about --version 0.9.2 --features cli`; the binary needs the `cli` feature).
For a local review build, `TWR_NOTICES_WITHOUT_RUST=1` packages notices without the Rust
crates, marked incomplete at the top of the file; never distribute such a build.

The Arch package includes its own matching Electron runtime and native terminal module,
so a system Electron upgrade cannot change the app's ABI. The Arch package reuses
the portable producer's native terminal module without recompiling it. Its launcher uses the stable
`tools.twr.desktop` class for Hyprland rules. Chromium's sandbox stays enabled.

The AUR package `twr-desktop-bin` installs the same files from the release's
`twr-desktop-<version>-linux-x64.tar.gz`. `node scripts/aur.mjs <archive>` writes its
`PKGBUILD` and `.SRCINFO`. CI builds that package from the identical archive, checks
`.SRCINFO` against `makepkg --printsrcinfo`, and installs it over the release package; each
release pushes it to the AUR when the release environment has the AUR key
(`AUR_SSH_PRIVATE_KEY`, below). It is not on the AUR yet. Its dependencies are what namcap
finds the bundle's ELF files link (with `libgcc` and `libstdc++`, not the `gcc-libs` group),
plus `git`, which twr runs, `libsecret`, which Chromium loads for the keyring, and
`hicolor-icon-theme`, whose folder holds the icon. namcap's remaining reports are expected:
ELF files under `/opt` (the self-contained bundle), the setuid `chrome-sandbox`, and `git`
and `libsecret` as possibly unneeded. The packaged app leaves out node-pty's Windows-only
sources (`deps/`, `src/` and the `win32` prebuilds), which namcap would otherwise read as a
dependency on Python.

`release/omarchy-pkgs/` holds a recipe for the same archive in Omarchy's own package
repository (omacom/omarchy-pkgs), with the steps to submit it; it has not been submitted.

`install.sh --desktop` installs the release's Arch package, `.deb`, AppImage or Mac app, checked
against the signed `desktop-sha256sums.txt` ([install.md](install.md#desktop-app)). The Homebrew
tap `ignaval/homebrew-twr` has a formula for the CLI (`twr`, the release archives) and a cask for
the app (`twr`, the disk image), written for each release by `release/homebrew/render.py` from
the release's checksums; the cask needs a Developer ID signed, notarized app, because Homebrew
quarantines what a cask installs.

### Releases

Pushing a stable `v<version>` tag runs `release.yml`. That publishes the CLI release,
calls `desktop.yml` for the same commit (with all of its QA), then attaches the installers,
update metadata and `desktop-sha256sums.txt` to the release. Prerelease tags skip the
desktop. Keep the following secrets in the protected `release` environment (never repository-wide
secrets, which pull-request runs can read):

- `CSC_LINK`, `CSC_KEY_PASSWORD`, `APPLE_ID`, `APPLE_APP_SPECIFIC_PASSWORD`,
  `APPLE_TEAM_ID`: a Developer ID certificate (base64 `.p12`) and notarization
  credentials, required for a desktop release. The Mac app is signed, notarized and stapled,
  and `latest-mac.yml` is published so Macs update themselves. The caller explicitly passes
  these five secrets to `desktop.yml`; its Mac job reads their values from the protected
  environment after approval. Missing values fail before the build instead of producing
  an ad hoc release. Notarization usually takes minutes, but a
  new team's first submissions can wait hours at Apple, so the release's Mac job allows
  five.
- `AUR_SSH_PRIVATE_KEY` (optional): the key registered on the
  `twr-desktop-bin` AUR account. With it, the release pushes the new PKGBUILD to the AUR.
  Otherwise, push the `twr-desktop-aur` workflow artifact by hand.
- `HOMEBREW_TAP_DEPLOY_KEY` (optional): a deploy key with write access to `ignaval/homebrew-twr`. With
  it, the release pushes `Formula/twr.rb` and, when the Mac app is Developer ID signed (the
  release has `latest-mac.yml`), `Casks/twr.rb`, after checking the signatures of the
  checksums they come from. Otherwise they are in the `twr-homebrew` workflow artifact.

`TWR_RELEASE_SIGNING_KEY_P256`, which signs `sha256sums.txt.p256.sig`
([development.md](development.md)), also signs `desktop-sha256sums.txt.p256.sig`, the signature
`install.sh --desktop` checks on a Mac.

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

With the hardened runtime, electron-builder signs every Mach-O file in the bundle: the app with
`packaging/entitlements.mac.plist` (JIT and the microphone), and the Electron helpers, the
three engines in `Resources/bin` (`twr`, `twr-desktop-bridge`, `twr-mobile-gateway`) and
node-pty's `pty.node` and `spawn-helper` with electron-builder's inherited entitlements (JIT,
unsigned executable memory, no library validation), each with a secure timestamp. electron-builder
notarizes and staples the app before it makes the disk image and the update zip, so both hold
the stapled app. The packaged QA drives a copy of the app with the QA marker added: for a
Developer ID build that copy is signed again ad hoc, without the hardened runtime, because
macOS checks a notarized app's seal on its first launch even when it isn't quarantined. The
copy's update policy is therefore the ad hoc one; the release app's own signature is checked
by `codesign`, `spctl` and `stapler` in `desktop.yml`.


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
Only one refresh runs at a time. It lists the workspaces with `twr ws list --json --detail`,
which asks every hub at once for its threads and requests and waits up to 3 seconds for
each, so the refresh opens no connection of its own. If the listing takes longer than 5
seconds or fails, the refresh uses the last list that worked and reads each running
workspace over a short-lived read-only connection, as it does with a CLI too old for
`--detail`.

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

When a saved hub is offline, the app shows dated cached history with request buttons disabled. Plain messages can be saved in the outbox for their original workspace and destination, their attached files with them; a message escaped with `//` or `\!` (as Take back puts one back) is a plain message. Reopening the app preserves the original request ID; reconnecting retries that ID within 23 hours. An expired receipt or an unknown delivery outcome requires review in the conversation before a new message is sent. Removing an outbox entry never recalls a message already delivered to the hub. Shell commands, slash commands and permission answers require a live connection. Forgetting a hub removes its local encrypted history, drafts, outbox and bootstrap metadata.

See [remote setup and QR pairing](remote-clients.md) and [thread attachments](attachments.md) for the shared desktop/mobile flows.
