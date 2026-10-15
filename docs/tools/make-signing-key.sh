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

set -eu

repo="ignaval/twr"
probe_message="twr signature probe"

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
key="$dir/signing-key.pem"
pub="$root/release/signing-key.pub"
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

command -v openssl > /dev/null 2>&1 || err "needs openssl (3.0 or newer)"
starts=$(grep -c '^# --- release key ---$' "$installer" || true)
ends=$(grep -c '^# --- end release key ---$' "$installer" || true)
if [ "$starts" != 1 ] || [ "$ends" != 1 ]; then
    err "$installer has no single release key block (# --- release key --- ... # --- end release key ---)"
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
openssl genpkey -algorithm ed25519 > "$fresh" || err "openssl could not make an Ed25519 key"
ln "$fresh" "$key" 2> /dev/null || err "$exists"
made=1
rm -f "$fresh"
fresh=""
openssl pkey -in "$key" -pubout -out "$tmp/signing-key.pub" ||
    made_err "openssl could not derive the public key"

# The probe: install.sh verifies this signature before it trusts openssl to check a release.
printf '%s' "$probe_message" > "$tmp/probe.msg"
openssl pkeyutl -sign -rawin -inkey "$key" -in "$tmp/probe.msg" -out "$tmp/probe.sig" ||
    made_err "openssl could not sign (OpenSSL 3.0 or newer is needed for pkeyutl -rawin)"
probe_sig=$(openssl base64 -A -in "$tmp/probe.sig") || made_err "openssl could not encode the probe"
openssl pkeyutl -verify -rawin -pubin -inkey "$tmp/signing-key.pub" -sigfile "$tmp/probe.sig" \
    -in "$tmp/probe.msg" > /dev/null || made_err "the new key's own signature does not verify"

{
    echo "# --- release key ---"
    echo "# The public key releases are signed with (release/signing-key.pub), and a fixed message"
    echo "# signed with it: openssl must verify that before it is trusted to check a release."
    echo "# docs/tools/make-signing-key.sh writes this block."
    printf "RELEASE_KEY='%s'\n" "$(cat "$tmp/signing-key.pub")"
    printf "PROBE_MESSAGE='%s'\n" "$probe_message"
    printf "PROBE_SIGNATURE='%s'\n" "$probe_sig"
    echo "# --- end release key ---"
} > "$tmp/block" || made_err "could not write the release key block"
awk -v block="$tmp/block" '
    $0 == "# --- release key ---" {
        while ((getline line < block) > 0) print line
        skip = 1
        next
    }
    $0 == "# --- end release key ---" { skip = 0; next }
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

echo "Made the release signing key."
echo
echo "Private key: $key (keep it there, and a backup offline; it is not in the repository)"
echo "Public key:  $pub, and install.sh's release key block (commit both):"
echo
cat "$pub"
echo
echo "Store the private key as the release environment's secret (needs the GitHub CLI, signed"
echo "in as a repository admin):"
echo
echo "  gh secret set TWR_RELEASE_SIGNING_KEY --env release --repo $repo < $(sh_word "$key")"
echo
echo "twr embeds the public key too: set RELEASE_KEY in crates/twr-cli/src/main.rs to"
echo
openssl pkey -pubin -in "$pub" -outform DER | od -An -v -j12 -tx1 |
    awk '{ for (i = 1; i <= NF; i++) printf "0x%s,%s", $i, (++n % 8 ? " " : "\n") }'
