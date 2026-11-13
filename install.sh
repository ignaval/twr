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
# With --desktop (`curl -fsSL https://twr.tools/install.sh | sh -s -- --desktop`) it installs
# the desktop app instead, checked against the release's desktop-sha256sums.txt, which is
# signed and checked like sha256sums.txt: on macOS (Apple silicon) the app from the disk image
# into /Applications (~/Applications when /Applications isn't writable); on Arch Linux the
# Arch package, and on Debian or Ubuntu the .deb, both with sudo; on other x86_64 Linux the
# AppImage into ~/.local/bin/twr-desktop, with a launcher entry.
#
# sha256sums.txt is signed (sha256sums.txt.sig, Ed25519, the key in the block below). When
# openssl here can check Ed25519 signatures (OpenSSL 3.0 or newer; macOS's LibreSSL can't),
# the signature must verify. Otherwise, when the P-256 block below holds a key, the ECDSA P-256
# signature (sha256sums.txt.p256.sig), which LibreSSL checks, must verify instead; without
# either the script says the checksums were not signature-checked and goes on. `twr update`
# checks the signatures itself, with the key built into twr. sha256sums.txt names its release
# on a signed line, `# twr release <tag>`, which must be the TWR_VERSION asked for.
#
# Environment:
#   TWR_VERSION            the release to install, e.g. v0.2.0 (default: the latest)
#   TWR_INSTALL_DIR        where to put `twr` (default: ~/.local/bin; a relative path is
#                          taken from the current directory)
#   TWR_DOWNLOAD_URL       fetch twr-<target>.tar.gz, sha256sums.txt and sha256sums.txt.sig
#                          (or sha256sums.txt.p256.sig) from this base URL instead of
#                          GitHub: any URL curl or wget can fetch (http:// too: your
#                          choice), or file:///<dir> for a local directory (copied, without
#                          curl or wget)
#   TWR_REQUIRE_SIGNATURE  1: fail when the signature can't be checked (no openssl that
#                          verifies Ed25519, nor P-256 when this script holds a P-256 key),
#                          except with TWR_SHA256SUMS_FILE, which the caller (twr update)
#                          has verified
#   TWR_SHA256SUMS_FILE    use this sha256sums.txt instead of downloading it; its signature
#                          is not checked here, even with TWR_REQUIRE_SIGNATURE=1: the caller
#                          (`twr update`) passes the one it verified
#   TWR_UPDATE             1 when `twr update` runs this: no first-install advice
#
# With --desktop, TWR_VERSION, TWR_DOWNLOAD_URL (which then holds desktop-sha256sums.txt and
# its signatures) and TWR_REQUIRE_SIGNATURE apply as above, and also:
#   TWR_DESKTOP_DIR        where the app goes: the folder for twr.app on macOS, for the
#                          AppImage (twr-desktop) on Linux; not used for the Arch and Debian
#                          packages
#   TWR_DESKTOP_PACKAGE    dmg, pacman, deb or appimage: install that instead of the one this
#                          machine would get (appimage: on Arch or Debian, no sudo)

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

# --- release key p256 ---
# The ECDSA P-256 public key (release/signing-key-p256.pub) of sha256sums.txt.p256.sig, checked
# where openssl can't check Ed25519 (macOS's LibreSSL), and PROBE_MESSAGE signed with it (base64
# of the DER signature). Empty: releases have no P-256 signature yet, and such an openssl checks
# nothing. docs/tools/make-signing-key.sh --p256 writes this block.
RELEASE_KEY_P256='-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEgs7mdVn04hFNq8NQp5E5e2kXhpU1
F+IC21xcMhqpPrjk83HLK81KWyvkV5xyaMYgjYhB/yNASIBqLUHWnnxLlg==
-----END PUBLIC KEY-----'
PROBE_SIGNATURE_P256='MEYCIQDMpn6V4PfMq2wJa21noPZ+cKKbg5ahWlPoM+vBaJfcnwIhAIuei/hR5QJMMHRZp/o3kSKYk9JcTdkQPpyqwXWZ4T2d'
# --- end release key p256 ---

