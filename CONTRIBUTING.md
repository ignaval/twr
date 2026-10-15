# Contributing to twr

Thanks for helping. This page says how to build and test twr and how to send a change. [docs/development.md](docs/development.md) is the maintainer's reference for the fuzzers, CI and releases.

To report a security vulnerability, don't open an issue: follow [SECURITY.md](SECURITY.md).

## Before you start

- **Small fixes** (a bug with a clear fix, a typo, a doc that disagrees with the code): open a pull request directly.
- **Larger changes** (a new feature, a new runtime, a change to the socket API, the event schema or a documented guarantee): open an issue first and describe what you want to change and why. twr has one maintainer, and some behaviour that looks like a bug is deliberate ([docs/security-model.md](docs/security-model.md) lists the guarantees that rest on it). Agreeing on the shape first saves you rework.

Development happens on `dev`, the default branch. `main` only moves for a release. Branch from `dev`, and open pull requests against `dev`.

## Build

You need a stable Rust toolchain, 1.89 or newer, and git. `rust-toolchain.toml` pins the version CI and the releases use, and rustup installs it for you in the repository.

```sh
cargo build -p twr-cli          # target/debug/twr
```

## The checks a change must pass

CI runs these on every pull request. Run them locally first:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
python3 docs/tools/check-links.py
shellcheck install.sh docs/tools/*.sh
```

CI also runs the tests on macOS, `cargo deny check` and actionlint ([docs/development.md](docs/development.md#ci)).

Notes on the tests:

- They need no agent CLI installed or signed in: they use a scripted fake runtime and recorded Claude Code, Codex and Grok Build sessions (`crates/twr-adapters/tests/fixtures/`).
- They run inside a twr thread too, as a [job](docs/usage.md#jobs) for example: `twr job run --slots 2 -- cargo test --workspace`. [docs/development.md](docs/development.md#tests-inside-a-twr-thread) explains the test token that makes this work and why only debug builds honour it.
- On macOS, `$TMPDIR` is under `/var`, a symlink to `/private/var`. git and twr report resolved paths while the tests compare them with the paths they created, so point `TMPDIR` at a directory without symlinks before you run the tests. CI does the same.
- macOS reads the process table with `ps`, and Linux reads `/proc`. To run the stale-process tests with the `ps` code on Linux: `RUSTFLAGS="--cfg twr_ps" cargo test -p twr-runner`.
- To run a few tests, keep the workspace selection and filter by name, `cargo test --workspace -- <name>`: `cargo test -p twr-hub` compiles a second copy of several crates ([why](docs/development.md#disk-usage)).

### Disk use

A full test build takes about 7 GB of `target/`, and cargo never removes old artifacts. `rm -rf target/debug/incremental` is always safe, and `cargo clean` frees all of it. If several twr worktrees build at once, [docs/development.md](docs/development.md#disk-usage) has measurements and what helps.

### The fake runtime

The `fake` runtime plays a prompt line by line, so you can drive threads into any state without a real agent: `!perm <tool>` asks for a permission, `!ask <question>|a|b` asks a question, `!write <file> [text]` writes a file, `!error`, `!crash` and others end a turn badly. It's only available with `TWR_ENABLE_FAKE=1`, because `!write` writes files. [docs/development.md](docs/development.md#the-fake-runtime) lists every directive.

### The fuzzers

Four seeded fuzz tests run a short version with every `cargo test`:

- `crates/twr-decisions/tests/command_fuzz.rs` checks how the hub reads the orchestrator's relayed answers and mode changes against an independent reference.
- `crates/twr-adapters/tests/translate_fuzz.rs` feeds recorded Claude Code, Codex and ACP sessions with damaged lines and odd commands through the translators, and checks that every permission card shows exactly the command that would run, or offers only `deny`.
- `crates/twr-hub/tests/frame_fuzz.rs` decodes valid, damaged and random runner protocol messages and WebSocket frames under the hub's size limits, and checks that nothing panics, memory stays bounded and valid messages round-trip.
- `crates/twr-hub/tests/runner_fuzz.rs` stresses the runner reconnect paths with dropped connections, crashes and failed writes.

If you change the command reading (`crates/twr-decisions/src/code/command.rs`), a translator's permission cards or the runner protocol, run the matching long version too, as [docs/development.md](docs/development.md#the-command-fuzz) shows. A failure prints the seed that reproduces it.

## Keep the docs in step

- [SKILL.md](SKILL.md) is the reference for the socket API, events and MCP tools, and agents read it. A change to a method, an event or a tool changes SKILL.md in the same pull request.
- A change to behaviour a page in `docs/` describes changes that page. twr compiles the docs into its binary for the orchestrator's `read_docs` tool, so a stale page gives wrong answers. A new page in `docs/` goes into `crates/twr-docs/src/lib.rs` (a test checks that).
- A change to what twr guarantees, or to what it leaves out, changes [docs/security-model.md](docs/security-model.md).
- User-visible changes get a line in [CHANGELOG.md](CHANGELOG.md) under `Unreleased`.

The docs are plain and precise: second person, short paragraphs, every claim true of the code.

## Sign off your commits

Every commit must carry a `Signed-off-by` line with your name and email, which `git commit -s` adds:

```
Signed-off-by: Your Name <you@example.com>
```

It certifies the [Developer Certificate of Origin](https://developercertificate.org): that you wrote the change, or otherwise have the right to submit it under the project's open source licence, and that you understand the contribution and your sign-off are public and kept with the project. To sign off commits you already made, `git rebase --signoff dev`.

## Licence of contributions

twr is licensed under Apache-2.0. As section 5 of the [licence](LICENSE) says, a contribution you submit is licensed under the same terms, with no additional terms or conditions, unless you state otherwise. There is no separate contributor agreement.

## AI-assisted contributions

twr is built with coding agents, and contributions made with them are welcome. The person who submits a pull request is responsible for all of it: you must have reviewed and understood every line, be able to explain why it's right, and have run the checks above yourself. Review questions go to you, not to the agent.

## Pull requests

- One topic per pull request. Keep unrelated cleanups out.
- Add tests for what you change: a bug fix comes with a test that fails without it.
- Describe what changed and why, link the issue, and list the checks you ran; the pull request template asks for these.
- Expect review comments, and push fixes as new commits.

Everyone taking part follows the [code of conduct](CODE_OF_CONDUCT.md).
