# Test signing key

A throwaway Ed25519 key pair for the tests of `twr update` and install.sh's signature checks.
It is NOT the release key (that one is `release/signing-key.pub`, and its private half is in
no repository): nothing signed with this key is trusted outside the tests, and the private key
is here on purpose.

- `test-key.pem`: the private key (PKCS#8, from `openssl genpkey -algorithm ed25519`)
- `test-key.pub`: its public key (`openssl pkey -in test-key.pem -pubout`)
- `probe.sig`: base64 of the signature of the message `twr signature probe` (no newline),
  made by `openssl pkeyutl -sign -rawin -inkey test-key.pem`: the tests check that ring
  verifies a signature openssl made, and use it as install.sh's probe for the test key