# The staged binary next to the target, removed on every way out until it is renamed into place.
staged=""
tmp=""
# With --desktop: the directory staging twr.app (or holding the one it replaces), and the disk
# image's mount point, removed and detached on every way out.
staged_dir=""
mounted=""

cleanup() {
    if [ -n "$mounted" ]; then
        hdiutil detach "$mounted" -force > /dev/null 2>&1 || true
    fi
    if [ -n "$staged" ]; then
        rm -f "$staged"
    fi
    if [ -n "$staged_dir" ]; then
        rm -rf "$staged_dir"
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

# fetch_signature DIR [NAME]: puts NAME (default sha256sums.txt.sig) into DIR, from where
# sha256sums.txt came from.
fetch_signature() {
    name=${2:-sha256sums.txt.sig}
    if [ "$origin" = gh ]; then
        gh release download ${version:+"$version"} -R "$GH_REPO_REF" -p "$name" \
            -D "$1" --clobber > /dev/null 2>&1
    else
        download "$origin/$name" "$1/$name" 2> /dev/null
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

# has_p256_verifier: whether the P-256 block holds a key and openssl here checks ECDSA P-256
# signatures with it: it must verify the probe signed with that key (LibreSSL does).
has_p256_verifier() {
    [ -n "$RELEASE_KEY_P256" ] || return 1
    has openssl || return 1
    printf '%s\n' "$RELEASE_KEY_P256" > "$tmp/release-key-p256.pem" || return 1
    printf '%s' "$PROBE_MESSAGE" > "$tmp/probe.msg" || return 1
    p256_signature "$PROBE_SIGNATURE_P256" "$tmp/probe-p256.sig" || return 1
    openssl dgst -sha256 -verify "$tmp/release-key-p256.pem" -signature "$tmp/probe-p256.sig" \
        "$tmp/probe.msg" > /dev/null 2>&1
}

# p256_signature TEXT FILE: decodes TEXT, the base64 of a DER ECDSA P-256 signature (at most
# 72 bytes), into FILE; fails when TEXT is not that.
p256_signature() {
    case "$1" in
        "" | *[!A-Za-z0-9+/=]*) return 1 ;;
    esac
    [ "${#1}" -le 96 ] || return 1
    printf '%s' "$1" | openssl base64 -d -A > "$2" 2> /dev/null || return 1
    bytes=$(wc -c < "$2" | tr -d ' ')
    [ "$bytes" -ge 8 ] && [ "$bytes" -le 72 ]
}

# check_p256_signature DIR [LIST]: checks DIR/LIST (default sha256sums.txt) against
# LIST.p256.sig, after has_p256_verifier. A missing or bad signature stops the install, as for
# Ed25519.
check_p256_signature() {
    slist=${2:-sha256sums.txt}
    if ! fetch_signature "$1" "$slist.p256.sig"; then
        if [ "$slist" = sha256sums.txt ]; then
            err "could not download sha256sums.txt.p256.sig; nothing was installed (every twr release since the P-256 key was added is signed with it; to install an older one, use OpenSSL 3.0 or newer, see docs/install.md)"
        fi
        err "could not download $slist.p256.sig; nothing was installed (the desktop checksums of releases after v0.4.0 are signed with it; to install an older one, use OpenSSL 3.0 or newer, see docs/install.md)"
    fi
    if ! p256_signature "$(cat "$1/$slist.p256.sig")" "$1/$slist.p256.sig.bin"; then
        err "$slist.p256.sig is not an ECDSA P-256 signature; nothing was installed"
    fi
    if ! openssl dgst -sha256 -verify "$tmp/release-key-p256.pem" \
        -signature "$1/$slist.p256.sig.bin" "$1/$slist" > /dev/null 2>&1; then
        err "the P-256 signature of $slist does not verify with the release key; nothing was installed"
    fi
}

# check_signature DIR [LIST]: checks DIR/LIST (default sha256sums.txt; desktop-sha256sums.txt
# for --desktop) against the release's signature of it, when openssl here can: the Ed25519
# one, else the P-256 one when this script holds that key. A missing or bad signature always
# stops the install: every release is signed, so its files were changed or are incomplete.
check_signature() {
    slist=${2:-sha256sums.txt}
    if ! has_verifier; then
        if has_p256_verifier; then
            check_p256_signature "$1" "$slist"
            return 0
        fi
        if [ -n "$RELEASE_KEY_P256" ]; then
            if [ "${TWR_REQUIRE_SIGNATURE:-}" = 1 ]; then
                err "TWR_REQUIRE_SIGNATURE=1, but this machine has no openssl that checks Ed25519 or ECDSA P-256 signatures; nothing was installed"
            fi
            warn "the checksums were not signature-checked (no openssl that checks Ed25519 or P-256 signatures)"
            return 0
        fi
        if [ "${TWR_REQUIRE_SIGNATURE:-}" = 1 ]; then
            err "TWR_REQUIRE_SIGNATURE=1, but this machine has no openssl that checks Ed25519 signatures (OpenSSL 3.0 or newer); nothing was installed"
        fi
        warn "the checksums were not signature-checked (no openssl with Ed25519 support)"
        return 0
    fi
    if ! fetch_signature "$1" "$slist.sig"; then
        err "could not download $slist.sig; nothing was installed (every twr release is signed)"
    fi
    if ! signature "$(cat "$1/$slist.sig")" "$1/$slist.sig.bin"; then
        err "$slist.sig is not an Ed25519 signature; nothing was installed"
    fi
    if ! openssl pkeyutl -verify -rawin -pubin -inkey "$tmp/release-key.pem" \
        -sigfile "$1/$slist.sig.bin" -in "$1/$slist" > /dev/null 2>&1; then
        err "the signature of $slist does not verify with the release key; nothing was installed"
    fi
}

# check_release DIR [LIST [PREFIX]]: DIR/LIST (default sha256sums.txt) must name one release
# (`PREFIX<tag>`, default `# twr release <tag>`, signed with the rest), and that must be the
# TWR_VERSION asked for, so the signed checksums of another release are refused. Sets release.
check_release() {
    slist=${2:-sha256sums.txt}
    prefix=${3:-# twr release }
    release=$(awk -v p="$prefix" 'index($0, p) == 1 { n++; t = substr($0, length(p) + 1) } END { if (n == 1) print t }' "$1/$slist")
    case "$release" in
        v[0-9]*) ;;
        *) err "$slist does not name one release (a '$prefix<tag>' line); nothing was installed" ;;
    esac
    if [ -n "$version" ] && [ "$release" != "$version" ]; then
        err "$slist is the checksums of $release, not of $version; nothing was installed"
    fi
}

