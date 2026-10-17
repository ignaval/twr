# Development

This page is the maintainer's reference: how the tests, fuzzers, CI and releases work, and how the screenshots and the site are made. [CONTRIBUTING.md](../CONTRIBUTING.md) covers building twr, the checks a change must pass and how to send one.

## Tests inside a twr thread

The tests run from inside a twr thread too. Their hubs and clients descend from that thread, so they would count as its agent; the CLI tests give their throwaway hubs (under a temporary `TWR_HOME`) and their own clients one token instead, in the file `TWR_TEST_USER_TOKEN_FILE` names, and a debug build of twr, which is what the tests run, takes a client that presents it for the user when its hub was started with it under `TWR_HOME`. The hub trusts the file only when it's a regular file (not a link) owned by the hub's user with no group or other access, and only debug-build clients send it. A hub without `TWR_HOME`, or started without the variable, ignores the token, and so does every release build, the one you install: anyone can write a token file, so it never makes an agent the user of a real hub. A hub the TUI starts on its own never gets the variable. CI checks that a release build ignores the token ([CI](#ci)). From a thread, a full test run is a good [job](usage.md#jobs): `twr job run --slots 2 -- cargo test --workspace`.

The tests use a scripted fake adapter plus recorded Claude Code, Codex and Grok Build sessions, so they don't need any agent CLI installed or signed in. The recordings live in `crates/twr-adapters/tests/fixtures/`, one JSON line per message in either direction.

## Disk usage

Each twr worktree builds into its own `target/`, so several threads working on twr at once can fill a disk. Measured on Linux with Rust 1.98, a full `cargo test --workspace --no-run` followed by one edit-and-rebuild of `twr-hub` (what an agent does all day):

| Dev profile | `target/` | test and bin executables | incremental cache | full build | rebuild after an edit |
|---|---|---|---|---|---|
| full debuginfo (Cargo's default, before) | 15 GB | 9.3 GB | 4.4 GB | 429 s | 58 s |
| `debug = "line-tables-only"` (now) | 7.2 GB | 3.4 GB | 2.9 GB | 434 s | 55 s |
| the same, dependencies without debuginfo | 6.4 GB | 2.9 GB | 2.9 GB | 510 s | 100 s |
| the same, `incremental = false` | 4.3 GB | 3.4 GB | none | 642 s | 171 s |

(Times on a shared 4-core machine under load: compare them, don't quote them.) What takes the space is the 48 executables of a test build, each linked with all of its crates: 39 integration test files, 22 of them in `twr-hub`, link the whole hub into ~250 MB each, three quarters of which was debuginfo. The workspace `Cargo.toml` therefore keeps only line tables in dev and test builds: panics and `RUST_BACKTRACE` still show files and lines. `CARGO_PROFILE_DEV_DEBUG=full` brings full debuginfo back for one build. A `target/` also grows over time, because cargo never removes old artifacts: one worktree reached 19 GB with stale test binaries and incremental sessions. `rm -rf target/debug/incremental` is always safe; `cargo clean` frees all of it.

Building part of the workspace adds copies too. Cargo turns on a dependency's features for what it builds, dev-dependencies included (`twr-hub`'s tests use `rmcp` with `client`), so `cargo test -p twr-hub`, `--test runners` or `--lib` compile `rmcp`, `twr-mcp` and everything above them again, apart from the full build's artifacts. To run a few tests, keep the workspace selection and filter by name: `cargo test --workspace -- <name>`.

twr helps from its side:

- Deleting a thread deletes its build output (`target/` and the rest of `[threads] park_disposable`) instead of keeping it in the trash for a week ([worktrees.md](worktrees.md#delete)), on Linux and macOS. Parking a clean thread already removes it everywhere.
- `[threads] env` and `[projects.<name>] env` set build variables for the agents' processes ([configuration.md](configuration.md)), e.g. `env = { CARGO_INCREMENTAL = "0" }` on a machine short of disk, at the price of the slower rebuilds above.

What doesn't pay off for twr's own threads:

- One `CARGO_TARGET_DIR` for every worktree of the project. Dependencies would be built once, but cargo locks the target directory for each build, so parallel threads wait for each other, and branches with different sources give their workspace crates the same artifact names, so each thread's build overwrites the others' and the next build of the others recompiles them. A thread's `cargo test` could even run binaries another thread just rebuilt. It suits one thread at a time, not parallel work.
- sccache (`RUSTC_WRAPPER=sccache`) shares compiled dependencies between worktrees, but only saves build time: each `target/` still gets its own copies, the cache adds its own, and it doesn't cache incrementally built workspace crates or linked executables, which are most of the size.
- Merging the integration test files of each crate into one test binary (`tests/it/main.rs` with a module per file) would save most of the executables' size and link time, about 1.5 GB per worktree for `twr-hub` alone with line tables. But it means moving every test file, which conflicts with every branch in flight, and tests that run in one process share its state. It's worth doing once few branches are open.

## The fake runtime

The `fake` runtime plays a prompt line by line. Lines that start with a directive do something; other lines become the agent's reply. It's only available with `TWR_ENABLE_FAKE=1`, because `!write` writes files.

| Directive | Effect |
|---|---|
| `!perm <tool> [input]` | a tool call that asks for permission; waits for the answer |
| `!ask <question>\|a\|b` | a question with options `a` and `b`; waits for the answer |
| `!tool <tool> [input]` | a tool call that succeeds |
| `!write <file> [text]` | writes `text` to `file` in the thread's directory |
| `!sleep <ms>` | pause |
| `!think <text>` | a thinking event, when `[threads] thinking` is on |
| `!long <bytes>` | a reply of that many bytes |
| `!error [msg]` | end the turn with an error |
| `!transient [n]` | end the turn with a transient error; the next `n - 1` turns fail too |
| `!giveup` | reply "I couldn't finish" |
| `!crash` | exit with code 1 |

`crates/twr-adapters/src/fake.rs` has the details. Scripted tests can emit `Step::EmitSession` for task lifecycle and autonomous-turn events without a turn stamp. `twr-hub/tests/background.rs` exercises background status, automatic resumption, stalled/lost tracking and exit/replay cleanup through the fake adapter.

## The command fuzz

`crates/twr-decisions/tests/command_fuzz.rs` checks how the hub reads the orchestrator's relayed answers and mode changes (`crates/twr-decisions/src/code/command.rs`) against a reference of the same short-command grammar written independently in the test. The fuzz draws random threads (some named or aliased by a stoplist word, a negative pronoun, a noun, a courtesy or qualifier word or a number, some sharing a name part, some near an orchestrator), waiting items (permissions, deny-only permissions, permissions listing no options, questions, an orchestrator's item) and messages: commands built from the grammar, mutations of them (a second sentence, a question mark, courtesy, a label, a typo of a name, a word outside the grammar, a joiner or a mark swapped) and random tokens. Mode calls are sometimes written otherwise ("p'lan", "PLAN", "pl-an") and canonicalized as the hub does for a generic ACP agent. It checks that every call's verdict (direct, uncovered, conflict) equals the reference's, so a message outside the grammar is never direct; that a question is never answered directly; that in a model of the hub nothing is applied without a covering command or the user's Confirm; and that the confirm card for the proposals, with names, labels and summaries now and then full of escapes, invisible characters and the card's own syntax, shows no invisible character as is and reads back as exactly its actions. The regular test run does 10k seeded cases. Before a release, and after any change to the command reading, run 1M cases in release mode, in a target directory of its own that you delete afterwards:

```sh
CARGO_TARGET_DIR=target/fuzz cargo test --release -p twr-decisions --test command_fuzz -- --ignored --nocapture
rm -rf target/fuzz
```

`TWR_FUZZ_SEED` and `TWR_FUZZ_CASES` change the seed and the number of cases; a failure prints the seed, the case and the message.

`crates/twr-decisions/tests/command_bounds.rs` reads messages of the hub's largest size (256 KiB) and messages at the grammar's caps, against items with very long labels, and checks with a counting allocator that each is read quickly and with little memory. The hub reads the message under its core lock, so this guards the caps: 4096 bytes and 48 tokens per message, checked before anything is parsed, and 8 clauses per message and 16 groups per target list, which bound each reading (a longer target list is dropped as a reading; the shorter readings stay). Option labels are never read as words, so a long label costs nothing.

## The translator fuzz

`crates/twr-adapters/tests/translate_fuzz.rs` checks the approval cards of the Claude Code, Codex and ACP translators, which decide what a permission item shows. Each case replays a recorded fixture (Grok Build's for ACP, plus a scripted session for the standard dialect) with its approval requests rewritten and repeated, under the same id and under new ones, and the other lines now and then damaged: a JSON value replaced, a key dropped or added, bytes flipped, the line cut short, dropped or sent twice. Commands, titles and grant roots mix printable ASCII, spaces at the ends, newlines, tabs, other control characters, bidirectional controls and zero-width characters, non-ASCII whitespace, combining marks, non-ASCII letters and the card's own marks, at lengths around each cut. Codex commands come wrapped in `bash -lc` or `sh -c`, quoted either way, nested or broken.

The test decodes the card's escaping with its own reader, written from the rule in `render_command`'s documentation, and reads what runs from the request line the item came from. For every permission item it checks that no translator panics; that the summary, and `full_input` when the summary shows only the start, decode back exactly to the command that runs (a Claude `Bash` request's `command`, followed by every other field of the input but its `description` and `timeout`, the Codex `command` or the script of its shell wrapper as POSIX quoting reads it, an ACP tool call's title or, untitled, its input), and that the labels a runtime supplies (Claude's tool name, an ACP call's kind) show marked; that a command past 64 KiB of marks makes the item deny-only; and that an `allow` answer approves exactly that request, and denies when the item is deny-only. The regular test run does 300 cases per translator. The long run does 100k each, a few minutes in release mode:

```sh
CARGO_TARGET_DIR=target/fuzz cargo test --release -p twr-adapters --test translate_fuzz -- --ignored --nocapture
rm -rf target/fuzz
```

`TWR_FUZZ_SEED` and `TWR_FUZZ_CASES` change the seed and the number of cases, and `TWR_FUZZ_CASE=n` runs case n alone. A failure prints the seed, the case, the request line and the item.

## The frame fuzz

`crates/twr-hub/tests/frame_fuzz.rs` checks how the hub and a runner decode runner protocol messages. Byte streams of WebSocket frames go through tungstenite with the hub's own limits (`pre_auth_ws_config` before a connection authenticates, `runner_ws_config` after): frames at the limit and one byte past it, headers that promise up to 2^63 bytes, fragments that add up past the limit, damaged bytes. Each text message is then decoded as both `RunnerMsg` and `HubMsg`. The messages are valid ones of every kind, mutated ones, random JSON, deep nesting, huge numbers and long escapes. The test checks that nothing panics; that a counting allocator sees decoding take at most 128 times the message's length, and at most 1 MiB for a message of the 8 KiB an unauthenticated peer may send; that tungstenite never yields a message past its limit and refuses a lying header before allocating for it; that every message that decodes encodes back to the same message; and that the handshake accepts a nonce only as 64 lowercase hex digits and a proof only for the same token, side, box and nonces. The regular test run does 4000 cases. The long run does 2M, a few minutes in release mode:

```sh
CARGO_TARGET_DIR=target/fuzz cargo test --release -p twr-hub --test frame_fuzz -- --ignored --nocapture
rm -rf target/fuzz
```

The variables are those of the translator fuzz.

## The runner fuzz harness

`crates/twr-hub/tests/runner_fuzz.rs` is a seeded stress test for the runner reconnect paths. It runs an in-process hub and two real runners behind a relay that cuts connections at any byte, drops and delays messages, makes stale connections, crashes and restarts runners and the hub, and fails SQLite writes. Every few steps it quiesces and checks the invariants: contiguous sequence numbers, replay equal to live state, at most one live runtime per thread, every answer delivered once to the right item, every prompt run once, nothing stuck.

It runs 20 seeds of 40 steps as part of `cargo test`. The long run takes about 30 minutes:

```sh
cargo test -p twr-hub --test runner_fuzz -- --ignored --nocapture
```

| Variable | Effect |
|---|---|
| `TWR_FUZZ_SEED=7` or `7,19` | only these seeds |
| `TWR_FUZZ_SEEDS=n` | number of seeds (default 20; the long run 500) |
| `TWR_FUZZ_BASE=n` | first seed (default 1; the long run 1000) |
| `TWR_FUZZ_ITERS=n` | steps per seed (default 40; the long run 60) |
| `TWR_FUZZ_JOBS=n` | seeds in parallel (default 2) |
| `TWR_FUZZ_MINUTES=n` | the long run starts no new seed after this (default 30) |
| `TWR_FUZZ_TMP=dir` | where the data goes (default `/dev/shm` when there is one) |
| `TWR_FUZZ_LOG=warn\|info\|debug` | tracing to stderr |
| `TWR_FUZZ_TRACE=1` | print the step log as it happens |

A failure prints the seed, the invariant, the details and the end of the step log, and keeps the data directory. A seed fixes the actions but not the thread scheduling, so run a failing seed a few times.

## The container box test

The regular suite tests container boxes through a fake `docker exec`. The tests that use a real docker are ignored; run them where `docker` works for your user:

```sh
cargo test -p twr-cli --test ssh_box container_box -- --ignored
```

They build `docs/tools/container-box/Dockerfile` once as `twr-test-box:<the Dockerfile's hash>` (its base pulled the first time, which needs the network), and run each case in a container of its own, removed at the end: a box from `twr box add --command` to the container's removal (a thread's turns, doctor, `docker restart`, `docker stop` and `start`, the hub's client killed during a daemon restart, `twr hub stop`), the terminal paths (a clone whose client is killed, Ctrl-C during a clone, the detach keys during `twr box login`), and a container that runs as root. The image stays for the next run; `docker image rm twr-test-box:<hash>` removes it. `TWR_TEST_BOX_IMAGE` names another base image (default `ubuntu:24.04`): the debug build the tests upload needs this machine's glibc or newer there.

## CI

`.github/workflows/ci.yml` runs on every push to `dev` and `main` and on every pull request. It uses `pull_request`, never `pull_request_target`, so code from a fork runs without the repository's secrets and with a read-only token, and GitHub asks a maintainer to approve the workflows of a first-time contributor before they run. A new push to a pull request cancels the run still going for its previous one. On `dev` and `main` a new run waits for the one in progress instead, and GitHub keeps one waiting run per branch: a newer push replaces the run still waiting, so of quick pushes only the first and the last get a CI result. The jobs:

- `cargo fmt --check`, shellcheck over `install.sh` and the shell scripts in `docs/tools/` and `.github/scripts/`, and the docs' relative links (`docs/tools/check-links.py`);
- clippy with `-D warnings` and `cargo test --workspace --locked` on Ubuntu and macOS, with Git LFS installed so the LFS tests of `twr-runner` run;
- the `twr-runner` tests on Linux with `RUSTFLAGS=--cfg twr_ps`;
- `cargo deny check` (cargo-deny 0.20.2), with the rules in `deny.toml`: the licences twr's dependencies may have, no known vulnerability or unmaintained crate, crates from crates.io only, no OpenSSL, and HTTP clients in `twr-cli` only. A dependency with a new licence fails it until the licence is added to both `deny.toml` and `about.toml`;
- actionlint (a checksum-pinned release binary) over the workflows, with shellcheck over their `run:` scripts;
- a real-systemd job on Ubuntu: it starts a user manager for the runner's user and runs the ignored `real_systemd_scope` test of `crates/twr-hub/tests/limits.rs`, one worker under `cpu_weight` and `memory_high` whose scope and slice it checks with `systemctl --user show`. When no user manager comes up, it skips with a notice; when one does, the test must not skip (`TWR_TEST_REQUIRE_SYSTEMD`). The rest of the suite replaces `systemd-run` and `systemctl` with stubs. On a machine with a user manager, run it with `cargo test -p twr-hub --test limits -- --ignored real_systemd`; it sets the weight of your `twr.slice` while it runs and puts the old value back;
- only when run by hand, `gh workflow run ci.yml`, the four release builds, without publishing: the static musl builds for Linux x86_64 and aarch64 with cargo-zigbuild, and the Apple silicon and Intel builds on the arm64 macOS runner. Each one checks its binary the way a release does and packs the archive, `NOTICE` and `THIRD-PARTY-LICENSES.html` included. The x86_64 Linux build also checks that the release binary ignores the test token: `.github/scripts/test-token-check.py` starts its hub with `TWR_TEST_USER_TOKEN_FILE` and checks that a client under a `TWR_AGENT` marker that presents the token is still an agent and is refused `hub.shutdown`. (The aarch64 build runs under qemu, where the check isn't run; the gate it tests is the same code on every target.) Locally, `python3 .github/scripts/test-token-check.py --twr target/release/twr --expect refuse` checks a release build, and `--twr target/debug/twr --expect accept` shows a debug build taking the token. Run the release builds before tagging; `gh workflow run ci.yml -f release-builds=false` runs only the regular jobs.

Every job uses the Rust version pinned in `rust-toolchain.toml`. The `release-build` job copies its setup, build commands and checks from `release.yml`, so a broken cross build shows up before a tag is pushed. When you change one, change the other.

## Branches

Work lands on `dev`. `main` moves only for a release, as a fast-forward to the `dev` commit being released, so `main` is always the latest release. twr.tools deploys from `main` (see [Website](#website)), so site and `install.sh` changes go live with the next release too.

## Releases

1. On `dev`, set the version in the workspace `Cargo.toml`, commit and push.
2. The push runs CI. Then run it by hand on that commit, `gh workflow run ci.yml --ref dev`, and wait for it to pass, release builds included (`gh run watch`). Also run the long command fuzz locally, `cargo test --release -p twr-decisions --test command_fuzz -- --ignored --nocapture` (see [The command fuzz](#the-command-fuzz)), and the long [translator](#the-translator-fuzz) and [frame](#the-frame-fuzz) fuzzes.
3. Fast-forward `main` to it and push: `git push origin dev:main` (refused unless it is a fast-forward). Then push a tag `v<version>` on that commit. `.github/workflows/release.yml` checks that the tag matches the version.
4. The workflow builds static musl binaries for Linux x86_64 and aarch64 with cargo-zigbuild, and macOS builds for Apple silicon and Intel with a deployment target of 11.0.
5. Each archive holds `twr`, `LICENSE`, `NOTICE`, `THIRD-PARTY-LICENSES.html` (generated with cargo-about from the dependency graph), `README.md` and `docs/` without `docs/tools/`.
6. The release job writes `sha256sums.txt` (a line naming the release, `# twr release <tag>`, then the checksums of the archives and of the tagged commit's `install.sh`) and signs it and that `install.sh` with the release key (the `release` environment's secret `TWR_RELEASE_SIGNING_KEY`; without it the release fails), adds build provenance attestations for the four archives (`actions/attest-build-provenance`), and publishes a GitHub Release with `twr-<target>.tar.gz` for each target, `sha256sums.txt`, `install.sh`, their `.sig` files and install instructions, verification included.

A tag with a `-`, such as `v0.2.0-rc.1`, makes a prerelease, which `install.sh` skips unless `TWR_VERSION` names it. Running the workflow by hand with an existing tag rebuilds it and replaces the release's files; a draft release that an interrupted run left behind is published then. That only works when it's started from `main`. A run started from any other branch fails, because a branch could carry a modified workflow.

What the workflow checks:

- The workflow resolves the tag to one commit through the API. Every build checks out that commit, and the release step checks that the tag still points at it before publishing.
- A run for a tag never cancels one in progress for the same tag: it waits for it. GitHub keeps one waiting run per tag, so a newer run replaces the one still waiting, which ends cancelled before it starts.
- Each build checks its binary's architecture with `file`, checks that the musl builds are statically linked, and runs `twr --version`, which must print the tag's version. The arm64 Linux build runs under qemu. The Intel macOS build runs under Rosetta when the runner has it, and otherwise prints a warning.
- Each build job records its archive's SHA-256 as a job output. The release job downloads the four artifacts by name, checks that nothing else arrived, compares each archive with its recorded hash and only then writes `sha256sums.txt`.
- The signatures are checked against the committed public key, `release/signing-key.pub`, before anything is published, so a secret that is not its private half fails the release. `docs/tools/make-signing-key.sh` makes a key pair: the private key stays outside the repository, and the script writes the public key and install.sh's copy of it, and prints the `gh secret set` command and the bytes for `RELEASE_KEY` in `crates/twr-cli/src/main.rs`.
- Users check the signatures: `twr update` verifies both with the key built into twr before it runs anything, and `install.sh` verifies `sha256sums.txt.sig` where `openssl` can ([install.md](install.md#the-install-script)). Both also check that `sha256sums.txt` names the release asked for, and `twr update` that it lists the `install.sh` it runs.
- The attestations let anyone check where an archive was built: `gh attestation verify <archive> --repo ignaval/twr --signer-workflow ignaval/twr/.github/workflows/release.yml`. The job that makes them is the only one with `id-token: write` and `attestations: write`. Only a tag push makes them: a manual rebuild runs from `main` while it builds the tag's commit, so its provenance would name the wrong source, and the archives it replaces carry no attestation (their signed checksums still hold).
- Third-party code is pinned. Actions use full commit SHAs, with the release in a comment. The Rust toolchain comes from `rust-toolchain.toml`. pip installs zig and cargo-zigbuild from PyPI wheels with `--require-hashes --only-binary :all:`. To update an action, look up the new tag's commit (`gh api repos/<owner>/<repo>/git/ref/tags/<tag>`, and follow an annotated tag to its commit) and replace both the SHA and the comment. When bumping cargo-zigbuild or ziglang, take the hashes of the `manylinux` x86_64 wheels from `https://pypi.org/pypi/<package>/<version>/json`.

### Repository settings

These GitHub settings close gaps the workflows can't close by themselves:

- **SHA-pinned actions.** Settings → Actions → General only allows actions pinned to a full commit SHA. The workflows comply.
- **The `release` environment.** The `release` job in `release.yml` runs in it. It deploys only from `v*` tags and `main` (a manual rebuild runs from `main`), and publishing waits for a maintainer's approval.
- **A tag ruleset for `v*`.** Only admins can create, move or delete a `v*` tag, so only they can start a release or change a released one.
- **Branch protection on `main` and `dev`.** Changes from others go through a pull request with the CI jobs green. Force pushes and deletion are refused. Admins are not bound.
- **Security features.** Private vulnerability reporting (see [SECURITY.md](../SECURITY.md)), secret scanning with push protection, and Dependabot alerts. `.github/dependabot.yml` has Dependabot propose weekly updates of the Cargo dependencies and the pinned actions, against `dev`.

`docs/tools/github-settings.sh` applies the tag ruleset, the branch protection and the environment's reviewer, and keeps its deployment policy. It lists the required checks by job name: when a job of `ci.yml` is added or renamed, update the list and run the script again. The SHA-pin rule, the environment's deployment policy and the security features are set in the repository's settings.

## Website

[twr.tools](https://twr.tools) is a Vercel project linked to this repository. Each push to `main` that changes `site/`, `install.sh`, `docs/img/` or `vercel.json` deploys it. `vercel.json` copies `site/index.html`, `install.sh`, the demo recording and the grid screenshot into the output, after checking `install.sh` with `sh -n`. Other branches don't deploy. `install.sh` is served as plain text with a 5-minute cache, so a fix reaches users quickly. `twr.codes` and both `www.` names redirect to `twr.tools` with a 308, keeping the path.

## Screenshots

The still TUI images in the README and [tui.md](tui.md) are real captures:

```sh
docs/tools/make-screenshots.sh
```

The script builds twr and starts a throwaway hub with `TWR_HOME` under `/tmp` and the `fake` runtime for the worker threads. The orchestrator can't be `fake`, so the script writes a small scripted ACP agent (`[runtimes.scripted]`) that answers the two typed messages and sums up each hub update in a sentence. It makes five small git repositories and drives eight threads into different states: waiting on a permission, asking a question, failed, running, done and parked. Then it runs `twr` in a private tmux server, captures the stack view at 160x42, a card selected, and the grid view at 160x25, a tile selected, with their colors, and converts the captures with `docs/tools/ansi2svg.py`, which uses only the Python standard library. The hub gets a `PATH` without `claude`, `codex` or `grok`, so no real agent can start. Everything is removed on exit.

It needs git, tmux and python3, and takes about a minute because it waits for the hub's 30-second git snapshot. Set `TWR_SHOTS_KEEP=<dir>` to keep the raw captures.

The animated recording at the top of the README and the site, `docs/img/tui-demo.svg`, is a real session too:

```sh
docs/tools/make-demo.sh
```

Its orchestrator and workers are small scripted ACP agents, `docs/tools/demo/orch.py` and `docs/tools/demo/worker.py`. The orchestrator calls twr's real MCP tools, so it really starts the two threads and relays "allow ratelimit". The workers write and commit files in their worktrees and ask for permissions over ACP, which become cards. The script types into the TUI in a private tmux server, captures the screen about ten times a second, and `ansi2svg.py --frames` turns the captures into one SVG that loops with CSS animation. Each element is drawn once and shown in the frames that have it, so the file stays under 100 KB. Viewers that ask for reduced motion see one still frame. The workers wait on files the script creates, so the script decides when each permission comes up.

It needs git, tmux and python3 and takes about a minute. `TWR_DEMO_KEEP=<dir>` keeps the raw frames and the hub's log, and `TWR_BIN=<path>` uses a twr binary you already built.

## Docs

`docs/tools/check-links.py` checks every relative link and anchor in the top-level Markdown files (`README.md`, `SKILL.md`, `CONTRIBUTING.md`, ...) and `docs/`:

```sh
python3 docs/tools/check-links.py
```

[SKILL.md](../SKILL.md) stays at the repository root, where agents look for it. It's the reference for the socket API, events and MCP tools; keep it in step with the code.
