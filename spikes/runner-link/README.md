# Linked runner validation

These are the isolated, non-shipping fixtures from the first stage of the
linked-runner design. They have their own workspace and lockfile, install no
service, launch no agent and read no real pairing state. The apps baseline has
merged; production now lives in `crates/twr-link`, `crates/twr-source` and the
CLI/hub/runner/client integrations described in [runner-links.md](../../docs/runner-links.md).
The production handshake uses exporter-bound ring HMAC confirmations rather than
the unaudited SPAKE2 experiment retained here.

The running fixtures use explicit addresses, local HTTP servers, a loopback Mainline
DHT testnet, and an optional network-disabled Docker container. They never publish
to n0 or the public DHT. Fetching crates, the base image and Ubuntu build packages
does use public download services. Test identities/codes are disposable.

## Run

From the repository root, using the pinned Rust toolchain:

```sh
cargo fmt --manifest-path spikes/runner-link/Cargo.toml -- --check
cargo clippy --manifest-path spikes/runner-link/Cargo.toml --all-targets --features local-relay --locked -- -D warnings
cargo test --manifest-path spikes/runner-link/Cargo.toml --features local-relay --locked
python3 -m unittest discover -s spikes/runner-link -p 'test_*.py' -v
cargo run --manifest-path spikes/runner-link/Cargo.toml --release --bin transport_probe --locked
```

The ordinary Rust tests need no Docker, root, agent login or public infrastructure.
The extra `local-relay` feature builds the NAT fixture executable; local relay tests
also enable the server as a dev dependency. `transport_probe` echoes a bounded
message between loopback endpoints and prints non-secret JSON evidence.

These are manual, point-in-time checks. Root CI's workspace commands do not run
this separate package. Production has its own workspace regression tests; there is no automatic
regression protection for this historical spike.

## What the fixtures establish

- QUIC authenticates the configured endpoint identities. An unpaired endpoint is
  refused before worker data is read. Initial application reads are capped at
  8 KiB and five seconds.
- Direct connections and a relay-only address both work with public lookup
  disabled. A reachable direct session continues after the local relay stops.
- A separate bulk stream carries 16 MiB + 123 bytes in chunks no larger than
  64 KiB, verifies its digest, and leaves the control stream usable. This is a
  transport experiment; production per-transfer sealed channels and Git
  transaction authorization are not implemented here.
- A code-derived key signs a pkarr record within the signed-packet limit, containing
  the relay URL needed to dial without a second DNS lookup. Local HTTP publication,
  tombstones, signed-packet tampering, expiry and cold-cache DHT resolution work.
  The DHT fixture has only loopback bootstrap nodes and no public relay defaults.
- The SPAKE2 experiment binds endpoint identities, roles, session, expiry and an
  identity-payload hash, the actual code-derived locator and initiation direction,
  with directional mutual confirmation. Tests reject wrong
  passwords, reflected/replayed messages and a proof for another session. Admission
  failure does not spend a code's three attempts; consumed/expired sessions fail
  locally regardless of discovery tombstones.
- Git fixtures capture effective staged/unstaged/untracked source without changing
  the user's index, HEAD or stash. Repository filters are disabled for both `add`
  and `write-tree`, while global/environment-configured ignore files are preserved.
  Repositories with skip-worktree or assume-unchanged entries are refused before
  capture, including sparse profiles, to avoid shipping hidden local edits.
  Named-ref bundles tolerate unknown negative tips and support a
  credential-free, no-origin return into a separate hub ref, preserving its checkout.

## Two NATs

Build the Linux fixture on a glibc compatible with Ubuntu 24.04 (the pinned test
image uses glibc 2.39). Then:

```sh
cargo build --manifest-path spikes/runner-link/Cargo.toml --features local-relay --bin nat_fixture --locked
docker build -f spikes/runner-link/Dockerfile.netns -t twr-runner-link-netns:spike spikes/runner-link
docker run --rm --network none --cap-add NET_ADMIN --cap-add SYS_ADMIN --security-opt apparmor=unconfined twr-runner-link-netns:spike
```

The container creates two endpoint namespaces, two independent NAT/router
namespaces and a private WAN bridge/relay. Endpoint LANs cannot route directly to
one another. The test transfers the bulk payload and verifies the original control
stream remains alive. The container has no external network, host mounts, published
ports or host-network mode; setup refuses an existing non-loopback interface.
This is still a **privileged container for trusted code only**, not a sandbox:
SYS_ADMIN and the AppArmor override grant broad access to the shared kernel.
Setup makes only `/proc/sys/net` temporarily writable for forwarding writes in the
new router namespaces, then restores it read-only before workers run. All relay
and endpoint workers run as nobody with every capability dropped and no-new-privileges
enabled; each binary verifies those conditions. Do not run the Python harness as
root on the host or use it with untrusted code. Its namespaces/children are torn down,
and `--rm` removes the container.