# verify DIR ASSET [LIST]: checks DIR/ASSET against DIR/LIST (default sha256sums.txt).
verify() {
    slist=${3:-sha256sums.txt}
    expected=$(awk -v f="$2" '$2 == f || $2 == "*" f { print $1; exit }' "$1/$slist")
    [ -n "$expected" ] || err "$2 is not listed in $slist"
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

# --- desktop app (--desktop) ---

# desktop_kind: what --desktop installs here: dmg (macOS), pacman (Arch), deb (Debian, Ubuntu)
# or appimage (other Linux), or TWR_DESKTOP_PACKAGE.
desktop_kind() {
    case "${TWR_DESKTOP_PACKAGE:-}" in
        dmg | pacman | deb | appimage)
            printf '%s\n' "$TWR_DESKTOP_PACKAGE"
            return
            ;;
        "") ;;
        *) err "TWR_DESKTOP_PACKAGE=$TWR_DESKTOP_PACKAGE is not dmg, pacman, deb or appimage" ;;
    esac
    case "$target" in
        aarch64-apple-darwin) echo dmg ;;
        x86_64-apple-darwin) err "the desktop app is built for Apple silicon Macs only; on an Intel Mac, run this script without --desktop for the twr CLI" ;;
        x86_64-unknown-linux-musl)
            if has pacman && is_arch; then
                echo pacman
            elif has dpkg && has apt-get; then
                echo deb
            else
                echo appimage
            fi
            ;;
        *) err "the desktop app is built for x86_64 Linux and Apple silicon Macs, not $target; run this script without --desktop for the twr CLI" ;;
    esac
}

