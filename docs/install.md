# Install

twr is one binary, `twr`. It runs on Linux (x86_64, aarch64) and macOS (Apple silicon, Intel). macOS is built and tested in CI, but has had less hands-on use than Linux so far. Windows is unsupported. WSL2 runs Linux binaries, but twr is untested there. It needs git and at least one agent CLI: `claude` (Claude Code 2.1.110 or newer), `codex` or `grok`. Sign in with each agent's own CLI first. twr never runs a login and never sees the keys. [runtimes.md](runtimes.md#what-has-been-checked-against-the-real-clis) lists the versions twr was checked against.

## The install script

```sh
curl -fsSL https://twr.tools/install.sh | sh
```

`twr.tools` serves the `install.sh` of this repository's `main` branch (`twr.codes` redirects there).

The script is also in the repository, and you can fetch it from GitHub instead:

```sh
curl -fsSL https://raw.githubusercontent.com/ignaval/twr/main/install.sh | sh
```

To read the script before it runs, or to check its signature first ([Checking the signature](#checking-the-signature)), download it to a file and run that file with `sh`.

The script picks the release build for your machine, checks it against the release's `sha256sums.txt` and installs it for the current user, without sudo. On Apple silicon it installs the native build even when the shell runs under Rosetta. With `gh` signed in to github.com it downloads through `gh release download -R github.com/ignaval/twr`. Otherwise it uses curl, over https only (TLS 1.2 or newer), or, without curl, wget with its own defaults. A `file://` base in `TWR_DOWNLOAD_URL` is copied directly and needs neither.

`sha256sums.txt` is signed. The script holds the release public key (the one in [`release/signing-key.pub`](../release/signing-key.pub)) and checks `sha256sums.txt.sig` with `openssl` when the `openssl` on your machine can verify Ed25519 signatures (OpenSSL 3.0 or newer). It tests that first on a signature built into the script. If it can, a missing or bad signature stops the install: every release is signed, so either means the release's files were changed or are incomplete. If it can't (macOS ships LibreSSL, which can't), the script prints `the checksums were not signature-checked` and goes on: the checksums then guard against a broken download, not against a changed release. `twr update` checks the signatures itself, with the key built into twr, whatever `openssl` you have.

`sha256sums.txt` starts with a line naming its release, `# twr release v0.2.0`, signed with the rest. The script refuses checksums that name another release than the `TWR_VERSION` you asked for, so the signed files of an older release can't pass for the one you named. `twr update` checks the same, and also that the release's `install.sh` is the one `sha256sums.txt` lists.

It reads these environment variables:

| Variable | Default | Effect |
|---|---|---|
| `TWR_VERSION` | the latest release | install this release, e.g. `v0.2.0`; needed for a prerelease |
| `TWR_INSTALL_DIR` | `~/.local/bin` | where `twr` goes; a relative path is taken from the current directory. Write `$HOME`, not a quoted `~`, which the script refuses |
| `TWR_DOWNLOAD_URL` | GitHub | a base URL with `twr-<target>.tar.gz`, `sha256sums.txt` and `sha256sums.txt.sig`, or `file:///tmp/release` for a local directory. Any scheme curl or wget fetches is used as you give it, `http://` included |
| `TWR_REQUIRE_SIGNATURE` | unset | `1`: stop when the signature can't be checked (no `openssl` that verifies Ed25519), except with `TWR_SHA256SUMS_FILE`, which the caller (`twr update`) has verified |
| `TWR_SHA256SUMS_FILE` | unset | use this `sha256sums.txt` instead of downloading it, and skip the signature check, even with `TWR_REQUIRE_SIGNATURE=1`: the caller (`twr update`) passes the one it verified |
| `TWR_UPDATE` | unset | `1` when `twr update` runs the script: it says "Updating" and leaves out the advice for a first install |

Put the variable before `sh`: `curl -fsSL https://twr.tools/install.sh | TWR_VERSION=v0.2.0 sh`.

The script copies the new binary into the install directory under a temporary name and runs `twr --version` from there. Only if that prints its version and exits successfully does it rename the new binary over the old one, so a running `twr` keeps working. If the binary doesn't run, or the download, checksum or copy fails, or the script is interrupted, the script removes the temporary file and the old `twr` stays. If the install directory isn't on your `PATH`, the script prints the line to add to your shell's startup file. If another `twr` comes first on your `PATH`, for example one from `cargo install`, it says so.

## Release builds

Each release has four archives, a checksum file (of the archives and `install.sh`), the release's `install.sh`, and an Ed25519 signature of the checksum file and of `install.sh` (`sha256sums.txt.sig`, `install.sh.sig`: base64 of the raw 64-byte signature):

| Archive | Platform |
|---|---|
| `twr-x86_64-unknown-linux-musl.tar.gz` | Linux x86_64, static |
| `twr-aarch64-unknown-linux-musl.tar.gz` | Linux arm64, static |
| `twr-aarch64-apple-darwin.tar.gz` | macOS on Apple silicon, 11.0 or newer |
| `twr-x86_64-apple-darwin.tar.gz` | macOS on Intel, 11.0 or newer |

Each archive holds the `twr` binary, `LICENSE`, `NOTICE`, `THIRD-PARTY-LICENSES.html` (the licences of the code compiled into twr), `README.md` and the `docs/` directory, without the maintainers' scripts in `docs/tools`. To install by hand, download your archive and `sha256sums.txt` into one directory and check the archive. On Linux:

```sh
sha256sum -c sha256sums.txt --ignore-missing
```

On macOS, which has `shasum` instead:

```sh
grep ' twr-aarch64-apple-darwin.tar.gz$' sha256sums.txt | shasum -a 256 -c
```

Use `twr-x86_64-apple-darwin.tar.gz` on Intel. Then unpack it with `tar -xzf` and put `twr` on your `PATH`.

The macOS builds are not signed or notarized by Apple. An archive downloaded with a browser carries the quarantine flag, and Gatekeeper then refuses to run `twr`. Remove the flag from the unpacked binary:

```sh
xattr -d com.apple.quarantine twr
```

`curl` sets no such flag, so the install script and `twr update` don't need this.

### Checking the signature

The checksums are only as good as the file they come from. To check that `sha256sums.txt` is the one the release workflow signed, save the release public key, [`release/signing-key.pub`](../release/signing-key.pub) in the repository, as `signing-key.pub`, download `sha256sums.txt.sig` next to `sha256sums.txt`, and run, with OpenSSL 3.0 or newer:

```sh
openssl base64 -d -A -in sha256sums.txt.sig -out sha256sums.txt.sig.bin
openssl pkeyutl -verify -rawin -pubin -inkey signing-key.pub \
  -sigfile sha256sums.txt.sig.bin -in sha256sums.txt
```

It prints `Signature Verified Successfully`. `install.sh` and `install.sh.sig` check the same way. macOS's `openssl` is LibreSSL, which can't do this; install OpenSSL 3 (for example with Homebrew) or check on a Linux machine.

The releases also carry build provenance attestations, from v0.2.0 on, so the GitHub CLI can check that an archive was built by the release workflow from this repository:

```sh
gh attestation verify twr-x86_64-unknown-linux-musl.tar.gz --repo ignaval/twr \
  --signer-workflow ignaval/twr/.github/workflows/release.yml
```

## From source

You need a stable Rust toolchain, 1.89 or newer, and git. The repository's `rust-toolchain.toml` pins the Rust that CI and the releases use (1.98.1). With rustup, cargo installs that version and uses it in the repository. To build with another toolchain, run `cargo +<toolchain> ...`.

```sh
cargo install --locked --git https://github.com/ignaval/twr twr-cli
```

builds the latest commit of the default branch, `dev`, where development happens. Add `--tag v0.2.0` (or another release tag) to build a release, or `--branch main` for the latest one. From a clone of the repository:

```sh
git clone https://github.com/ignaval/twr
cd twr
cargo install --locked --path crates/twr-cli
```

Either puts `twr` in `~/.cargo/bin`. `twr update` doesn't replace a `twr` installed by cargo: run the same `cargo install` again (add `--force` if cargo says it's already installed). SQLite is bundled, so there is no system library to install.

### A static Linux binary

The release builds are static musl binaries cross-compiled with [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild). With zig and cargo-zigbuild installed:

```sh
cargo zigbuild --release --locked -p twr-cli --target x86_64-unknown-linux-musl
# target/x86_64-unknown-linux-musl/release/twr
```

Without zig you can build a static glibc binary to copy to another Linux box:

```sh
RUSTFLAGS="-C target-feature=+crt-static" \
  cargo build --release -p twr-cli --target x86_64-unknown-linux-gnu
# target/x86_64-unknown-linux-gnu/release/twr
```

A static glibc build still loads glibc's name-service libraries at runtime when it resolves a host name, as a runner does when it reaches its hub by name. If that matters, build for `x86_64-unknown-linux-musl` with `cargo build`, which needs `musl-gcc` for the bundled SQLite.

## Where twr keeps things

| What | Default | With `TWR_HOME=X` |
|---|---|---|
| config | `~/.config/twr/config.toml` (`$XDG_CONFIG_HOME`) | `X/config.toml` |
| data: `twr.db`, `worktrees/`, `hub.log` | `~/.local/share/twr/` (`$XDG_DATA_HOME`) | `X/data/` |
| hub socket | `$XDG_RUNTIME_DIR/twr/hub.sock`, else `~/.local/state/twr/hub.sock` (`$XDG_STATE_HOME`) | `X/hub.sock` |
| data of [workspace](workspaces.md) `W` | `~/.local/share/twr/workspaces/W/` | `X/data/workspaces/W/` |
| hub socket of workspace `W` | `$XDG_RUNTIME_DIR/twr/ws/W.sock`, else `~/.local/state/twr/ws/W.sock` | `X/ws/W.sock` |

An empty variable counts as unset. `--socket` or `[hub] socket` in the config overrides the socket path. A Unix socket path can be at most 107 bytes on Linux and 103 on macOS.

To uninstall, delete the binary and, if you want the history gone too, the data directory. Thread worktrees live in the data directory, so commit what you want to keep first. Workspaces keep their data, worktrees included, inside it too (`workspaces/`). Their branches (`twr/<name>`, `twr/@<workspace>/<name>` in a named workspace, or under `twr/<home-path-hash>/` with `TWR_HOME`, with the name each thread had when its worktree was made) live in your repositories and stay.

## Next

```sh
twr                          # open the TUI (starts the hub in the background)
twr thread new <repo>        # start an agent thread on a git repository
```

See [usage.md](usage.md).
