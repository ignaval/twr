# Certificate-free phone pairing: the wire contract

This is the contract that the hub, the gateway, the Android and iOS apps and the desktop client implement. Each piece is checked against the test vectors in [`clients/shared/fixtures/phone-pairing-v2.json`](../../clients/shared/fixtures/phone-pairing-v2.json), which `cargo test -p twr-core phone_pairing` generates and checks.

The design and its reasons are in the phone-pairing report (2026-11-23), kept outside the repository. User-facing behaviour belongs in [mobile.md](../mobile.md), [remote-clients.md](../remote-clients.md) and [security-model.md](../security-model.md).

Phase 2 adds a relay route; its wire protocol is specified separately by the relay work. This document reserves the `relay:` route kind and nothing else.

## Identity

The gateway keeps an identity in its data directory, `mobile/`, beside `gateway-id`. That directory is mode 0700, and every file in it is 0600.

| File | Contents |
|---|---|
| `identity-key.pem` | ECDSA P-256 private key, PKCS#8 PEM. Created once and never rotated, except by `twr phone rekey`. |
| `identity-cert.pem` | A self-signed X.509 certificate for that key, described below |
| `iroh-key` | 32 random bytes, an Ed25519 secret key for the phase 2 iroh route. Its public key, as iroh prints an `EndpointId`, goes in invitations as `iroh`. Unused in phase 1. |

**The certificate:**

- validity 800 days, starting one hour before creation;
- subject CN `twr hub`;
- SAN `DNS:twr-hub.invalid`;
- EKU `serverAuth`;
- signed with the identity key, ECDSA-SHA256.

The gateway re-issues it with the **same key** when fewer than 30 days remain. iOS applies its 825-day, SAN and EKU rules to some trust paths, and staying within them costs nothing.

**The pin** is `base64url-nopad(SHA-256(SubjectPublicKeyInfo DER))`: 43 characters. It depends only on the key, so re-issuing the certificate never changes it.

The gateway serves TLS 1.3 (TLS 1.2 only if a client offers nothing else) with this certificate whenever it runs without `--tls-cert`. With `--tls-cert`/`--tls-key` it serves that certificate instead: that is **certificate mode**, today's behaviour, with version 1 invitations.

## Pairing codes

- 12 characters from `ABCDEFGHJKLMNPQRSTUVWXYZ23456789`: 32 symbols, so 60 bits.
- Displayed as `XXXX-XXXX-XXXX`.
- **Normalization:** strip ASCII spaces and `-`, then uppercase ASCII. The result must be exactly 12 characters, all in the alphabet. Anything else is not a code.
- One use, 5 minutes, 5 failed attempts. That is the existing device store's rule, now with 12 characters instead of 10.

## The proof key and the pin proof

These are for typed pairing, where the phone has no pin yet.

```text
stretched = Argon2id(password = normalized code (ASCII), salt = "twr-mobile-pair-v2",
                     m = 65536 KiB, t = 3, p = 1, version 0x13, output 32 bytes)
K_proof   = HKDF-SHA256(salt = "twr-mobile-pair-v2", ikm = stretched, info = "pin-proof", L = 32)
proof     = HMAC-SHA256(K_proof, "twr-mobile-pin-v2" || 0x00 || pin (32 raw bytes, the SHA-256)
                                  || nonce (32 bytes) || gateway_id (UTF-8, as served by /health))
```

- The hub derives `K_proof` once, when it creates the code (`device.pair_begin`), and keeps it with the pairing state, never the code itself. The state lives in `devices.json`, 0600, and is removed with the pairing.
- The gateway asks the hub for proofs over the owner socket. Neither ever logs a code, `K_proof` or a proof.

## Gateway endpoints

**`GET /health`, unauthenticated.**

```json
{"service":"twr-mobile","protocol":2,"gateway_id":"<uuid>","pin":"<43 chars>"}
```

In certificate mode, `pin` is absent and `protocol` stays `1`: released desktop clients refuse any other number. Pinned-mode clients check that `pin` is there, not the number. Protocol 1 clients ignore the new field. A gateway without a pin (certificate or development mode) answers `/v1/pair/proof` with `404`.

**`GET /v1/pair/proof?nonce=<base64url-nopad of 32 bytes>`, unauthenticated.**