# is_arch: whether this is Arch Linux or a distribution built on it (Omarchy, EndeavourOS...).
is_arch() {
    [ -f /etc/arch-release ] && return 0
    [ -r /etc/os-release ] || return 1
    grep -Eq '^(ID|ID_LIKE)="?([^"]* )?arch( [^"]*)?"?$' /etc/os-release
}

# fetch_desktop_sums DIR: puts the release's desktop-sha256sums.txt into DIR, from where fetch
# would get the CLI's files: TWR_DOWNLOAD_URL, else gh when it is signed in, else GitHub.
fetch_desktop_sums() {
    if [ -n "${TWR_DOWNLOAD_URL:-}" ]; then
        origin=${TWR_DOWNLOAD_URL%/}
        strict=""
        download "$origin/desktop-sha256sums.txt" "$1/desktop-sha256sums.txt" ||
            err "could not download desktop-sha256sums.txt from $origin"
        return 0
    fi
    if has gh && gh auth status --hostname github.com > /dev/null 2>&1; then
        if gh release download ${version:+"$version"} -R "$GH_REPO_REF" -p desktop-sha256sums.txt \
            -D "$1" --clobber > /dev/null 2>&1; then
            origin=gh
            return 0
        fi
        say "gh could not download desktop-sha256sums.txt; trying without it"
    fi
    if [ -n "$version" ]; then
        origin="https://github.com/$REPO/releases/download/$version"
    else
        origin="https://github.com/$REPO/releases/latest/download"
    fi
    download "$origin/desktop-sha256sums.txt" "$1/desktop-sha256sums.txt" 2> /dev/null ||
        err "could not download desktop-sha256sums.txt from $origin: that release has no desktop app (prereleases don't), or its installers are still being attached (try again in a few minutes)"
}

# fetch_desktop ASSET DIR: puts ASSET of the release that desktop-sha256sums.txt named (version)
# into DIR, from where that list came from.
fetch_desktop() {
    case "$origin" in
        gh)
            if ! gh_with_bar "$1" "$2" && ! gh release download "$version" -R "$GH_REPO_REF" -p "$1" \
                -D "$2" --clobber > /dev/null 2>&1; then
                err "could not download $1 with gh"
            fi
            ;;
        *) download "$origin/$1" "$2/$1" bar || err "could not download $1 from $origin" ;;
    esac
}

# with_terminal: whether a command can put its questions (and sudo its password prompt) to the
# terminal. This script may be reading itself from stdin (curl | sh), which they must not read.
with_terminal() {
    (: < /dev/tty) 2> /dev/null
}

# as_root CMD...: runs CMD as root (through sudo unless this is root), reading the terminal, or
# nothing when there is none.
as_root() {
    if [ "$(id -u)" != 0 ]; then
        has sudo || err "installing the package needs root, and sudo is not installed; run this as root, or set TWR_DESKTOP_PACKAGE=appimage for the AppImage"
        set -- sudo "$@"
    fi
    if with_terminal; then
        "$@" < /dev/tty
    else
        "$@" < /dev/null
    fi
}

