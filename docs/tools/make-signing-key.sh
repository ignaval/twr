#!/bin/sh
# Makes the Ed25519 key pair the releases are signed with:
#
#   docs/tools/make-signing-key.sh [dir]
#
# The private key goes to DIR/signing-key.pem (default: ~/.config/twr-release), in a directory
# only you can read; an existing key is never replaced. The public key goes to
# release/signing-key.pub, and install.sh's release key block (between `# --- release key ---`
# and `# --- end release key ---`) gets the public key and a fixed message signed with the new
# key, which install.sh verifies to find out whether the openssl at hand can check Ed25519
# signatures. Commit both files. A run that fails before both are written removes the key it
# made: run it again.
#
# It prints the public key and the command that stores the private key as the `release`
# environment's secret, which the release workflow signs with. It never prints the private
# key. Needs OpenSSL 3.0 or newer (`pkeyutl -rawin`).
#
#   docs/tools/make-signing-key.sh --p256 [dir]
#
# makes the second, ECDSA P-256 key pair the same way, for the openssl that can't check Ed25519
# (macOS's LibreSSL): DIR/signing-key-p256.pem, release/signing-key-p256.pub and install.sh's
# P-256 block (`# --- release key p256 ---`). The release workflow signs sha256sums.txt with it
# too (sha256sums.txt.p256.sig) once the secret it prints is set. Any openssl does, LibreSSL
# included. docs/development.md ("A P-256 release key") has the steps.

set -eu

repo="ignaval/twr"
probe_message="twr signature probe"

mode=ed25519
if [ "${1:-}" = --p256 ]; then
    mode=p256
    shift
fi
root=$(cd "$(dirname "$0")/../.." && pwd)
if [ -z "${1:-}" ] && [ -z "${HOME:-}" ]; then
    echo "make-signing-key: HOME is not set; name the key's directory" >&2
    exit 1
