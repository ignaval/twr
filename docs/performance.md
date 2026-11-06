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

Markdown blocks are memoized and the transcript is virtualized. A thousand tool events
leave only two or three transcript blocks mounted in the measured views. Executable
verification reads fixed 1 MiB chunks instead of retaining whole binaries. The Mac debug
startup probe's main-process RSS fell from approximately 430 to 192 MiB after that change;
this is a startup allocation sample with large debug executables, not a production-memory
or leak claim. Summed process RSS double-counts shared mappings and is not physical usage.

The same Mac development harness at 420 × 480 measured 881 → 302 ms for 1,000 tool
events to become visible, and 112 → 1 ms median hub RPC. The current packaged app is
measured separately. The benchmark's input-to-paint metric measures the input event to
the next animation frame; automation command completion includes unrelated driver/IME
work and is recorded separately in raw JSON.

| Current environment | 1,000 events visible | RPC p95 | Input → paint p95 | Frame p95 |
| --- | ---: | ---: | ---: | ---: |
| Apple Silicon packaged app | 197 ms | 4 ms | 17 ms | 19 ms |
| Android ARM64 phone emulator | 365 ms | 18 ms | 24 ms | 17 ms |
| Android ARM64 Pixel Fold emulator | 562 ms | 28 ms | 15 ms | 33 ms |
| Arch x86_64 QEMU, software GPU, Hyprland at 125% | 1,868 ms | 47 ms | 103 ms | 67 ms |

The Linux VM validates behavior and catches layout regressions; its timing does not
predict performance on a physical Omarchy machine. The earlier Linux measurement used
a different compositor/GPU setup, so no controlled before/after claim is made for Linux.
Mobile views were measured with the software keyboard visible. Current packaged Mac
cold-fixture launch to connected hub was 2.47 seconds. It includes creating and starting
a new hub, rather than attaching to a previously running one.

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
report are written under ignored `desktop/test-results`. Preserve these with each release
QA run. iOS results must only be added after native Simulator tests have completed.
