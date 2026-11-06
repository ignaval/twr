# Application performance audit

Measured on 2026-11-06 with disposable real Rust hubs and deterministic fake agents.
These are dated single-run diagnostics, not controlled release benchmarks. The table
is a recorded snapshot; later raw runs are preserved separately with their own source
revisions. Physical mobile battery use, cellular latency and production provider
latency are outside this fixture.

## Changes and evidence

The companion now reuses a bounded pool of eight authenticated hub RPC connections.
A read can reconnect, and a mutation can retry only when the protocol proves it was
not executed. An uncertain mutation is never blindly retried. Mobile reconnect preserves
the original mutation ID and the gateway's durable receipt rather than creating another
operation. Bounded, separate terminal/control event lanes prevent terminal output from
forcing a complete state reconnect.

The gateway retains live conversation bursts within a per-device limit of 4,096
events and normally 4 MiB of encoded JSON. A single valid event up to the existing
16 MiB socket limit can be retained alone. Slow sockets that fall behind the
retained history receive an explicit resync. Events are encoded once and readers
share their immutable bytes. The byte and event-count limits bound retained
payloads and bookkeeping separately.
History is released when the last socket disconnects. Terminal output has its own lane.

Large remote replies are split at UTF-8 boundaries into 32 KiB parts, with a bounded
assembler and ordered control/terminal lanes. Heartbeats share that scheduler without
waiting for a whole large reply. The foreground watchdog measures incoming frame progress,
so a healthy slow transfer does not fail a fixed pong deadline. Native Android QA completes
a 4 MiB transcript over a throttled 64 KiB/s link across background/foreground transitions
without reconnecting, then detects and recovers a silent downstream connection loss.

Executable digest results are reused only while file identity, size, mode and modification
and change timestamps match. Verification still uses bounded reads when a binary changes.
Cleanup keeps live engines and versions pinned by stopped runner or speech service units.

Collapsed long user/tool messages now pass only a bounded preview (12 lines or about
1,600 characters) to Markdown. Expansion and copying retain the exact original content.
A regression fixture with 4,000 checklist entries mounts only 12 entries while collapsed;
the packaged Mac suite verifies full expansion and native clipboard content.

Markdown blocks are memoized and the transcript is virtualized. A thousand tool calls
leave only two or three transcript blocks mounted in the measured views. Executable
verification reads fixed 1 MiB chunks instead of retaining whole binaries. The Mac debug
startup probe's main-process RSS fell from approximately 430 to 192 MiB after that change;
this is a startup allocation sample with large debug executables, not a production-memory
or leak claim. Summed process RSS double-counts shared mappings and is not physical usage.

The same Mac development harness at 420 × 480 measured 881 → 292 ms for 1,000 tool
calls to become visible, and 112 → 1 ms median hub RPC. The current packaged app is
measured separately. The benchmark's input-to-frame diagnostic measures the input event to
the next requestAnimationFrame callback, not actual pixel paint or INP. Automation
command completion includes unrelated driver/IME work and is recorded separately in
raw JSON. The Mac harness retains raw input/frame samples, focus and visibility state
so a single slow callback can be investigated rather than attributed to rendering.

| Measured environment | 1,000 tool calls visible | RPC p95 | Input → frame p95 | Frame p95 | Source / sample date |
| --- | ---: | ---: | ---: | ---: | --- |
| Apple M6, packaged macOS app | 188 ms | 2 ms | 15 ms | 19 ms | not recorded · 2026-11-06 15:23 UTC |
| Android ARM64 phone emulator | 392 ms | 12 ms | 13 ms | 17 ms | a860f569580e · 2026-11-06 16:28 UTC |
| Android ARM64 Pixel Fold emulator | 518 ms | 15 ms | 14 ms | 50 ms | a860f569580e · 2026-11-06 16:34 UTC |
| iPhone Simulator, Xcode 27.1 CI | 3,339 ms | 146 ms | 36 ms | 23 ms | a860f569580e · 2026-11-06 16:37 UTC |
| iPhone Duo Simulator, Xcode 27.1 CI | 3,694 ms | 15 ms | 51 ms | 23 ms | 0d0aa7ec169b · 2026-11-06 17:12 UTC |
| iPad Simulator, Xcode 27.1 CI | 5,137 ms | 17 ms | 23 ms | 18 ms | a860f569580e · 2026-11-06 16:41 UTC |
| Arch x86_64 QEMU, software GPU, Hyprland at 125% | 1,395 ms | 74 ms | 27 ms | 68 ms | not recorded · 2026-11-06 15:19 UTC |
| Ubuntu 22.04 userspace, x86_64 QEMU on Arch kernel | 2,243 ms | 67 ms | 22 ms | 33 ms | not recorded · 2026-11-06 16:29 UTC |

