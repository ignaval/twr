# Conversation read-aloud toggle review

2026-11-17, based on `3638f6d`, branch `twr/@twr/read-aloud-toggle`.

The conversation toolbar now controls the existing desktop speaker's persisted live
switch for the current workspace. It appears in orchestrator conversations and deep dives,
with on/off icons, the toolbar's pressed styling, an accessible name and pressed state,
and an explicit desktop drag exclusion. An unknown setting is shown as mixed, with a
connection/error tooltip and a speaker reconnect action. Disconnected controls are disabled.

Opening the conversation attaches the existing `twr speak --control stdio` process without
sending a live control or requesting microphone access. The button reads `speech.status`
from the current workspace and sends `speech.control` with the local device id, including
when the desktop process stands by for the local TUI. It never broadcasts to other devices.
Reported state, rather than command delivery, confirms a toggle. Speaker events and polling
follow external changes; workspace switches and disconnects invalidate pending work.

Disabling uses `live: false`: the existing player silences current automatic playback and
drops queued automatic messages/announcements for that workspace. Explicit `/read` requests
and other workspaces retain their existing behavior. Paused playback stays independent.
Deep dives still control orchestrator auto-reading; worker messages remain on-demand only.
Remote hub views and phones omit the button because their bridge does not expose this
computer's speaker controls. No speech feed, filtering, persistence or microphone code changes.

## Voice-input-draft integration

Inspected the active uncommitted work in `twr/@twr/voice-input-draft`, thread
`01M7NSHDE5G17G39ZN82497GYG`. Keep its ordinary dictation in the editable composer, its
speech provenance and offline handling, hands-free auto-send, cancellation fences and
removal of the idle Voice ready presentation. This change does not edit `Composer.tsx`,
`VoiceControl.tsx`, `voice.ts`, `offline.ts`, or App's voice submission paths.

Hands-free still intentionally enables live reading through the shared speaker. The new
toolbar follows that reported change, and switching live off does not stop recognition.
Ordinary dictation in voice-input-draft does not turn live on. Avoid reconnecting that
path to the toolbar toggle or introducing a separate reading preference.

A temporary directory combined this implementation with an uncommitted voice-input-draft
snapshot; neither branch was merged or modified for that check. Its source patches applied
without conflicts, and TypeScript plus the read-aloud, voice, composer and offline suites
passed (130 tests). The snapshot patch SHA-256 was
`91d1155f08176e765c3d788de95583b5fe86d09a88a779fef92b0b86b380d492`.
Later changes on that active branch still need their normal integration review.

The hub refused a coordination prompt to that thread: agent-originated sends are not
authorized. These notes provide the handoff; no message was delivered.

## Verification

- `cd clients && npx tsc --noEmit`: passed.
- `npx vitest run --configLoader runner tests/read-aloud.test.ts`: 12 tests passed,
  covering persistence, local TUI ownership, workspace/device isolation, external changes,
  delayed/missing acknowledgement, errors, reconnects and late asynchronous results.
- Existing voice, composer, contracts and accessibility suites: 119 tests passed.
- `CARGO_TARGET_DIR=/tmp/twr-read-aloud-target cargo test -p twr-speech`: 67 passed,
  including live-off cancellation/persistence, request retention and workspace isolation.
- `npx vite build --configLoader runner --outDir /tmp/twr-read-aloud-build`: passed
  with the existing large-chunk warning. Changed TS/TSX files pass Prettier.
- Real Chromium against the shared App with a disposable browser host: on/off state,
  keyboard Space activation, computed `no-drag`, deep-dive availability, exact device-targeted
  RPCs, external state changes, disconnect/reconnect and an 800px viewport passed without
  page errors. Local harness: `/tmp/twr-read-aloud-browser.mjs`; screenshots:
  `/tmp/twr-read-aloud-desktop.png` and `/tmp/twr-read-aloud-narrow.png`.

The browser host was a fixture; no audible hardware playback, installed application,
real provider account or production workspace was exercised. No merge, package or install
was performed.
