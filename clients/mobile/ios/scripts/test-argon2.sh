#!/usr/bin/env bash
# Builds the iOS app's vendored Argon2id (App/TwrArgon2) with the host C compiler and checks
# it against the phone-pairing test vector, then checks the rest of the proof derivation
# (HKDF-SHA256, HMAC-SHA256, the pin) from that output with Python's standard library.
# Runs anywhere with cc and python3; no Xcode needed.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
package="$here/../App/TwrArgon2"
sources="$package/Sources/TwrArgon2"
fixture="$here/../../../shared/fixtures/phone-pairing-v2.json"
cc="${CC:-cc}"

build="$(mktemp -d)"
trap 'rm -rf "$build"' EXIT

"$cc" -std=c99 -O2 -Wall -Wextra -Wno-type-limits -DARGON2_NO_THREADS \
  -I "$sources/include" -I "$sources/argon2/include" -I "$sources/argon2/src" \
  "$package/test/argon2_vector_test.c" "$sources/twr_argon2.c" \
  "$sources/argon2/src/argon2.c" "$sources/argon2/src/core.c" "$sources/argon2/src/encoding.c" \
  "$sources/argon2/src/ref.c" "$sources/argon2/src/blake2/blake2b.c" \
  -o "$build/argon2_vector_test"

# One value per line: code, salt, t, m, p, stretched_hex.
mapfile -t vector < <(python3 -I - "$fixture" <<'PY'
import json, sys
f = json.load(open(sys.argv[1]))
a = f["argon2id"]
assert a["len"] == 32
for value in (f["code"], a["salt"], a["t"], a["m_kib"], a["p"], f["stretched_hex"]):
    print(value)
PY
)

stretched="$("$build/argon2_vector_test" "${vector[@]}")"
echo "Argon2id(${vector[0]}) = $stretched: matches the fixture's stretched_hex"

python3 -I - "$fixture" "$stretched" <<'PY'
import base64, hashlib, hmac, json, sys
f = json.load(open(sys.argv[1]))
stretched = bytes.fromhex(sys.argv[2])

def b64url(raw):
    return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()

def unb64url(text):
    return base64.urlsafe_b64decode(text + "=" * (-len(text) % 4))

salt = f["hkdf"]["salt"].encode()
prk = hmac.new(salt, stretched, hashlib.sha256).digest()
key = hmac.new(prk, f["hkdf"]["info"].encode() + b"\x01", hashlib.sha256).digest()
assert key.hex() == f["proof_key_hex"], ("proof key", key.hex(), f["proof_key_hex"])

spki = bytes.fromhex(f["spki_hex"])
p256_prefix = bytes.fromhex("3059301306072a8648ce3d020106082a8648ce3d030107034200")
assert len(spki) == 91 and spki[:26] == p256_prefix and spki[26] == 4, "P-256 SPKI layout"
pin = hashlib.sha256(spki).digest()
assert b64url(pin) == f["pin"], ("pin", b64url(pin), f["pin"])

nonce = unb64url(f["nonce_b64url"])
assert len(nonce) == 32
message = b"twr-mobile-pin-v2\x00" + pin + nonce + f["gateway_id"].encode()
proof = hmac.new(key, message, hashlib.sha256).digest()
assert b64url(proof) == f["proof_b64url"], ("proof", b64url(proof), f["proof_b64url"])
print("HKDF proof key, pin and proof match the fixture")
PY