- `200 {"proof":"<base64url-nopad of 32 bytes>"}` while an invitation is open.
- `404` when none is.
- `400` for a malformed nonce.
- It shares `/v1/pair`'s rate limits, per address and global, and its 35-second deadline.
- A proof request doesn't count as a failed attempt. Every proof for the open code is derived from the same key, so a second one tells an attacker nothing new.

**`POST /v1/pair`** is unchanged: `{code, name}` returns the device token and metadata. The code is normalized before the hub checks it.

## Invitation, version 2

The QR code and the copied link both carry this JSON:

```json
{
  "type": "twr-pair",
  "version": 2,
  "id": "<gateway_id uuid>",
  "name": "<hub display name, at most 80 characters>",
  "pin": "<43 chars>",
  "iroh": "<EndpointId string>",
  "code": "<12 characters, normalized>",
  "expiresAt": 1763900000000,
  "routes": ["tls:192.168.1.20:47321", "tls:[2001:db8::5]:47321", "tls:box.tail1234.ts.net:47321"],
  "scopes": ["read", "operate"]
}
```

**Fields:**

- `iroh` is optional.
- `expiresAt` is milliseconds since the epoch. The hub enforces expiry; clients only display it.
- `scopes` is for display only.
- There are 1 to 8 routes. The whole JSON is at most 2048 bytes.

**Parsers:**

- Reject unknown top-level keys, a wrong `type`, and a version other than 1 or 2. Version 1 stays as today: `url`, `code`, `expiresAt`.
- Keep route entries whose kind they don't know, and skip them when connecting.

**Link form.** `twr://pair#` followed by base64url-nopad of the JSON's UTF-8 bytes. The secret is in the fragment. Scanners accept the raw JSON, the `twr://pair#` link, and version 1 JSON.

**Routes.** Each entry is `<kind>:<rest>`:

| Kind | Rest | Meaning |
|---|---|---|
| `tls` | `<host>:<port>`, where host is an IPv4 literal, `[IPv6]` or a DNS name | Direct TLS to the gateway, authenticated by the pin |
| `relay` | reserved for phase 2; defined by the relay specification | |

**How the hub builds the list:**

1. The configured `address` first, if set: a DNS name or a forwarded `host:port`.
2. Then addresses from the interfaces that are up and not loopback or link-local, in this order:
   - global IPv4 and IPv6;
   - tailnet addresses (100.64.0.0/10, fd7a:115c:a1e0::/48);
   - private LAN addresses (RFC 1918, fc00::/7 outside the tailnet range).
3. Cut the list off at 8.

Interface addresses come only for a listener on all addresses (`0.0.0.0` gives IPv4 ones, `[::]` IPv6 ones). A listener on one address offers that address, a loopback one included. Within each class, IPv4 comes before IPv6. When there is no route at all, the gateway opens no invitation and asks for `--address`.

**Saved settings.** The pinned gateway keeps `address`, `listen` and `autostart` in `gateway-settings.json` in its directory (0600). The desktop app keeps its certificate-mode gateway's settings in its own settings file, which the gateway cannot read, so `--address` is saved only in pinned mode.

## Client trust rule

**For a hub paired by pin,** on every route and every connection: accept the TLS connection only if the leaf certificate's SPKI SHA-256 equals the saved pin. Ignore the chain, the name and the validity dates. Send nothing first: not the code, not the token, not the request line.

- A mismatch is a hard error naming the route: "this is another device, not <hub>". Never fall back to system trust for a pinned hub.
- Certificate-mode hubs keep system trust, and their credentials never move to a pinned route, or the reverse.

**Typed pairing,** with an address and a code:

1. Connect, and capture the leaf SPKI pin `P` without trusting it yet.
2. Send `GET /v1/pair/proof` with a fresh random 32-byte nonce, and read `gateway_id` from `/health` on the same connection or another one that presented `P`.
3. Compute `K_proof` from the typed code. Check the proof in constant time.
4. Only then `POST /v1/pair` on a connection that presents `P`, and save `P` as the pin.

A man in the middle presenting its own key can't produce a valid proof for that key without the code.

## Saved hub (client side)

| Field | Meaning |
|---|---|
| `pin` | The pinned key |
| `iroh` | The iroh endpoint id, when the invitation had one |
| `routes` | The invitation's routes, plus any the gateway reports later. At most 16, deduplicated. A route is dropped after 30 days unseen. |
| `last` | Per network type (`wifi`, `cellular`, `other`), the route that last connected |
| `token`, `gateway_id`, `name`, `scopes` | As today |