# desktop_dir DEFAULT: TWR_DESKTOP_DIR or DEFAULT, as an absolute path.
desktop_dir() {
    d=${TWR_DESKTOP_DIR:-$1}
    case "$d" in
        /*) ;;
        "~"*) err "TWR_DESKTOP_DIR=$d starts with ~, which the shell did not expand; use \$HOME instead" ;;
        *)
            here=$(pwd) || err "could not read the current directory for TWR_DESKTOP_DIR=$d"
            d="${here%/}/$d"
            ;;
    esac
    printf '%s\n' "${d%/}"
}

# install_dmg DMG VERSION: copies twr.app from the disk image into the Applications folder,
# replacing the one there once the new one is in place beside it. curl, wget and gh set no
# quarantine flag, so Gatekeeper doesn't stop the app's first launch.
install_dmg() {
    has hdiutil || err "the disk image needs hdiutil (macOS)"
    if [ -n "${TWR_DESKTOP_DIR:-}" ]; then
        apps=$(desktop_dir "")
    elif [ -w /Applications ]; then
        apps=/Applications
    else
        [ -n "${HOME:-}" ] || err "HOME is not set; set TWR_DESKTOP_DIR to where twr.app should go"
        apps="$HOME/Applications"
    fi
    app="$apps/twr.app"
    # A command line that starts with the app's executable: a fixed-string match, which pgrep
    # (a regular expression) can't do, and not this check's own command line.
    if ps -axo command= 2> /dev/null |
        awk -v p="$app/Contents/MacOS/" 'index($0, p) == 1 { found = 1 } END { exit !found }'; then
        err "twr is running from $app; quit it (twr › Quit twr), then run this again. Nothing was installed"
    fi
    mkdir -p "$apps" || err "could not create $apps"
    mounted="$tmp/mnt"
    mkdir "$mounted"
    # Its messages only when it fails: macOS 27 warns on every attach that the flags are deprecated.
    if ! hdiutil attach -nobrowse -readonly -noautoopen -mountpoint "$mounted" "$1" \
        > /dev/null 2> "$tmp/hdiutil.log" < /dev/null; then
        mounted=""
        cat "$tmp/hdiutil.log" >&2
        err "could not open $(basename "$1")"
    fi
    [ -d "$mounted/twr.app" ] || err "$(basename "$1") holds no twr.app"
    staged_dir=$(mktemp -d "$apps/.twr.app.XXXXXX") || err "could not write to $apps"
    ditto "$mounted/twr.app" "$staged_dir/twr.app" || err "could not copy twr.app to $apps"
    hdiutil detach "$mounted" > /dev/null 2>&1 || true
    mounted=""
    if has codesign && ! codesign --verify --deep --strict "$staged_dir/twr.app" 2> /dev/null; then
        err "the code signature of twr.app in $(basename "$1") does not verify; nothing was installed"
    fi
    if [ -e "$app" ]; then
        mv "$app" "$staged_dir/twr.app.old" || err "could not replace $app"
    fi
    if ! mv "$staged_dir/twr.app" "$app"; then
        if [ -e "$staged_dir/twr.app.old" ]; then
            mv "$staged_dir/twr.app.old" "$app" || true
        fi
        err "could not install $app"
    fi
    rm -rf "$staged_dir"
    staged_dir=""
    say "Installed twr $2 to $app"
    say ""
    say "Open it from Launchpad or Spotlight, or with: open '$app'"
    if has codesign && codesign --display --verbose=2 "$app" 2>&1 | grep -q '^Authority=Developer ID Application: '; then
        say "It updates itself."
    else
        say "This build is not signed with a Developer ID, so it can't update itself: it tells you"
        say "when a new release is out, and running this command again installs it."
    fi
}

# install_pacman PACKAGE: installs the Arch package with pacman, as root.
install_pacman() {
    if pacman -Qq twr-desktop-bin > /dev/null 2>&1; then
        err "twr-desktop-bin, the AUR package, is installed: update it with your AUR helper (yay -Syu twr-desktop-bin) instead. Nothing was installed"
    fi
    say "Installing the Arch package needs root: running sudo pacman -U $(basename "$1")"
    if with_terminal; then
        as_root pacman -U --needed "$1" || err "pacman did not install $(basename "$1")"
    else
        as_root pacman -U --needed --noconfirm "$1" || err "pacman did not install $(basename "$1")"
    fi
    say ""
    say "Installed twr $2. Open twr from your app launcher, or run twr-desktop."
    say "It tells you when a new release is out; run this command again to install it."
}

# install_deb PACKAGE: installs the Debian package with apt-get, as root, which installs what it
# depends on too.
install_deb() {
    # apt-get reads the package as its own unprivileged user (_apt): give it a way in.
    mkdir "$tmp/deb"
    cp "$1" "$tmp/deb/" || err "could not stage $(basename "$1")"
    chmod 711 "$tmp"
    chmod 755 "$tmp/deb"
    chmod 644 "$tmp/deb/$(basename "$1")"
    say "Installing the Debian package needs root: running sudo apt-get install $(basename "$1")"
    if with_terminal; then
        as_root apt-get install "$tmp/deb/$(basename "$1")" || err "apt-get did not install $(basename "$1")"
    else
        as_root apt-get install -y "$tmp/deb/$(basename "$1")" || err "apt-get did not install $(basename "$1")"
    fi
    say ""
    say "Installed twr $2. Open twr from your app launcher, or run twr-desktop."
    say "It tells you when a new release is out; run this command again to install it."
}

# desktop_exec PATH: PATH as one Exec argument of a desktop entry: double-quoted with `"`, `` ` ``,
# `$` and `\` escaped, then `\` doubled (Exec is a string value) and `%` doubled (field codes),
# as clients/desktop/autostart.ts writes it.
desktop_exec() {
    q=$(printf '%s' "$1" | sed 's/[\\"`$]/\\&/g')
    printf '"%s"\n' "$q" | sed -e 's/\\/\\\\/g' -e 's/%/%%/g'
}

# install_appimage APPIMAGE VERSION: puts the AppImage at TWR_DESKTOP_DIR/twr-desktop (default
# ~/.local/bin), where it replaces itself on updates, with a launcher entry and its icon.
install_appimage() {
    [ -n "${TWR_DESKTOP_DIR:-}" ] || [ -n "${HOME:-}" ] ||
        err "HOME is not set; set TWR_DESKTOP_DIR to where the AppImage should go"
    bin=$(desktop_dir "${HOME:-}/.local/bin")
    case "$bin" in
        *"
"*) err "TWR_DESKTOP_DIR=$bin has a line break, which a launcher entry can't hold" ;;
    esac
    mkdir -p "$bin" || err "could not create $bin"
    if [ -d "$bin/twr-desktop" ]; then
        err "$bin/twr-desktop is a directory; move it away or set TWR_DESKTOP_DIR to another place"
    fi
    staged=$(mktemp "$bin/.twr-desktop.new.XXXXXX") || err "could not write to $bin"
    cp "$1" "$staged" || err "could not write to $bin"
    chmod 755 "$staged" || err "could not make $staged executable"
    mv -f "$staged" "$bin/twr-desktop" || err "could not install $bin/twr-desktop"
    staged=""
    say "Installed twr $2 to $bin/twr-desktop"

    data=${XDG_DATA_HOME:-${HOME:-}/.local/share}
    case "$data" in
        /*) ;;
        *) return 0 ;;
    esac
    # The AppImage's own icon, extracted by its runtime (no FUSE needed). Best effort: without
    # it the launcher shows a generic icon.
    mkdir "$tmp/icon"
    if (cd "$tmp/icon" && "$bin/twr-desktop" --appimage-extract 'usr/share/icons/hicolor/*' > /dev/null 2>&1); then
        icon=$(find "$tmp/icon/squashfs-root/usr/share/icons/hicolor" -type f -name 'twr-desktop.png' 2> /dev/null | sort | tail -n 1)
        if [ -n "$icon" ] && mkdir -p "$data/icons/hicolor/512x512/apps" &&
            cp "$icon" "$data/icons/hicolor/512x512/apps/tools.twr.desktop.png"; then
            :
        else
            warn "could not install the app's icon; the launcher shows a generic one"
        fi
    fi
    entry="$data/applications/tools.twr.desktop.desktop"
    if [ -e "$entry" ] && ! grep -qx 'X-twr-Installer=install.sh' "$entry" 2> /dev/null; then
        warn "$entry was not written by this script: left as it is"
    elif mkdir -p "$data/applications" && {
        printf '%s\n' "[Desktop Entry]" "Name=twr" "Comment=One conversation for all your coding agents"
        printf 'Exec=%s %%U\n' "$(desktop_exec "$bin/twr-desktop")"
        printf '%s\n' "Icon=tools.twr.desktop" "Terminal=false" "Type=Application" "Categories=Development;" \
            "Keywords=agents;coding;terminal;" "StartupWMClass=tools.twr.desktop" "X-twr-Installer=install.sh"
    } > "$entry.new" && mv -f "$entry.new" "$entry"; then
        if has update-desktop-database; then
            update-desktop-database "$data/applications" > /dev/null 2>&1 || true
        fi
        say "Added twr to your app launcher ($entry)"
    else
        rm -f "$entry.new"
        warn "could not write $entry; start twr with $bin/twr-desktop"
    fi
    if ! { ldconfig -p 2> /dev/null || /sbin/ldconfig -p 2> /dev/null; } | grep -q 'libfuse\.so\.2'; then
        say ""
        say "AppImages need FUSE 2 (libfuse.so.2), which this machine doesn't seem to have. Install"
        say "it with your package manager: fuse2 (Arch), libfuse2t64 or libfuse2 (Debian, Ubuntu),"
        say "fuse-libs (Fedora)."
    fi
    say ""
    say "It updates itself."
}

# install_desktop: --desktop.
install_desktop() {
    kind=$(desktop_kind)
    trap cleanup EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 141' PIPE
    trap 'exit 143' TERM
    tmp=$(mktemp -d 2> /dev/null || mktemp -d -t twr) || err "could not create a temporary directory"

    say "Installing the twr desktop app ${version:-(latest release)} ($kind)..."
    fetch_desktop_sums "$tmp"
    check_signature "$tmp" desktop-sha256sums.txt
    check_release "$tmp" desktop-sha256sums.txt "# twr desktop release "
    # Every other file comes from the release the signed list names, even when a newer one is
    # published meanwhile.
    version=$release
    case "$origin" in
        */releases/latest/download) origin="https://github.com/$REPO/releases/download/$release" ;;
    esac
    v=${release#v}
    case "$kind" in
        dmg) asset="twr-$v-arm64.dmg" ;;
        pacman) asset="twr-desktop-$v-1-x86_64.pkg.tar.zst" ;;
        deb) asset="twr-desktop_${v}_amd64.deb" ;;
        appimage) asset="twr-$v.AppImage" ;;
    esac
    awk -v f="$asset" '$2 == f || $2 == "*" f { found = 1 } END { exit !found }' "$tmp/desktop-sha256sums.txt" ||
        err "$release has no $asset (desktop-sha256sums.txt doesn't list it); nothing was installed"
    fetch_desktop "$asset" "$tmp"
    verify "$tmp" "$asset" desktop-sha256sums.txt
    case "$kind" in
        dmg) install_dmg "$tmp/$asset" "$v" ;;
        pacman) install_pacman "$tmp/$asset" "$v" ;;
        deb) install_deb "$tmp/$asset" "$v" ;;
        appimage) install_appimage "$tmp/$asset" "$v" ;;
    esac
    say ""
    say "The app runs its own twr engine. For the twr command in your terminal too, run this"
    say "script again without --desktop."
}

