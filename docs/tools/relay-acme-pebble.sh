#!/bin/sh
# Runs the relay's ACME test (crates/twr-relay/tests/acme.rs) against pebble, Let's Encrypt's
# ACME test server, in Docker on loopback. Nothing here contacts a real ACME service: pebble
# issues from its own throwaway CA, and validates tls-alpn-01 by connecting to 127.0.0.1.
#
#   sh docs/tools/relay-acme-pebble.sh
#
# Needs Docker.
set -eu

# Pinned by digest (the image of 2026-11-23); PEBBLE_IMAGE overrides it.
image=${PEBBLE_IMAGE:-ghcr.io/letsencrypt/pebble@sha256:0c81f05db371f436c97fb894bd6d2ab8fc9c8e2ce40f8bef8a8e15941bcd75ef}
port=${TWR_PEBBLE_PORT:-18444}
name=twr-relay-pebble-$$
work=$(mktemp -d "${TMPDIR:-/tmp}/twr-relay-pebble.XXXXXXXX")
cleanup() {
    docker rm -f "$name" >/dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

cat > "$work/pebble.json" <<EOF
{
  "pebble": {
    "listenAddress": "127.0.0.1:14000",
    "managementListenAddress": "127.0.0.1:15000",
    "certificate": "test/certs/localhost/cert.pem",
    "privateKey": "test/certs/localhost/key.pem",
    "httpPort": 5002,
    "tlsPort": $port,
    "ocspResponderURL": "",
    "externalAccountBindingRequired": false
  }
}
EOF

# Host networking, so pebble validates on 127.0.0.1 where the test's relay listens; the two
# names resolve there through the container's /etc/hosts.
docker run -d --name "$name" --network host \
    --add-host relay.test:127.0.0.1 --add-host iroh.relay.test:127.0.0.1 \
    -e PEBBLE_VA_NOSLEEP=1 -e PEBBLE_WFE_NONCEREJECT=0 \
    -v "$work/pebble.json:/config.json:ro" \
    "$image" -config /config.json >/dev/null

docker cp "$name:/test/certs/pebble.minica.pem" "$work/root.pem" >/dev/null 2>&1 ||
    docker cp "$name:/go/src/github.com/letsencrypt/pebble/test/certs/pebble.minica.pem" "$work/root.pem"

i=0
until curl -sf --cacert "$work/root.pem" https://127.0.0.1:15000/roots/0 -o "$work/issuer.pem"; do
    i=$((i + 1))
    if [ "$i" -gt 50 ]; then
        echo "pebble didn't start" >&2
        docker logs "$name" >&2
        exit 1
    fi
    sleep 0.2
done

TWR_PEBBLE_DIRECTORY=https://127.0.0.1:14000/dir \
TWR_PEBBLE_ROOT="$work/root.pem" \
TWR_PEBBLE_ISSUER="$work/issuer.pem" \
TWR_PEBBLE_PORT=$port \
    cargo test -p twr-relay --features server --test acme -- --ignored --nocapture