## Racing routes

1. Start the remembered route for the current network type at once.
2. After 250 ms, start the remaining `tls` routes in parallel. On cellular, skip private LAN addresses (RFC 1918, fc00::/7 outside the tailnet range, `.local` names).
3. The first route to complete TLS with the pin and answer `/health` with the expected `gateway_id` wins. Cancel the rest.
4. Save the winner as `last` for the current network type.

**Reconnect at once on a network change:** Android's `ConnectivityManager` callback, iOS's `NWPathMonitor`. Don't wait for a timeout.

**While connected, the gateway reports its current routes** in the connection hello, as `routes`. The client merges them under the limits above. Only a pinned gateway does: the hello of a certificate-mode gateway has no `routes`.

## Client API (TypeScript ↔ native transport)

`clients/shared/remote.ts` `NativeTransport` is implemented by Android (`TwrTransportPlugin.java`), iOS (`TwrTransportPlugin.swift`) and the desktop (`clients/desktop/remote-transport.ts`). The extensions below are backwards compatible: certificate-mode hubs keep working unchanged.

```ts
interface Hub {
  id: string; name: string; url: string; scopes: string[]; gateway_id?: string;
  pin?: string;       // set for pinned hubs; absent for certificate-mode hubs
  routes?: string[];  // pinned hubs: known routes, e.g. "tls:192.168.1.20:47321"
  route?: string;     // pinned hubs: the route last connected
}
interface PairRequest {
  code: string; name: string; hubName: string;
  url?: string;                                            // certificate mode (v1), as today
  pin?: string; gatewayId?: string; routes?: string[]; iroh?: string;  // a scanned v2 invitation
  address?: string;                                        // typed pairing: "host" or "host:port" (default 47321)
}
interface NativeTransport {
  // existing: listHubs, pair, connect, send, disconnect, removeHub, addListener("message", …)
  updateRoutes(request: { id: string; routes: string[] }): Promise<void>;
  addListener(event: "network", listener: (value: { type: "wifi" | "cellular" | "other" | "none" }) => void):
    Promise<{ remove(): Promise<void> }>;
}
```

**`pair`:**

- `pin` set: a scanned invitation. Race `routes` with the pin check. Then `POST /v1/pair` on the winner.
- `address` set: typed pairing, with the proof flow above.
- Otherwise: `url`, certificate mode.

Every case returns a `Hub`; pinned ones carry `pin`, `routes` and `route`. The `url` of a pinned hub is `https://<host>:<port>` of the winning route, for display only.

**`connect`.** For a pinned hub, race the saved routes as in [Racing routes](#racing-routes), then open `/v1/socket` on the winner, with the pin check, and remember the winner. Certificate-mode hubs connect as today.

**`updateRoutes`** merges the routes the gateway's hello reports into the saved hub, within the limits in [Saved hub](#saved-hub-client-side). The shared TypeScript calls it when the hello carries `routes`.

**The `network` event** fires when the device's default network changes. When it fires, the TypeScript layer reconnects at once if it was connected, or was trying to be, without waiting for a timeout or backoff.

**Errors.** `pair` and `connect` reject with messages that name the route and the reason: unreachable, timed out, another device's key, or bad code. A pin mismatch carries `PIN_MISMATCH` in the message.

## Gateway command-line output (for `twr phone`, the TUI and the desktop wizard)

In pinned mode, `--pair --pair-json`, and `twr gateway --pair --pair-json` once the gateway is folded into `twr`, prints one JSON line. It is a superset of today's:

```json
{"code":"ABCDEFGHJK23","expires_at":1763900000,"scopes":["read","operate"],"gateway_id":"<uuid>",
 "invitation":{ …the version 2 invitation object… },"link":"twr://pair#…"}
```

`expires_at` is seconds, as today; the invitation's `expiresAt` is milliseconds. In certificate mode the output is unchanged, with no `invitation` and no `link`.

`--gateway-info` adds `"pin"` (absent in certificate mode) and `"routes"`, the list the gateway would put in an invitation now. Both are absent while a certificate-mode gateway runs. When no gateway runs, they describe the pinned gateway that would start, if its identity exists. A running gateway's record also carries `"mode"`: `pinned`, `certificate` or `development`. A pinned gateway's `tls_cert` and `tls_key` are its `identity-cert.pem` and `identity-key.pem`, so the desktop app's checks still read the record.
