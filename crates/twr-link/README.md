# Linked runners

`docs/runner-links.md` documents the user workflow and trust boundary. This crate owns discovery, machine trust, the independent service and per-workspace provisioning. `twr-source` provides network-free source capture and bounded Git bundles; the existing runner owns agent execution and spooling.

The implementation requires Linux/macOS, Rust 1.98 or later, and the checked-in lockfile. It deliberately treats paired hubs as trusted owners of the runner's OS account. It does not sandbox hostile hubs.

## Review and verification

Three requested Claude Opus 5.5 reviews at high effort found lifecycle, recovery, control-cache and desktop integration defects. The fixes include:

- One window → registry → endpoint lock order, with a deadlock regression.
- Reuse of an existing machine grant when adding an alias; explicit replacement for an active grant, covered over private QUIC.
- Idempotent forgetting, durable offline retirement, receipt comparisons without prefix timing and permanent per-grant fences.
- Cached concurrent management/capacity controls without reconnecting, machine-wide retirement rate limits with releasable claims, full-capacity checks, generation-specific cancellation and a short wait-ticket expiry.
- Nonblocking coordinated power/admission probes across all 64 possible slots, and a lease through native artifact publication and compiler descendants. Crash cleanup and revocation target the recorded grant epoch and runtime/build generation.
- Native scratch cleanup and removal of changed source materializations before retry, explicit cache reclamation, immutable hub snapshots and asynchronous dormant-service bootstrap.
- Duplicate-spawn detection before capacity admission and definitive rejection of unreserved uploads.
- Temporary build-time source objects and refs, archive support, raw-byte/ignore preservation and a lightweight build dependency.
- An explicit revocation confirmation, self identification and pairing recovery in the clients. Desktop hosting controls always reach the local service while viewing a remote hub; exact-session cancellation still works after the old view is fenced.
- A private publisher in fixtures, publication-failure cleanup, and an unhealthy-candidate rollback test using private Unix sockets and a mock launcher.

The native build quota scanner tolerates files disappearing during compiler scratch cleanup. Build failure output is bounded and returned to the owner. Executable uploads are bounded at 1 GiB; source/Git bundles are bounded separately at 128 MiB.

Reproduce the checks from the repository root:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked --no-fail-fast
cargo test --workspace --test runner_link -- --ignored
cargo deny check advisories bans licenses sources
npm --prefix clients run build
npm --prefix clients test
npm --prefix clients run catalog:check
```

The explicit ignored conformance test compiles the actual hub's embedded source in an isolated runner tree. The ordinary unit suite also compiles a small recorded source archive when the PATH compiler is already installed in rustup; it never installs a toolchain. Pairing, recovery, replacement, Git round trips, worker reconnect and revocation use loopback QUIC/private sockets. The fixtures do not publish pairing records or probe real agent CLIs. Systemd/launchd integration needs the platform CI and release smoke checks; local rollback tests inject a launcher rather than touching the user's service manager.
