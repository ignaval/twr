# Application performance audit

Measured on 2026-11-06 with disposable real Rust hubs and deterministic fake agents.
These are single-run diagnostics, not controlled release benchmarks. Physical mobile
battery use, cellular latency and production provider latency are outside this fixture.

## Changes and evidence

The companion now reuses a bounded pool of eight authenticated hub RPC connections.
A read can reconnect, and a mutation can retry only when the protocol proves it was
not executed. An uncertain mutation is never blindly retried. Mobile reconnect preserves
the original mutation ID and the gateway's durable receipt rather than creating another
operation. Bounded, separate terminal/control event lanes prevent terminal output from
forcing a complete state reconnect.

Large remote replies are split at UTF-8 boundaries into 32 KiB parts, with a bounded
assembler and ordered control/terminal lanes. Heartbeats share that scheduler without
waiting for a whole large reply. The foreground watchdog measures incoming frame progress,
so a healthy slow transfer does not fail a fixed pong deadline. Native Android QA completes
a 4 MiB transcript over a throttled 64 KiB/s link across background/foreground transitions
without reconnecting, then detects and recovers a silent downstream connection loss.

Executable digest results are reused only while file identity, size, mode and modification
and change timestamps match. Verification still uses bounded reads when a binary changes.
Cleanup keeps live engines and versions pinned by stopped runner or speech service units.

Markdown blocks are memoized and the transcript is virtualized. A thousand tool calls
leave only two or three transcript blocks mounted in the measured views. Executable
verification reads fixed 1 MiB chunks instead of retaining whole binaries. The Mac debug
startup probe's main-process RSS fell from approximately 430 to 192 MiB after that change;
this is a startup allocation sample with large debug executables, not a production-memory
or leak claim. Summed process RSS double-counts shared mappings and is not physical usage.

The same Mac development harness at 420 × 480 measured 881 → 293 ms for 1,000 tool
calls to become visible, and 112 → 1 ms median hub RPC. The current packaged app is
measured separately. The benchmark's input-to-paint metric measures the input event to
the next animation frame; automation command completion includes unrelated driver/IME
work and is recorded separately in raw JSON.

| Current environment | 1,000 tool calls visible | RPC p95 | Input → paint p95 | Frame p95 |
| --- | ---: | ---: | ---: | ---: |
| Apple Silicon packaged app | 190 ms | 2 ms | 16 ms | 18 ms |
| Android ARM64 phone emulator | 410 ms | 14 ms | 15 ms | 17 ms |
| Android ARM64 Pixel Fold emulator | 457 ms | 22 ms | 15 ms | 33 ms |
| iPhone Simulator, iOS 18.7, Xcode 27.1 CI | 3,754 ms | 75 ms | 33 ms | 22 ms |
| Arch x86_64 QEMU, software GPU, Hyprland at 125% | 1,931 ms | 46 ms | 128 ms | 40 ms |

The Linux VM validates behavior and catches layout regressions; its timing does not
predict performance on a physical Omarchy machine. The earlier Linux measurement used
a different compositor/GPU setup, so no controlled before/after claim is made for Linux.
Android views were measured with the software keyboard visible. The iOS diagnostic runs
on a different CI host and cannot be used as a controlled comparison with the local Android
emulators or packaged desktop app. Current packaged Mac
cold-fixture launch to connected hub was 2.47 seconds. It includes creating and starting
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
`assistantCompletion: true`; the report excludes earlier iOS samples automatically.

## Reproduction

Build the Rust engine first and run from `desktop`:

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
