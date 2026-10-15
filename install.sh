#!/bin/sh
# Installs twr, a control tower for parallel coding agents (https://github.com/ignaval/twr):
#
#   curl -fsSL https://twr.tools/install.sh | sh
#
# twr.tools serves this file from the repository's main branch.
#
# It downloads the release binary for this machine (Linux x86_64/aarch64, static; macOS Apple
# silicon/Intel), checks it against the release's sha256sums.txt and installs it for the
# current user (no sudo). With a `gh` signed in to github.com it downloads through
# `gh release download`.
#
# sha256sums.txt is signed (sha256sums.txt.sig, Ed25519, the key in the block below). When
# openssl here can check Ed25519 signatures (OpenSSL 3.0 or newer; macOS's LibreSSL can't),
# the signature must verify; otherwise the script says the checksums were not
# signature-checked and goes on. `twr update` checks the signatures itself, with the key built
# into twr. sha256sums.txt names its release on a signed line, `# twr release <tag>`, which
# must be the TWR_VERSION asked for.
#
# Environment:
#   TWR_VERSION            the release to install, e.g. v0.2.0 (default: the latest)
#   TWR_INSTALL_DIR        where to put `twr` (default: ~/.local/bin; a relative path is
#                          taken from the current directory)
#   TWR_DOWNLOAD_URL       fetch twr-<target>.tar.gz, sha256sums.txt and sha256sums.txt.sig
#                          from this base URL instead of GitHub: any URL curl or wget can
#                          fetch (http:// too: your choice), or file:///<dir> for a local
#                          directory (copied, without curl or wget)
#   TWR_REQUIRE_SIGNATURE  1: fail when the signature can't be checked (no openssl that
#                          verifies Ed25519), except with TWR_SHA256SUMS_FILE, which the
#                          caller (twr update) has verified
#   TWR_SHA256SUMS_FILE    use this sha256sums.txt instead of downloading it; its signature
#                          is not checked here, even with TWR_REQUIRE_SIGNATURE=1: the caller
#                          (`twr update`) passes the one it verified
#   TWR_UPDATE             1 when `twr update` runs this: no first-install advice

set -eu

REPO="ignaval/twr"
# gh resolves a bare OWNER/REPO against GH_HOST or its default host, which may be a GitHub
# Enterprise server: name github.com explicitly.
GH_REPO_REF="github.com/$REPO"

# --- release key ---
# The public key releases are signed with (release/signing-key.pub), and a fixed message
# signed with it: openssl must verify that before it is trusted to check a release.
# docs/tools/make-signing-key.sh writes this block.
RELEASE_KEY='-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAoEFCsIDDaJTSlpcteH2pp3aZpLQ+SB+18sUdZNqe9dg=
-----END PUBLIC KEY-----'
PROBE_MESSAGE='twr signature probe'
PROBE_SIGNATURE='fEAddJcpdHzHQJhiSd6QOtAlzR7Boxkn0wpZJBGVKdOUF06lnwox2LcTgjFSh9ygU9Yd9G73suI6hoL/+5xPCA=='
# --- end release key ---

# The staged binary next to the target, removed on every way out until it is renamed into place.
staged=""
tmp=""

cleanup() {
    if [ -n "$staged" ]; then
        rm -f "$staged"
    fi
    if [ -n "$tmp" ]; then
        rm -rf "$tmp"
    fi
}

say() {
    printf '%s\n' "$*"
}

err() {
    printf 'twr install: %s\n' "$*" >&2
    exit 1
}

warn() {
    printf 'twr install: %s\n' "$*" >&2
}

has() {
    command -v "$1" > /dev/null 2>&1
}

# The release target of this machine, e.g. aarch64-apple-darwin.
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Linux) os=unknown-linux-musl ;;
        Darwin)
            os=apple-darwin
            # A shell running under Rosetta reports x86_64 on Apple silicon: use the native build.
            if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2> /dev/null || true)" = 1 ]; then
                arch=arm64
            fi
            ;;
        *) err "unsupported operating system: $os (twr runs on Linux and macOS)" ;;
    esac
    case "$arch" in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) err "unsupported CPU architecture: $arch (twr is built for x86_64 and aarch64)" ;;
    esac
    printf '%s-%s\n' "$arch" "$os"
}

# Whether to draw a progress bar for the binary's download: only on a terminal.
progress=""
if [ -t 2 ]; then
    progress=1
fi

# Whether curl may only use https (TLS 1.2 or newer), redirects included: for GitHub, not for
# a TWR_DOWNLOAD_URL the user gave (which may be http:// on purpose).
strict=1

