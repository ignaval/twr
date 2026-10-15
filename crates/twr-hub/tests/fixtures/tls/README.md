# Test TLS fixtures

A throwaway certificate authority (`ca.pem`, "twr test CA") and a certificate and key for
`localhost` / `127.0.0.1` (`hub.pem`, `hub.key`), made once with the `openssl` CLI for the
runner TLS tests in `crates/twr-hub/tests/runners.rs`. They protect nothing: the key is public
on purpose, and nothing outside the tests trusts this CA.