The Linux VM validates behavior and catches layout regressions; its timing does not
predict performance on a physical Omarchy machine. The earlier Linux measurement used
a different compositor/GPU setup, so no controlled before/after claim is made for Linux.
Android views were measured with the software keyboard visible. The iOS diagnostic runs
on a different CI host and cannot be used as a controlled comparison with the local Android
emulators or packaged desktop app. The recorded packaged Mac
cold-fixture launch to connected hub was 2.54 seconds. It includes creating and starting
a new hub, rather than attaching to a previously running one.

macOS ancestor argument reads now use two batched `ps` listings for a whole chain,
reducing repeated subprocess launches during hub authentication. The existing program-name
parsing and identity rules are shared with the earlier single-process reader.

The mobile custom-scheme SHA-256 fallback now yields between 1 MiB chunks. A Node-on-Mac
25 MiB diagnostic previously blocked the event loop for 87 ms; the cooperative version
took 120 ms overall and allowed 24 timer callbacks, with an 11 ms maximum event-loop gap.
This trades a little checksum throughput for responsiveness; it is not a native-phone
benchmark. A regression test checks both the digest against native SHA-256 and timer progress.

Earlier iOS stream timing samples were discarded because the test could match the user
prompt. The corrected harness waits for the rendered assistant reply and records
`assistantCompletion: true`. The report also requires `qaVerified: true`, set only
after the complete native integration test succeeds; failed or partial runs are excluded.

## Reproduction

Build the Rust engine first and run from `clients`:

```sh
TWR_PERF=1 npm run test:integration
TWR_PERF=1 npm run test:qa:mac
TWR_PERF=1 node mobile/scripts/android-qa.mjs
TWR_ANDROID_SERIAL=emulator-5556 TWR_PERF=1 node mobile/scripts/android-qa.mjs
node mobile/scripts/ios-qa.mjs
node scripts/performance-report.mjs
```

Native iOS QA requires Xcode 27.1 and installed phone/Duo runtimes. The Mobile CI workflow
uses the Xcode 27 runner. Raw samples, viewport/device details, screenshots and the HTML
report are written under ignored `clients/test-results`. Preserve these with each release
QA run. iOS results must only be added after native Simulator tests have completed.

A packaged Mac input-to-frame outlier of 172 ms did not recur in a quiet run with
the emulators and Linux VM stopped: p95 was 15 ms, all 12 samples were focused and
visible, maximum frame interval was 19 ms and no long tasks were observed. This does
not establish the cause of the earlier outlier. Both raw runs are preserved.

The table retains earlier iOS diagnostics. Their legacy `finalEventToRenderMs` field
measured the first matching text event to a later DOM polling observation; it did not
measure final-frame-to-paint latency. Some early live recorders started before transcript
subscription completed, so fewer than the expected 2,002 events were captured. Their
server/receive spans cover only that observed subset and cannot describe the full stream.
The overall workload-to-observed-assistant-reply measurement remains separate.

The corrected iOS probe waits for transcript subscription readiness, requires the full
workload event counts and an actual final text event, and retains per-event timing.
`finalEventToDomObservationMs` includes UI batching and polling; it is not paint timing.
The workload includes a debug hub and native bridge on the CI host, so end-to-end time
must not be attributed entirely to WebKit rendering. A small local TCP probe did not
demonstrate a latency benefit from disabling Nagle; no speculative transport or batching
change was made. Physical-device and release-server profiling remain outside these
Simulator diagnostics.