# download URL FILE [bar]: with "bar" (and a terminal), shows a progress bar.
download() {
    case "$1" in
        file://*)
            # A local release directory: copy the file, no curl or wget needed (wget has no
            # file:// support). file://localhost/... names the same local path.
            src=${1#file://}
            src=${src#localhost}
            case "$src" in
                /*) cp "$src" "$2" ;;
                *) err "$1 is not a local file:// URL (use file:///absolute/path)" ;;
            esac
            return
            ;;
    esac
    if has curl; then
        if [ -n "$progress" ] && [ "${3:-}" = bar ]; then
            curl -fL ${strict:+--proto '=https' --tlsv1.2} --retry 3 --progress-bar -o "$2" "$1"
        else
            curl -fsSL ${strict:+--proto '=https' --tlsv1.2} --retry 3 -o "$2" "$1"
        fi
    elif has wget; then
        if [ -n "$progress" ] && [ "${3:-}" = bar ] && wget --help 2>&1 | grep -q -- --show-progress; then
            # From the file's directory, so the bar is labelled with its name, not the temp path.
            (cd "$(dirname "$2")" && wget -q --show-progress -O "$(basename "$2")" "$1")
        else
            wget -q -O "$2" "$1"
        fi
    else
        err "need curl or wget to download twr"
    fi
}

# gh_with_bar ASSET DIR: downloads the release's ASSET into DIR through the GitHub API with
# curl, to show a progress bar (gh release download shows none). gh supplies the asset's API
# URL and the token, which goes to curl in a header file only this user can read, never on a
# command line. curl drops the header when GitHub redirects to its storage host. Fails
# quietly (for the caller to use gh instead) when anything is missing.
gh_with_bar() {
    if [ -z "$progress" ] || ! has curl; then
        return 1
    fi
    url=$(gh release view ${version:+"$version"} -R "$GH_REPO_REF" --json assets \
        --jq ".assets[] | select(.name == \"$1\") | .apiUrl" 2> /dev/null) || return 1
    [ -n "$url" ] || return 1
    token=$(gh auth token --hostname github.com 2> /dev/null) || return 1
    [ -n "$token" ] || return 1
    (umask 077 && printf 'Authorization: Bearer %s\nAccept: application/octet-stream\n' "$token" > "$2/.headers")
    token=""
    if curl -fL --proto '=https' --tlsv1.2 --retry 3 --progress-bar -H @"$2/.headers" \
        -o "$2/$1" "$url"; then
        rm -f "$2/.headers"
        return 0
    fi
    rm -f "$2/.headers" "$2/$1"
    return 1
}

# The release's base URL the files came from, or "gh" when gh downloaded them (set by fetch,
# used by fetch_signature).
origin=""

# fetch ASSET DIR: puts ASSET and sha256sums.txt of the release into DIR (ASSET only when
# TWR_SHA256SUMS_FILE gives the checksums).
fetch() {
    sums=sha256sums.txt
    if [ -n "${TWR_SHA256SUMS_FILE:-}" ]; then
        sums=""
    fi
    if [ -n "${TWR_DOWNLOAD_URL:-}" ]; then
        base=${TWR_DOWNLOAD_URL%/}
        origin=$base
        strict=""
        if download "$base/$1" "$2/$1" bar && { [ -z "$sums" ] || download "$base/$sums" "$2/$sums"; }; then
            return 0
        fi
        err "could not download $1${sums:+ and $sums} from $base"
    fi
    via_gh=""
    if has gh && gh auth status --hostname github.com > /dev/null 2>&1; then
        via_gh=1
        origin=gh
        if [ -z "$sums" ] || gh release download ${version:+"$version"} -R "$GH_REPO_REF" -p "$sums" \
            -D "$2" --clobber > /dev/null 2>&1; then
            if gh_with_bar "$1" "$2" || gh release download ${version:+"$version"} -R "$GH_REPO_REF" \
                -p "$1" -D "$2" --clobber > /dev/null 2>&1; then
                return 0
            fi
        fi
        say "gh could not download the release; trying without it"
    fi
    if [ -n "$version" ]; then
        base="https://github.com/$REPO/releases/download/$version"
    else
        base="https://github.com/$REPO/releases/latest/download"
    fi
    origin=$base
    if download "$base/$1" "$2/$1" bar && { [ -z "$sums" ] || download "$base/$sums" "$2/$sums"; }; then
        return 0
    fi
    if [ -n "$via_gh" ]; then
        err "could not download $1 from $base, nor with gh (is there such a release?)"
    fi
    err "could not download $1 from $base (is there such a release?)"
}

# fetch_signature DIR: puts sha256sums.txt.sig into DIR, from where sha256sums.txt came from.
fetch_signature() {
    if [ "$origin" = gh ]; then
        gh release download ${version:+"$version"} -R "$GH_REPO_REF" -p sha256sums.txt.sig \
            -D "$1" --clobber > /dev/null 2>&1
    else
        download "$origin/sha256sums.txt.sig" "$1/sha256sums.txt.sig" 2> /dev/null
    fi
}

# has_verifier: whether openssl here checks Ed25519 signatures. It must verify the probe, a
# fixed message signed with the release key (OpenSSL 3.0 and newer do; LibreSSL, macOS's
# openssl, does not).
has_verifier() {
    has openssl || return 1
    printf '%s\n' "$RELEASE_KEY" > "$tmp/release-key.pem" || return 1
    printf '%s' "$PROBE_MESSAGE" > "$tmp/probe.msg" || return 1
    signature "$PROBE_SIGNATURE" "$tmp/probe.sig" || return 1
    openssl pkeyutl -verify -rawin -pubin -inkey "$tmp/release-key.pem" -sigfile "$tmp/probe.sig" \
        -in "$tmp/probe.msg" > /dev/null 2>&1
}

# signature TEXT FILE: decodes TEXT, the base64 of an Ed25519 signature, into FILE; fails when
# TEXT is not that.
signature() {
    case "$1" in
        "" | *[!A-Za-z0-9+/=]*) return 1 ;;
    esac
    [ "${#1}" -eq 88 ] || return 1
    printf '%s' "$1" | openssl base64 -d -A > "$2" 2> /dev/null || return 1
    [ "$(wc -c < "$2" | tr -d ' ')" = 64 ]
}

# check_signature DIR: checks DIR/sha256sums.txt against the release's signature of it, when
# openssl here can. A missing or bad signature always stops the install: every release is
# signed, so its files were changed or are incomplete.
check_signature() {
    if ! has_verifier; then
        if [ "${TWR_REQUIRE_SIGNATURE:-}" = 1 ]; then
            err "TWR_REQUIRE_SIGNATURE=1, but this machine has no openssl that checks Ed25519 signatures (OpenSSL 3.0 or newer); nothing was installed"
        fi
        warn "the checksums were not signature-checked (no openssl with Ed25519 support)"
        return 0
    fi
    if ! fetch_signature "$1"; then
        err "could not download sha256sums.txt.sig; nothing was installed (every twr release is signed)"
    fi
    if ! signature "$(cat "$1/sha256sums.txt.sig")" "$1/sha256sums.sig.bin"; then
        err "sha256sums.txt.sig is not an Ed25519 signature; nothing was installed"
    fi
    if ! openssl pkeyutl -verify -rawin -pubin -inkey "$tmp/release-key.pem" \
        -sigfile "$1/sha256sums.sig.bin" -in "$1/sha256sums.txt" > /dev/null 2>&1; then
        err "the signature of sha256sums.txt does not verify with the release key; nothing was installed"
    fi
}

# check_release DIR: DIR/sha256sums.txt must name one release (`# twr release <tag>`, signed
# with the rest), and that must be the TWR_VERSION asked for, so the signed checksums of
# another release are refused.
check_release() {
    release=$(awk 'index($0, "# twr release ") == 1 { n++; t = substr($0, 15) } END { if (n == 1) print t }' "$1/sha256sums.txt")
    case "$release" in
        v[0-9]*) ;;
        *) err "sha256sums.txt does not name one release (a '# twr release <tag>' line); nothing was installed" ;;
    esac
    if [ -n "$version" ] && [ "$release" != "$version" ]; then
        err "sha256sums.txt is the checksums of $release, not of $version; nothing was installed"
    fi
}

# verify DIR ASSET: checks DIR/ASSET against DIR/sha256sums.txt.
verify() {
    expected=$(awk -v f="$2" '$2 == f || $2 == "*" f { print $1; exit }' "$1/sha256sums.txt")
    [ -n "$expected" ] || err "$2 is not listed in sha256sums.txt"
    if has sha256sum; then
        actual=$(sha256sum "$1/$2" | awk '{ print $1 }')
    elif has shasum; then
        actual=$(shasum -a 256 "$1/$2" | awk '{ print $1 }')
    else
        err "need sha256sum or shasum to verify the download"
    fi
    [ "$actual" = "$expected" ] || err "checksum mismatch for $2 (expected $expected, got $actual); nothing was installed"
}

# The startup file to add PATH to, for the user's login shell (printed for the user to type,
# so the tilde stays as it is).
# shellcheck disable=SC2088
profile_file() {
    case "${SHELL:-}" in
        */zsh) printf '%s\n' "~/.zshrc" ;;
        */bash)
            if [ "$(uname -s)" = Darwin ]; then
                printf '%s\n' "~/.bash_profile"
            else
                printf '%s\n' "~/.bashrc"
            fi
            ;;
        *) printf '%s\n' "~/.profile" ;;
    esac
}