fi
dir=${1:-$HOME/.config/twr-release}
case "$dir" in
    /*) ;;
    *) dir="$(pwd)/$dir" ;;
esac
if [ "$mode" = p256 ]; then
    key="$dir/signing-key-p256.pem"
    pub="$root/release/signing-key-p256.pub"
    begin="# --- release key p256 ---"
    end="# --- end release key p256 ---"
    secret=TWR_RELEASE_SIGNING_KEY_P256
else
    key="$dir/signing-key.pem"
    pub="$root/release/signing-key.pub"
    begin="# --- release key ---"
    end="# --- end release key ---"
    secret=TWR_RELEASE_SIGNING_KEY
fi
installer="$root/install.sh"

err() {
    printf 'make-signing-key: %s\n' "$*" >&2
    exit 1
}

# sh_word S: S as one shell word, single-quoted (`'` as `'\''`), to paste as shown.
sh_word() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

tmp=""
fresh=""
made=""
cleanup() {
    if [ -n "$tmp" ]; then
        rm -rf "$tmp"
    fi
    if [ -n "$fresh" ]; then
        rm -f "$fresh"
    fi
    # A key the repository does not hold yet (nothing else used it) is not left behind: a run
    # again makes a new one and writes both files for it.
    if [ -n "$made" ]; then
        rm -f "$key"
    fi
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 141' PIPE
trap 'exit 143' TERM

# made_err PROBLEM: fails after the new key was made: it is removed on the way out.
made_err() {
    err "$*; the new key $key was removed: run this again"
}

if [ "$mode" = p256 ]; then
    command -v openssl > /dev/null 2>&1 || err "needs openssl"
else
    command -v openssl > /dev/null 2>&1 || err "needs openssl (3.0 or newer)"
fi
starts=$(grep -cxF "$begin" "$installer" || true)
ends=$(grep -cxF "$end" "$installer" || true)
if [ "$starts" != 1 ] || [ "$ends" != 1 ]; then
    err "$installer has no single release key block ($begin ... $end)"
fi

umask 077
mkdir -p "$dir" || err "could not create $dir"
chmod 700 "$dir" || err "could not make $dir private"
exists="$key already exists; move it away first (a new key also needs a new public key in the repository and a new secret)"
if [ -e "$key" ] || [ -L "$key" ]; then
    err "$exists"
fi
tmp=$(mktemp -d) || err "could not create a temporary directory"

# Made in a new file of its own, then linked to its name: the link fails when anything is
# there (a file, a dangling symlink, a FIFO), so the key is never written through something
# already in place.
fresh=$(mktemp "$dir/.signing-key.XXXXXX") || err "could not create a file in $dir"
if [ "$mode" = p256 ]; then
    openssl ecparam -name prime256v1 -genkey -noout > "$fresh" ||
        err "openssl could not make a P-256 key"
else
    openssl genpkey -algorithm ed25519 > "$fresh" || err "openssl could not make an Ed25519 key"
fi
ln "$fresh" "$key" 2> /dev/null || err "$exists"
made=1
rm -f "$fresh"
fresh=""
openssl pkey -in "$key" -pubout -out "$tmp/signing-key.pub" ||
    made_err "openssl could not derive the public key"

# The probe: install.sh verifies this signature before it trusts openssl to check a release.
printf '%s' "$probe_message" > "$tmp/probe.msg"
if [ "$mode" = p256 ]; then
    openssl dgst -sha256 -sign "$key" -out "$tmp/probe.sig" "$tmp/probe.msg" ||
        made_err "openssl could not sign"
    openssl dgst -sha256 -verify "$tmp/signing-key.pub" -signature "$tmp/probe.sig" \
        "$tmp/probe.msg" > /dev/null || made_err "the new key's own signature does not verify"
else
    openssl pkeyutl -sign -rawin -inkey "$key" -in "$tmp/probe.msg" -out "$tmp/probe.sig" ||
        made_err "openssl could not sign (OpenSSL 3.0 or newer is needed for pkeyutl -rawin)"
fi
probe_sig=$(openssl base64 -A -in "$tmp/probe.sig") || made_err "openssl could not encode the probe"
if [ "$mode" = ed25519 ]; then
    openssl pkeyutl -verify -rawin -pubin -inkey "$tmp/signing-key.pub" -sigfile "$tmp/probe.sig" \
        -in "$tmp/probe.msg" > /dev/null || made_err "the new key's own signature does not verify"
fi

if [ "$mode" = p256 ]; then
    {
        echo "$begin"
        echo "# The ECDSA P-256 public key (release/signing-key-p256.pub) of sha256sums.txt.p256.sig, checked"
        echo "# where openssl can't check Ed25519 (macOS's LibreSSL), and PROBE_MESSAGE signed with it (base64"
        echo "# of the DER signature). Empty: releases have no P-256 signature yet, and such an openssl checks"
        echo "# nothing. docs/tools/make-signing-key.sh --p256 writes this block."
        printf "RELEASE_KEY_P256='%s'\n" "$(cat "$tmp/signing-key.pub")"
        printf "PROBE_SIGNATURE_P256='%s'\n" "$probe_sig"
        echo "$end"
    } > "$tmp/block" || made_err "could not write the release key block"
else
    {
        echo "$begin"
        echo "# The public key releases are signed with (release/signing-key.pub), and a fixed message"
        echo "# signed with it: openssl must verify that before it is trusted to check a release."
        echo "# docs/tools/make-signing-key.sh writes this block."
        printf "RELEASE_KEY='%s'\n" "$(cat "$tmp/signing-key.pub")"
        printf "PROBE_MESSAGE='%s'\n" "$probe_message"
        printf "PROBE_SIGNATURE='%s'\n" "$probe_sig"
        echo "$end"
    } > "$tmp/block" || made_err "could not write the release key block"
fi
awk -v block="$tmp/block" -v begin="$begin" -v end="$end" '
    $0 == begin {
        while ((getline line < block) > 0) print line
        skip = 1
        next
    }
    $0 == end { skip = 0; next }
    !skip { print }
' "$installer" > "$tmp/install.sh" || made_err "could not rewrite $installer (it is unchanged)"
# Written in place, so install.sh keeps its mode.
cat "$tmp/install.sh" > "$installer" ||
    made_err "could not write $installer (it may be written in part: git checkout install.sh restores it)"
if ! { mkdir -p "$root/release" && cp "$tmp/signing-key.pub" "$pub" && chmod 644 "$pub"; }; then
    made_err "could not write $pub ($installer holds the removed key's block until then)"
fi
# The repository holds the new key's public half: the key stays.
made=""

if [ "$mode" = p256 ]; then
    echo "Made the P-256 release signing key."
else
    echo "Made the release signing key."
fi
echo
echo "Private key: $key (keep it there, and a backup offline; it is not in the repository)"
echo "Public key:  $pub, and install.sh's release key block (commit both):"
echo
cat "$pub"
echo
echo "Store the private key as the release environment's secret (needs the GitHub CLI, signed"
echo "in as a repository admin):"
echo
echo "  gh secret set $secret --env release --repo $repo < $(sh_word "$key")"
echo
if [ "$mode" = p256 ]; then
    # A P-256 SubjectPublicKeyInfo is a 26-byte header and the 65-byte uncompressed point.
    echo "For twr to check the P-256 signature too (optional: twr update checks the Ed25519 one"
    echo "itself, whatever openssl there is), its uncompressed public point is"
    echo
    openssl pkey -pubin -in "$pub" -outform DER | od -An -v -j26 -tx1 |
        awk '{ for (i = 1; i <= NF; i++) printf "0x%s,%s", $i, (++n % 8 ? " " : "\n") }'
    echo
else
    echo "twr embeds the public key too: set RELEASE_KEY in crates/twr-cli/src/main.rs to"
    echo
    openssl pkey -pubin -in "$pub" -outform DER | od -An -v -j12 -tx1 |
        awk '{ for (i = 1; i <= NF; i++) printf "0x%s,%s", $i, (++n % 8 ? " " : "\n") }'
fi
