#!/bin/sh
# Pairs the desktop's pinned transport with a real `twr gateway` (tests/real-gateway.test.ts),
# on loopback, in a disposable twr home (removed afterwards when the script made it). Never touches
# your own hub: TWR_HOME points at the scratch directory, and the gateway listens on
# 127.0.0.1 only.
#
#   clients/scripts/real-gateway-qa.sh [scratch-dir]      (default: a new mktemp -d)
#
# TWR_BIN picks the twr to test (default target/debug/twr). From inside a twr thread, run it
# under `systemd-run --user --wait --pipe --collect` with TWR_AGENT, TWR_THREAD,
# TWR_RUNTIME_GEN and TWR_WORKSPACE unset, as docs/development.md says for manual QA.
set -eu
root=$(cd "$(dirname "$0")/../.." && pwd)
twr=${TWR_BIN:-$root/target/debug/twr}
made=
if [ -n "${1:-}" ]; then home=$1; else home=$(mktemp -d); made=1; fi
port=${TWR_QA_PORT:-47991}
mkdir -p "$home"
export TWR_HOME="$home" TWR_GATEWAY_AUTOSTART=off
gw=
cleanup() {
    [ -n "$gw" ] && kill "$gw" 2>/dev/null || true
    "$twr" hub stop >/dev/null 2>&1 || true
    [ -n "$made" ] && rm -rf "$home" || true
}
trap cleanup EXIT INT TERM
"$twr" ws list >/dev/null
"$twr" gateway --listen "127.0.0.1:$port" >"$home/gateway.out" 2>&1 &
gw=$!
i=0
until "$twr" gateway --gateway-info 2>/dev/null | grep -q '"running":true'; do
    i=$((i + 1))
    if [ "$i" -gt 150 ]; then
        echo "the gateway didn't start:" >&2
        cat "$home/gateway.out" >&2
        exit 1
    fi
    sleep 0.2
done
cd "$root/clients"
TWR_REAL_GATEWAY_PAIR="'$twr' gateway --pair --pair-only --json" \
    TWR_REAL_GATEWAY_ADDRESS="127.0.0.1:$port" \
    npx vitest run tests/real-gateway.test.ts