main() {
    pair_after_install=0
    desktop=0
    case "${1:-}" in
        --pair) pair_after_install=1; shift ;;
        --desktop) desktop=1; shift ;;
        "") ;;
        *) err "usage: install.sh [--pair | --desktop]" ;;
    esac
    [ "$#" -eq 0 ] || err "usage: install.sh [--pair | --desktop]"
    target=$(detect_target)
    if [ "$desktop" = 1 ]; then
        version=${TWR_VERSION:-}
        case "$version" in
            "" | latest) version="" ;;
            v*) ;;
            *) version="v$version" ;;
        esac
        install_desktop
        return
    fi
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
    if [ "$pair_after_install" = 1 ]; then
        # This script comes from main, which can be ahead of the latest release: v0.3.0 and
        # older have no `twr runner up`. The message names no version, so it stays true once
        # a release has it (then only an older TWR_VERSION gets here).
        if ! "$dir/twr" runner up --help > /dev/null 2>&1; then
            err "installed $installed at $dir/twr, but it is too old to pair: it has no \`twr runner up\`, which --pair runs (this script can be ahead of the release it installs). To pair this machine, install a twr that has it: run this script again with --pair once a newer release is out (without TWR_VERSION it takes the latest), or build twr from source (https://github.com/$REPO/blob/main/docs/install.md#from-source) and run \`twr runner up\`. The hub you pair with needs \`twr box pair\`, from a twr as new."
        fi
        "$dir/twr" runner up
        return
    fi

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