This relay has no QUIC address-discovery listener. The test requires a relay-selected
path, measured before its final ACK allows the peer to close. This does **not**
establish UDP hole punching between two Internet NATs,
LAN/tailnet interface enumeration, or performance through n0's public relay.

## Measured gates and limits

On Ubuntu 24.04 x86-64 with Rust 1.98.1, local fixtures pass. The two-NAT fixture
transferred 16,777,339 bytes with the direct LAN route unavailable, in approximately
0.48 seconds through the private relay after the review hardening. That is a fixture result, not an Internet
throughput estimate. The standalone release loopback executable measured 14,945,216
bytes (14.9 MB), with a first clean release build of 6m 07s; this is not an incremental
size measurement of twr. This spike is tested with the pinned Rust 1.98.1 toolchain
and declares 1.98; it does not claim a validated minimum at iroh's advertised 1.91
floor. Some dev/other-platform dependencies in the lockfile require 1.95. The
shipping workspace still advertises 1.89 and is unchanged; adopting iroh needs an
explicit compiler-floor review.

The completed validation is 18 Rust integration tests, 11 Python fixture/guard tests,
strict feature-inclusive clippy, formatting, repository doc links, the two-NAT Docker
run and the release loopback smoke probe. The explicit endpoint-rejection case also
passed five fresh-key runs. This does not turn the failing adoption gate below green.

## Implementation review

Claude Opus 5.5 at high reviewed the initial implementation and independently reran
its Rust/Python suites. Its four must-fix and five should-fix findings are addressed:
preserve global/environment/XDG ignores; refuse hidden index flags; require an
actual code-1 unpaired-endpoint rejection; restrict writable sysctls and drop worker
privileges; enforce relay-path evidence while the session is live and wait for relay
readiness; derive the locator inside the PAKE transcript and bind initiation roles;
exercise replayed PAKE messages as well as confirmations; record the release size;
and explicitly document the absence of automatic CI for this standalone package.
Regression tests and the hardened NAT run validate the fixes. This is not a second
independent review of the final diff, crypto-library acceptance, or a production audit.

## Dependency and production gates

The dependency assessment deliberately uses the existing policy without an ignore:

```sh
cargo deny --manifest-path spikes/runner-link/Cargo.toml --config deny.toml check
```

This currently **fails the adoption gate**. Licence and registry-source checks pass,
but `iroh → netwatch → netlink-packet-core → paste 1.0.15` triggers
[RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436.html), an
unmaintained-package advisory (not a claim of a known exploitable vulnerability).
The new HTTP dependency chains also need an explicit, narrowly scoped policy design
after the apps baseline merges. Both ring and AWS-LC occur in the resolved dependency
graph even with iroh's default features disabled and `tls-ring` selected. This
package does not change or waive any shipping policy.

SPAKE2 0.4.0 is a **candidate for protocol experiments only**. Its upstream documents
that it has not received an independent audit. These tests neither approve that
library for production nor prove the full pairing protocol secure. No CPace crate
or custom curve arithmetic is used. Durable two-peer trust commit/recovery,
certificates, service lifecycle, revocation, worker API conformance, path confinement
and production resource quotas remain later work behind their gates.

`source_probe.py` is restricted to disposable fixtures. Its Git subprocess groups
are killed on timeout; transports/credentials and repository filters are disabled.
It still lacks production descriptor-relative path confinement, protection against
concurrent hostile config writers, incremental-transfer recovery and unpacked-object
budgets. Its compressed bundle cap is checked after local bundle creation, so it
does not bound temporary source disk use. It captures non-ignored untracked source;
that can include secrets that should have been ignored. It does not package ignored
inputs, external dependencies, submodules, LFS content, or a Rust toolchain/build
manifest. A concurrently swapped incoming bundle can also race its verification/ref
publication; production needs a private incoming file and transactional ref publication.
Do not substitute it for a shipping synchronization API.

Mac keychain/login/sleep checks, real-machine installs and public-infrastructure
tests have not run. Rootless namespace creation is unavailable on the development
host; the successful NAT test uses the isolated Docker path above.
