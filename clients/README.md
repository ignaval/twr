# twr clients

Desktop and mobile are clients of the same Rust hub. They share one React UI and one npm dependency/build setup.

- `desktop/`: Electron host, isolated preload, and desktop renderer entry.
- `mobile/`: Capacitor host, native Android/iOS projects, remote transport, and mobile layout.
- `shared/`: shared React screens, styles, typed host interface, and presentation helpers.
- `scripts/`, `tests/`: shared build and verification tooling.

The authoritative application projection and gateway live in [`crates/twr-app`](../crates/twr-app). The hub continues running when a client closes.

From the repository root:

```sh
npm --prefix clients ci
npm --prefix clients run build
npm --prefix clients test
npm --prefix clients run mobile:build
```

See [desktop development](../docs/desktop.md), [mobile development](../docs/mobile.md), and [performance measurements](../docs/performance.md). Generated installers, native QA captures, and measurements are under ignored `release/` and `test-results/`.