# path_advice DIR: the command that adds DIR to PATH for the user's shell, to paste as shown
# whatever DIR holds: `fish_add_path 'DIR'` for fish (`\` and `'` escaped as fish reads single
# quotes), else a line appended to the shell's profile, `export PATH="DIR:$PATH"` (`\`, `"`, `$`
# and `` ` `` escaped for double quotes), itself single-quoted (`'` as `'\''`).
path_advice() {
    case "${SHELL:-}" in
        */fish)
            fq=$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e "s/'/\\\\'/g")
            say "  fish_add_path '$fq'"
            ;;
        *)
            dq=$(printf '%s' "$1" | sed 's/[\\"$`]/\\&/g')
            sq=$(printf '%s' "export PATH=\"$dq:\$PATH\"" | sed "s/'/'\\\\''/g")
            say "  printf '%s\\n' '$sq' >> $(profile_file)"
            ;;
    esac
}

main() {
    target=$(detect_target)
    version=${TWR_VERSION:-}
    case "$version" in
        "" | latest) version="" ;;
        v*) ;;
        *) version="v$version" ;;
    esac
    if [ -z "${TWR_INSTALL_DIR:-}" ] && [ -z "${HOME:-}" ]; then
        err "HOME is not set; set TWR_INSTALL_DIR to where twr should go"
    fi
    dir=${TWR_INSTALL_DIR:-$HOME/.local/bin}
    case "$dir" in
        /*) ;;
        "~"*) err "TWR_INSTALL_DIR=$dir starts with ~, which the shell did not expand; use \$HOME instead" ;;
        *)
            here=$(pwd) || err "could not read the current directory for TWR_INSTALL_DIR=$dir"
            dir="${here%/}/$dir"
            ;;
    esac
    asset="twr-$target.tar.gz"

    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 141' PIPE
    trap 'exit 143' TERM
    tmp=$(mktemp -d 2> /dev/null || mktemp -d -t twr) || err "could not create a temporary directory"

    if [ "${TWR_UPDATE:-}" = 1 ]; then
        say "Updating twr to ${version:-the latest release} for $target..."
    else
        say "Installing twr ${version:-(latest release)} for $target..."
    fi
    fetch "$asset" "$tmp"
    if [ -n "${TWR_SHA256SUMS_FILE:-}" ]; then
        # Checked by the caller (`twr update` verifies its signature).
        cp "$TWR_SHA256SUMS_FILE" "$tmp/sha256sums.txt" || err "could not read TWR_SHA256SUMS_FILE=$TWR_SHA256SUMS_FILE"
    else
        check_signature "$tmp"
    fi
    check_release "$tmp"
    verify "$tmp" "$asset"
    mkdir "$tmp/x"
    tar -xzf "$tmp/$asset" -C "$tmp/x" || err "could not unpack $asset"
    [ -f "$tmp/x/twr" ] || err "$asset has no twr binary"

    mkdir -p "$dir" || err "could not create $dir"
    if [ -d "$dir/twr" ]; then
        err "$dir/twr is a directory; move it away or set TWR_INSTALL_DIR to another place"
    fi
    # Stage next to the target and run it there; only a binary that runs is renamed over the
    # old one (a running twr keeps its old binary). On any failure the old twr stays. mktemp
    # creates the staging file itself, so nothing already at that path is written through.
    staged=$(mktemp "$dir/.twr.new.XXXXXX") || err "could not write to $dir"
    cp "$tmp/x/twr" "$staged" || err "could not write to $dir"
    chmod 755 "$staged" || err "could not make $staged executable"
    # It must exit 0 as well as print its version: one that prints it and then fails or
    # crashes does not run here.
    installed=$("$staged" --version 2> /dev/null) || installed=""
    case "$installed" in
        "twr "*) ;;
        *) err "the downloaded twr does not run on this machine (tried in $dir); nothing was installed" ;;
    esac
    mv -f "$staged" "$dir/twr" || err "could not install $dir/twr"
    staged=""
    say "Installed $installed to $dir/twr"

    case ":${PATH:-}:" in
        *":$dir:"*)
            found=$(command -v twr 2> /dev/null || true)
            if [ -n "$found" ] && [ "$found" != "$dir/twr" ]; then
                say ""
                say "Note: $found comes first on your PATH and shadows the new twr."
                say "Remove it (cargo uninstall twr-cli, if cargo installed it) or move $dir earlier."
            fi
            ;;
        *)
            if [ "${TWR_UPDATE:-}" = 1 ]; then
                return 0
            fi
            say ""
            say "$dir is not on your PATH. Add it with:"
            say ""
            path_advice "$dir"
            say ""
            say "then open a new terminal."
            ;;
    esac

    if [ "${TWR_UPDATE:-}" = 1 ]; then
        return 0
    fi
    say ""
    say "Next steps:"
    say "  twr                      open the TUI (starts the hub in the background)"
    say "  twr thread new <repo>    start an agent thread on a git repository"
    say ""
    say "twr needs git and at least one agent CLI: claude (Claude Code), codex or grok."
}

main "$@"
