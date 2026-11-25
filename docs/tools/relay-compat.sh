#!/bin/sh
# The relay's compatibility check (docs/relay-protocol.md §12-§13): this build's relay against
# `twr relay probe` from the last two releases, and this build's probe against those releases'
# relays. A release from before the relay existed is skipped, and the script says so.
# Everything runs on loopback.
#
#   sh docs/tools/relay-compat.sh [TWR_BINARY]
#
# TWR_BINARY is this build's twr (default target/debug/twr; build it with
# `cargo build -p twr-cli --no-default-features`). RELAY_COMPAT_TAGS overrides the releases
# (space-separated tags). Old releases are built in a temporary worktree with their own
# lockfile, in target/relay-compat.
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
head_bin=${1:-target/debug/twr}
[ -x "$head_bin" ] || { echo "no twr at $head_bin: cargo build -p twr-cli --no-default-features" >&2; exit 2; }
head_bin=$(cd "$(dirname "$head_bin")" && pwd)/$(basename "$head_bin")

tags=${RELAY_COMPAT_TAGS:-$(git tag --list 'v[0-9]*' --sort=-v:refname | grep -v -- '-' | head -n 2)}
[ -n "$tags" ] || { echo "no release tags here (fetch them: git fetch --tags)" >&2; exit 2; }

work=$(mktemp -d "${TMPDIR:-/tmp}/twr-relay-compat.XXXXXXXX")
cleanup() {
    if [ -f "$work/pids" ]; then
        while read -r p; do kill "$p" 2>/dev/null || true; done < "$work/pids"
        # Wait for them: a relay still exiting writes to its state while it goes.
        while read -r p; do wait "$p" 2>/dev/null || true; done < "$work/pids"
    fi
    for wt in "$work"/src-*; do
        [ -d "$wt" ] && git worktree remove --force "$wt" >/dev/null 2>&1
    done
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

# Starts a relay from binary $1 (named $2) on loopback port $3; its relay URL goes to
# $work/relay-$2/url.
start_relay() {
    bin=$1
    tag=$2
    port=$3
    dir="$work/relay-$tag"
    mkdir -p "$dir"
    cat > "$dir/relay.toml" <<EOF
[relay]
public_host = "127.0.0.1"
listen = ["127.0.0.1:$port"]
state_dir = "$dir/state"
metrics_listen = "127.0.0.1:$((port + 1))"
region = "compat"
[tls]
mode = "self-signed"
[iroh]
listen = "127.0.0.1:$((port + 2))"
quic_listen = "127.0.0.1:$((port + 3))"
[limits]
exempt_addresses = ["127.0.0.0/8"]
EOF
    "$bin" relay serve --config "$dir/relay.toml" > "$dir/serve.log" 2>&1 &
    echo "$!" >> "$work/pids"
    i=0
    until [ -s "$dir/state/relay-key.pem" ] && grep -q listening "$dir/serve.log"; do
        i=$((i + 1))
        if [ "$i" -gt 100 ]; then
            echo "the $tag relay didn't start:" >&2
            cat "$dir/serve.log" >&2
            return 1
        fi
        sleep 0.1
    done
    "$bin" relay url --config "$dir/relay.toml" > "$dir/url"
}

fail=0
checked=0
start_relay "$head_bin" head 28443
head_url=$(cat "$work/relay-head/url")
port=28453
for tag in $tags; do
    if ! git cat-file -e "$tag:crates/twr-relay/Cargo.toml" 2>/dev/null; then
        echo "skip $tag: it has no relay (the relay is newer)"
        continue
    fi
    wt="$work/src-$tag"
    git worktree add --detach "$wt" "$tag" >/dev/null 2>&1
    echo "building twr $tag ..."
    (cd "$wt" && CARGO_TARGET_DIR="$root/target/relay-compat" cargo build -q --locked -p twr-cli --no-default-features)
    old_bin="$work/twr-$tag"
    cp "$root/target/relay-compat/debug/twr" "$old_bin"

    echo "== $tag probe -> this relay"
    if "$old_bin" relay probe "$head_url" --state "$work/probe-$tag"; then checked=$((checked + 1)); else fail=1; fi
    start_relay "$old_bin" "$tag" "$port"
    port=$((port + 10))
    old_url=$(cat "$work/relay-$tag/url")
    echo "== this probe -> $tag relay"
    if "$head_bin" relay probe "$old_url" --state "$work/probe-head-$tag"; then checked=$((checked + 1)); else fail=1; fi
done

echo "== this probe -> this relay"
"$head_bin" relay probe "$head_url" --state "$work/probe-head" || fail=1

if [ "$fail" != 0 ]; then
    echo "relay compatibility: FAILED" >&2
    exit 1
fi
echo "relay compatibility: ok ($checked cross-release checks; releases: $(printf '%s\n' "$tags" | tr '\n' ' '))"
