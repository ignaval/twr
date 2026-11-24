# twr relay wire protocol

Status: **draft, version 1.** Normative for `twr relay serve` and for every twr component that talks to a relay: the phone gateway (hub side), the phone apps and `twr-link`. Nothing here is released yet. Until the first release that ships it, version 1 can still change. Each change is recorded in [Changes](#changes) at the end, with the commit that made it.

The design and the reasons for it are in the phone pairing design, §11 to §13. This page specifies only what crosses the wire. [relay.md](relay.md) covers running a relay.

Key words: **must**, **must not**, **should** and **may** are used as in RFC 2119.

## Contents

1. [Overview](#1-overview)
2. [Common encodings](#2-common-encodings)
3. [Names, pins and relay URLs](#3-names-pins-and-relay-urls)
4. [Front end: one port, routed by the ClientHello](#4-front-end-one-port-routed-by-the-clienthello)
5. [Hub protocol (`twr-relay/1`)](#5-hub-protocol-twr-relay1)
6. [Phone passthrough](#6-phone-passthrough)
7. [Port mode, for a relay without a DNS name](#7-port-mode-for-a-relay-without-a-dns-name)
8. [HTTPS API](#8-https-api)
9. [Record store (pkarr)](#9-record-store-pkarr)
10. [iroh relay and admission](#10-iroh-relay-and-admission)
11. [Limits and refusals](#11-limits-and-refusals)
12. [Versioning and negotiation](#12-versioning-and-negotiation)
13. [Conformance and test vectors](#13-conformance-and-test-vectors)
14. [Security properties](#14-security-properties)
15. [Changes](#changes)

## 1. Overview

A relay listens on one TCP port, 443 for twr's own instances. It serves four things on it:

| Who | What | How the relay tells it apart |
|---|---|---|
| A hub (the phone gateway) | One long-lived TLS connection to the relay, multiplexed with yamux. Phone connections arrive on it as streams. | TLS server name is the relay's own host name (or absent), ALPN `twr-relay/1` |
| A phone | A TLS connection to its hub, passed through without being decrypted | TLS server name `<hub>-<tag>.<phone suffix>` |
| A runner or hub using iroh (`twr-link`) | iroh's relay protocol (HTTP upgrade to WebSocket at `/relay`) | TLS server name is the relay's iroh host name |
| Anyone | The HTTPS API: info, hub status, record store, iroh admission | TLS server name is the relay's own host name (or absent), any ALPN except `twr-relay/1` |

Optionally, UDP port 7842 serves iroh's QUIC address discovery (QAD), and extra TCP ports serve [port mode](#7-port-mode-for-a-relay-without-a-dns-name).

The relay never holds a key that decrypts phone traffic. It terminates TLS only for its own names: the hub connection, the HTTPS API and the iroh relay.

```text
phone ──TLS (pinned hub key, SNI <hub>-<tag>.eu.relay.twr.tools)──┐
                                                                  │ relay: peek SNI, check tag,
                                                                  │ open a yamux stream to the hub,
hub ──TLS (relay's cert, ALPN twr-relay/1)──yamux──────────────────┘ copy bytes both ways
```

## 2. Common encodings

- **base32**: RFC 4648 alphabet, lowercase, no padding (`a`-`z`, `2`-`7`). Decoders must reject uppercase, padding and any other character.
- **base64url**: RFC 4648 §5, no padding. Decoders must reject padding.
- **hex**: lowercase.
- **u64be(n)**: 8 bytes, big-endian.
- **Strings in hashes and MACs** are ASCII bytes. `\0` is one zero byte.
- **Times** are integer Unix seconds, UTC.
- **JSON** is UTF-8 (RFC 8259). Receivers must ignore object members they don't know, unless a section says otherwise. Senders must not send duplicate member names; receivers may reject them.
- **Frames.** On the hub connection, a frame is a 4-byte big-endian length followed by that many bytes of one JSON object. The length must be between 2 and 65536. A receiver that reads a length outside that range, or a body that isn't one JSON object, must close the stream.

## 3. Names, pins and relay URLs

### 3.1 Pin

A **pin** is SHA-256 of a public key's DER-encoded SubjectPublicKeyInfo (SPKI): 32 bytes. Written as base64url, 43 characters. Phones pin the hub's TLS identity key with it (phase 1); hubs pin a self-hosted relay's key with it.

Supported SPKI key types, in version 1:

| Key | SPKI | Signature for [hub auth](#53-authentication) |
|---|---|---|
| ECDSA P-256 | `id-ecPublicKey` with `prime256v1`, uncompressed point (91 bytes DER) | ECDSA with SHA-256, DER-encoded `(r, s)` |
| Ed25519 | `id-Ed25519` (44 bytes DER) | Ed25519 (RFC 8032), 64 bytes |

A relay must refuse other key types with `bad_key`.

### 3.2 Hub name

A hub's relay name is derived from its pin:

```text
hub_name = base32( SHA-256( "twr-relay hub-name v1\0" || pin )[0..16] )
```

That is 128 bits, 26 characters. Anyone who knows the pin (the phone, from the invitation) can compute it; no one can claim a name without the key behind it ([§5.3](#53-authentication)).

### 3.3 Phone credential and tag

At pairing, the hub gives each phone a **relay credential**: a `phone_id` of 16 random bytes (written as 32 hex characters) and a `relay_key` of 32 random bytes (base64url). The hub registers both with its relays ([§5.5](#55-phone-credentials)). The relay key authorizes nothing at the hub; it only gets a phone past the relay to the hub, which then requires the pinned key and the device token as on any route.

The phone derives a **tag** for each 5-minute window:

```text
window = floor(unix_seconds / 300)
tag    = base32( HMAC-SHA256( key = relay_key,
                              msg = "twr-relay tag v1\0" || hub_name || u64be(window) )[0..16] )
```

`hub_name` here is its 26 ASCII characters. The tag is 26 characters.

The phone's TLS server name is then:

```text
<hub_name>-<tag>.<phone suffix>        e.g. 3j7…26 chars…-q4f…26 chars….eu.relay.twr.tools
```

The first label is 53 characters, under DNS's 63. The relay accepts the windows `window - 1`, `window` and `window + 1` by its own clock, so a phone clock may be off by up to 5 minutes, and a tag lives at most 15 minutes.

### 3.4 Relay URLs

A hub's relay setting names relays with a **relay URL**:

```text
relay://<host>[:<port>][#<pin>]
```

- `host` is a DNS name, an IPv4 address, or an IPv6 address in brackets. `port` defaults to 443.
- With `#<pin>`, the hub must accept the relay's certificate if and only if its SPKI pin equals `<pin>`, ignoring names, chains and dates. This is how a relay without a DNS name or a public certificate is used.
- Without a pin, the hub must verify the certificate against the WebPKI roots twr ships (`webpki-roots`) for `host`.
- Anything after the pin, a path or a query must be rejected.

`twr` as a setting value means twr's own relays. A twr release ships their URLs (`relay://eu.relay.twr.tools`, `relay://us.relay.twr.tools` when they exist). Everything else a client needs (phone routes, the iroh URL, the record store URL) it learns from the relay itself ([§5.4](#54-welcome), [§8.1](#81-get-v1info)).

## 4. Front end: one port, routed by the ClientHello

For each TCP connection the relay reads, without consuming, the first TLS record or records until it has the whole ClientHello. It then routes the connection by the server name (SNI) and the ALPN list in it. The connection's bytes, the ClientHello included, then go unchanged to whatever handles that route.

Rules, applied in this order:

1. The bytes aren't a TLS handshake record holding a ClientHello, the ClientHello is over 16 KiB, or it hasn't fully arrived within 5 seconds: close (`not_tls`, `too_large`, `slow_hello`).
2. ALPN offers `acme-tls/1`: the relay's own TLS, answering an ACME tls-alpn-01 challenge (RFC 8737), then close.
3. SNI is `<hub_name>-<tag>.<suffix>` for one of the relay's phone suffixes: [phone passthrough](#6-phone-passthrough).
4. SNI equals the relay's iroh host name: the [iroh relay](#10-iroh-relay-and-admission).
5. SNI equals the relay's own host name, or there is no SNI: the relay's own TLS. Then ALPN `twr-relay/1` goes to the [hub protocol](#5-hub-protocol-twr-relay1), anything else to the [HTTPS API](#8-https-api).
6. Anything else: close (`unknown_name`).

Server names compare case-insensitively, and a trailing dot is ignored. The relay sends nothing on a connection it closes in step 1, 3 or 6, so a scanner can't tell a relay from a closed port without a valid name.

## 5. Hub protocol (`twr-relay/1`)

### 5.1 Transport

1. The hub opens TCP to the relay URL's host and port and starts TLS 1.3 with:
   - SNI: the relay URL's host, if it is a DNS name; none for an IP address;
   - ALPN: the hub protocol versions it supports, newest first, for example `["twr-relay/2", "twr-relay/1"]`. Version 1 is `twr-relay/1`.
2. The hub checks the relay's certificate as [§3.4](#34-relay-urls) says.
3. The relay selects one ALPN value. If it selected none, or one the hub didn't offer, the hub must close.
4. Both sides run **yamux** (the [yamux specification](https://github.com/hashicorp/yamux/blob/master/spec.md), version 0) over the TLS stream. The hub is the yamux client (odd stream ids), the relay the server. Streams start with yamux's 256 KiB window. twr uses the `yamux` crate 0.13. Its relay allows at most 32 streams and a 16 MiB receive window per hub connection; twr's hub client allows 64 streams and 64 MiB. Each side enforces only its own limits, so other implementations may choose theirs.
5. The hub opens the first stream, the **control stream**. It carries [frames](#2-common-encodings) in both directions for the life of the connection. Every later stream is opened by the relay and carries one phone connection.

TLS 1.2 must be refused: the authentication below depends on a TLS 1.3 exporter.

### 5.2 Messages on the control stream

Every frame is a JSON object with a string member `type`. Version 1 defines:

| `type` | Direction | When |
|---|---|---|
| `hello` | hub → relay | First frame, exactly once |
| `welcome` | relay → hub | Answer to a valid `hello` |
| `phones` | hub → relay | Any time after `welcome`: the full set of phone credentials |
| `phones_ok` | relay → hub | Answer to `phones` |
| `moved` | hub → relay | Any time after `welcome`: a signed "moved to" note |
| `moved_ok` | relay → hub | Answer to `moved` |
| `ping` | either | Keepalive |
| `pong` | either | Answer to `ping` |
| `drain` | relay → hub | The relay is going away; reconnect elsewhere or later |
| `error` | either | A refusal or a protocol error |

A receiver that gets an unknown `type` must answer `{"type":"error","code":"unsupported","ref":"<that type>","fatal":false}` and carry on.

`phones` and `moved` are requests. The relay answers each, in order, with its `_ok` message or with a non-fatal `error` whose `ref` is the request's type (`"phones"` or `"moved"`). An `error` without a matching `ref`, such as `quota`, is a notice and answers nothing.

### 5.3 Authentication

Both sides compute a TLS exporter (RFC 8446 §7.5) on the connection:

```text
exporter = TLS-Exporter(label = "EXPORTER-twr-relay-hub-auth", context = "twr-relay/1", length = 32)
```

`context` is the selected ALPN value as bytes. The hub signs:

```text
signed = "twr-relay hub-auth v1\0" || exporter
```

with its identity key (the key the phones pin), and sends:

```json
{"type":"hello",
 "spki":"<base64url DER SPKI>",
 "sig":"<base64url signature>",
 "client":"twr 0.7.0",
 "features":[]}
```

- `client` is informational, at most 64 characters.
- `features` lists optional extensions the hub understands. Version 1 defines none.

The relay checks the key type ([§3.1](#31-pin)) and the signature, then derives the hub name from the SPKI's pin. Because the exporter is unique to this TLS session, a signature can't be replayed on another connection or relayed by a man in the middle. The relay must not answer anything but `error` before a valid `hello`, and must close a connection that sends no valid `hello` within 10 seconds.

### 5.4 Welcome

```json
{"type":"welcome",
 "hub":"<hub_name>",
 "session":"<16 random bytes, hex>",
 "server":"twr 0.7.0",
 "region":"eu",
 "features":[],
 "phone_routes":[{"host":"eu.relay.twr.tools","port":443,"suffix":"eu.relay.twr.tools","tags":[1]}],
 "port_route":null,
 "relay_pin":null,
 "iroh_url":"https://iroh.eu.relay.twr.tools/",
 "records_url":"https://eu.relay.twr.tools/pkarr",
 "limits":{"phones":32,"phone_connections":8,"bytes_per_second":10000000,"monthly_bytes":20000000000,"idle_seconds":600}}
```

- `phone_routes`: where phones connect. For each: connect TCP to `host:port`, and send the SNI `<hub_name>-<tag>.<suffix>` with the tag version from `tags`, the highest one the phone supports. `host` may be an IP address; the phone then connects to it and still sends the SNI. A route with an empty list means only port mode is offered.
- `port_route`: `{"host":…,"port":…}` if this relay gives this hub a port of its own ([§7](#7-port-mode-for-a-relay-without-a-dns-name)), else `null`.
- `relay_pin`: the relay's own pin when it uses a self-signed certificate, so phones can check the [HTTPS API](#8-https-api) by pin. `null` when the certificate is publicly trusted.
- `iroh_url`, `records_url`: `null` when the relay doesn't run them.
- `limits`: the values this relay enforces for this hub, for display and planning.

If a session for the same hub name already exists on this relay, the new one replaces it: the relay sends the old one `error` with code `replaced` and `fatal: true`, and closes it.

### 5.5 Phone credentials

```json
{"type":"phones","phones":[{"id":"<32 hex>","key":"<base64url, 32 bytes>"}]}
```

The list is the hub's complete set, which replaces whatever the relay held. An empty list removes all. The relay answers:

```json
{"type":"phones_ok","count":3}
```

or `error` with `too_many_phones` (over the limit; the relay keeps the previous set) or `bad_request`.

- The hub must send `phones` after every `welcome`, before it expects phone connections.
- Revoking a device on the hub must send a new set at once. The relay closes every open phone connection whose `id` is no longer in the set.
- The relay keeps a hub's set for 7 days after its session ends, so that the [status page](#82-get-v1hubslabel) can tell a phone "your hub is offline" rather than "unknown". It doesn't persist the set across a relay restart; the hub resends it when it reconnects.
- The relay must not log `key` values, and must not return them in any response.

### 5.6 Phone streams

For each phone connection the relay accepts ([§6](#6-phone-passthrough)), it opens a yamux stream to the hub and sends one frame:

```json
{"type":"phone","phone":"<32 hex>","mode":"sni","remote":"198.51.100.7:52114"}
```

- `phone`: the credential that matched, or `null` in port mode without a credential.
- `mode`: `sni` or `port`.
- `remote`: the phone's address as the relay saw it, for the hub's own rate limits and display. A hub must not treat it as authenticated.

After that frame the stream carries the phone's TCP bytes unchanged in both directions, starting with its ClientHello. The hub runs its TLS server on it as on a direct connection: the same pinned identity, the same device token check. Closing the stream (yamux FIN or RST) closes the phone's TCP connection, and the reverse.

The hub may refuse a stream by resetting it.

### 5.7 Moved note

When a hub changes relays, it can leave a signed note at the old relay for phones that were offline:

```json
{"type":"moved","body":"<base64url of a JSON object>","sig":"<base64url>"}
```

- `sig` is the hub identity key's signature, as in [§5.3](#53-authentication), over `"twr-relay moved v1\0" || body bytes`.
- The body is `{"hub":"<hub_name>","issued":<unix>,"routes":[…]}`. `routes` is the hub's route list in the format the hub and the phone agree on (the invitation's route objects); the relay doesn't interpret it. The body must be at most 4096 bytes.

The relay checks that the signature verifies under this session's key, that `body.hub` is the session's hub name and that `issued` is within 5 minutes of its clock. It stores the note with the SPKI for 90 days, replacing an older one, and answers `{"type":"moved_ok"}`. It serves it at [`/v1/hubs/<hub_name>/moved`](#83-get-v1hubshub_namemoved). A phone checks that SHA-256 of the returned SPKI equals its pin, then the signature, before trusting any route in it.

### 5.8 Keepalive, drain and errors

- Either side may send `{"type":"ping","n":<integer>}`; the other answers `{"type":"pong","n":<same>}`. The hub should ping every 30 seconds. The relay closes a session that has sent nothing for 90 seconds.
- Before a planned shutdown the relay sends `{"type":"drain","reason":"restart","retry_after_seconds":<n>}`, stops sending new phone streams, and closes the connection after at most 10 seconds. The hub should connect to its other relay at once, and back to this one after `retry_after_seconds`.
- `{"type":"error","code":"<code>","message":"<text>","fatal":<bool>,"ref":"<type>"}`. `message` is for logs, not for parsing. After a fatal error the sender closes the connection. Codes in version 1:

| Code | Fatal | Meaning |
|---|---|---|
| `bad_request` | maybe | A frame that doesn't parse or is missing members |
| `bad_key` | yes | Unsupported key type or malformed SPKI |
| `bad_signature` | yes | The `hello` or `moved` signature doesn't verify |
| `hello_required` | yes | Anything but `hello` first |
| `denied` | yes | The hub's name or address is on the relay's deny list |
| `rate_limited` | yes | Too many new hub names from this address ([§11](#11-limits-and-refusals)) |
| `capacity` | yes | The relay is full |
| `replaced` | yes | A newer session for the same hub took over |
| `too_many_phones` | no | `phones` over the limit |
| `quota` | no | The hub's monthly byte quota is used up; phone connections are refused until it frees up |
| `unsupported` | no | Unknown `type` |

A hub should back off reconnecting after a fatal error: from 1 second, doubling to at most 5 minutes, with jitter. After `denied` it should wait at least an hour.

## 6. Phone passthrough

When the front end ([§4](#4-front-end-one-port-routed-by-the-clienthello)) sees SNI `<hub_name>-<tag>.<suffix>`:

1. Both labels must be 26 base32 characters, else close (`bad_label`).
2. The hub must have a credential set and a live session on this relay, else close (`hub_offline`).
3. The tag must match one of the hub's credentials for one of the three accepted windows, else close (`bad_tag`).
4. The hub's limits must allow another connection ([§11](#11-limits-and-refusals)), else close.
5. The relay opens a phone stream ([§5.6](#56-phone-streams)) and copies bytes both ways, the peeked ClientHello first.

The relay sends nothing to the phone itself in any case: it is the hub's TLS server that answers. A phone that gets a closed connection with no TLS answer should fetch the [status page](#82-get-v1hubslabel) to say why.

The phone's check on this connection is the same as on any route: the server's SPKI pin must equal the hub's pin. It must not check the certificate's names or chain.

## 7. Port mode, for a relay without a DNS name

Phones that connect to an IP address send no SNI. To still route them, a self-hosted relay can give a hub a TCP port of its own, configured by the relay's operator for a hub name.

- On such a port, a ClientHello whose SNI parses as a phone label is handled as in [§6](#6-phone-passthrough), for any hub.
- A ClientHello without SNI goes to the port's hub, if that hub is connected, as a stream with `"mode":"port"` and `"phone":null`. The relay can't check a credential then; the hub's pinned key and device token are the only checks, as on an open port of a VPS.
- The operator may require a credential on a port (`allow_unauthenticated = false`); then a ClientHello without SNI is closed.

A phone whose TLS stack can set the server name independently of the address it connects to (Network.framework's `sec_protocol_options_set_tls_server_name`, or a custom `Dns` in OkHttp) should use the SNI form even without DNS: the relay's `phone_routes` then name an IP `host` with the suffix `relay.invalid`, which never resolves (RFC 6761 §6.4) and so is never looked up. Port mode is the fallback for a stack that can't.

## 8. HTTPS API

Served on the relay's own host name, or with no SNI, over HTTP/1.1 or HTTP/2. Every response is JSON with `Content-Type: application/json`, except the record store's. No endpoint sets cookies or logs request bodies. Responses carry `Cache-Control: no-store` unless a section says otherwise.

### 8.1 `GET /v1/info`

```json
{"relay":"twr-relay",
 "server":"twr 0.7.0",
 "region":"eu",
 "hub_protocols":["twr-relay/1"],
 "tag_versions":[1],
 "admission_versions":[1],
 "records":"pkarr",
 "phone_suffixes":["eu.relay.twr.tools"],
 "iroh_url":"https://iroh.eu.relay.twr.tools/",
 "iroh_quic_port":7842,
 "records_url":"https://eu.relay.twr.tools/pkarr",
 "relay_pin":null}
```

- `iroh_quic_port`: the UDP port of iroh's QUIC address discovery (QAD, [§10.1](#101-the-relay)) on the iroh host, or `null` when the relay doesn't run it. An endpoint that finds it `null` or missing (a relay from before this field) runs without QAD.
- The other fields are as in the [welcome](#54-welcome).

Cacheable for 5 minutes. Monitoring uses it as the liveness check.

### 8.2 `GET /v1/hubs/<label>`

`<label>` is a phone's `<hub_name>-<tag>`, the same label as in its SNI.

- `200 {"hub":"<hub_name>","connected":true}` if the tag is valid and the hub has a session here.
- `200 {"hub":"<hub_name>","connected":false}` if the tag is valid for a credential the relay still keeps ([§5.5](#55-phone-credentials)) and the hub has no session.
- `404 {"error":"unknown"}` otherwise, including a bad tag. The relay doesn't say which.

So only a phone holding a credential learns whether a hub is online.

### 8.3 `GET /v1/hubs/<hub_name>/moved`

`200 {"spki":"<base64url>","body":"<base64url>","sig":"<base64url>"}`, the [moved note](#57-moved-note), or `404 {"error":"none"}`. Public: the note is signed, and phones that know the pin can check it.

### 8.4 Errors

`400 {"error":"bad_request"}`, `404 {"error":"not_found"}`, `413 {"error":"too_large"}`, `429 {"error":"rate_limited"}` with `Retry-After`, `503 {"error":"capacity"}`.

## 9. Record store (pkarr)

The relay's replacement for `dns.iroh.link`: a store for signed pkarr packets, holding iroh address records and `twr-link`'s pairing locators. Its HTTP interface is the [pkarr relay protocol](https://github.com/pubky/pkarr/blob/main/design/relays.md), so iroh's `PkarrPublisher`/`PkarrResolver` and the `pkarr` client work against it unchanged with `records_url` as the relay URL.

- `PUT /pkarr/<z32 public key>`, body `<64-byte Ed25519 signature><u64be timestamp in microseconds><DNS packet, at most 1000 bytes>`.
  - The signature must verify under the key for the BEP 44 signable `"3:seqi" || decimal(timestamp) || "e1:v" || decimal(len(packet)) || ":" || packet`.
  - `200` when stored. `400` for a bad key, signature or length. `409` if the store holds a packet for this key with a later timestamp. `428` if it holds one with the same timestamp but different content. `429` per [§11](#11-limits-and-refusals).
  - The relay doesn't parse the DNS packet beyond its length, and ignores the request's `Content-Type`. Clients should send `application/pkarr.org/relays#payload` or `application/octet-stream`. If a later version checks it, it keeps accepting any type for one release.
- `GET /pkarr/<z32 public key>`: `200` with the stored body (`application/pkarr.org/relays#payload`) and `Last-Modified` and `memento-datetime` headers set to when it was stored; `304` if `If-Modified-Since` isn't older than that; `404` if there is none. `Cache-Control: public, max-age=30`.
- A `policy` query parameter is accepted and ignored: there is no DHT behind this store.
- Records expire 2 hours after they were stored. iroh republishes every 5 minutes, and pairing locators live 15 minutes, so a live record never expires. The store isn't persisted; after a restart, publishers fill it again within minutes.
- Records are public data, like DNS. Publishing to both of a machine's regions covers the loss of one.

z32 is z-base-32 of the 32-byte key, 52 characters, as pkarr writes it.

## 10. iroh relay and admission

### 10.1 The relay

On its iroh host name the relay runs iroh's own relay server (the `iroh-relay` crate, at the version `twr-link` uses), with TLS terminated by the relay under its own certificate. The protocol is iroh's: HTTP upgrade at `/relay`, plus `/ping` and `/generate_204` for latency probes. Its versioning is iroh's 1.x wire promise. If the relay runs QAD, it does so on UDP (7842 by default) with the same certificate, and `/v1/info` gives the port as `iroh_quic_port`.

Only **admitted** endpoints may connect. A relay that serves iroh must refuse every other endpoint at the relay handshake.

### 10.2 `POST /v1/iroh/admit`

On the relay's HTTPS API. The body is one JSON object, at most 4 KiB, of one of two kinds.

**Pairing admission**, for an endpoint that is about to pair and so has no partner yet:

```json
{"v":1,"kind":"pairing","endpoint":"<64 hex>","issued":<unix>,"sig":"<base64url>"}
```

`sig` is the endpoint key's Ed25519 signature over `"twr-relay admit v1\0pairing\0" || endpoint (32 bytes) || u64be(issued)`. `issued` must be within 5 minutes of the relay's clock. The relay admits the endpoint for 15 minutes from now.

**Pair admission**, for two endpoints that have paired:

```json
{"v":1,"kind":"pair","a":"<64 hex>","b":"<64 hex>","expires":<unix>,"sig_a":"<base64url>","sig_b":"<base64url>"}
```

Each of `sig_a` and `sig_b` is that endpoint key's signature over `"twr-relay admit v1\0pair\0" || a (32 bytes) || b (32 bytes) || u64be(expires)`. `a` and `b` must differ. The relay admits both until `expires` or 30 days from now, whichever is first. Posting the same record again renews it, so either side can repost it on every start without the other being online. A third party who copies the record can only renew the same two endpoints.

The two signers exchange their signatures over their own authenticated link. Both should post the record to every relay either of them uses, since iroh connects to the peer's home relay as well as its own.

Answers: `200 {"admitted":["<64 hex>",…],"until":<unix>}`; `400 {"error":"bad_request"}` or `{"error":"bad_signature"}` or `{"error":"stale"}`; `429` per [§11](#11-limits-and-refusals); `403 {"error":"denied"}` for a denied key or address.

Admissions are kept in the relay's state directory, so a restart doesn't lock out paired runners.

### 10.3 Endpoint ids

64 hex characters: the 32-byte Ed25519 public key that is the iroh `EndpointId`.

## 11. Limits and refusals

Defaults for version 1. A relay operator can change them; a hub learns the values that apply to it in `welcome`.

| Scope | Limit | Default |
|---|---|---|
| Per hub | registered phones | 32 |
| | phone connections open at once | 8 |
| | bytes relayed in any 30 days, both directions | 20 GB |
| Per connection | throughput, each direction, token bucket | 10 MB/s, bursts to 32 MB |
| | idle (no bytes either way) | 10 minutes |
| | time to a complete ClientHello | 5 seconds |
| | time to a valid `hello` after TLS | 10 seconds |
| Per source address | new hub names per hour | 10 |
| | phone connection attempts per minute | 60 |
| | connections that haven't sent a full ClientHello | 16 |
| | record store `PUT`s per minute | 60 |
| | pairing admissions per hour | 10 |
| | pair admissions per hour | 60 |
| Global | connections | 20,000 |
| | hub sessions | 50,000 |
| | stored records | 100,000 |
| | admitted iroh endpoints | 200,000 |
| | iroh relay connections, receive rate | 10 MB/s, bursts to 32 MB |

- A **source address** is the IPv4 address, or the IPv6 /64.
- An operator may exempt addresses from the per-source-address limits (twr's own relays exempt their monitoring hosts). The deny list still applies to them.
- A relay may keep a deny list of hub names, endpoint ids and address prefixes.
- Every refusal increments a counter by reason; none is logged with an address ([§14](#14-security-properties)).

## 12. Versioning and negotiation

Each wire format has its own version, negotiated where both sides talk:

| Format | Version 1 | How a client finds what the relay supports |
|---|---|---|
| Hub protocol | ALPN `twr-relay/1` | TLS ALPN: the hub offers every version it has, newest first; the relay picks the highest it shares |
| Phone tag | `"twr-relay tag v1\0"`, the 53-character label | `phone_routes[].tags` in `welcome`, which the hub passes to its phones; `/v1/info` `tag_versions` |
| Moved note | `"twr-relay moved v1\0"` | Fixed per hub protocol version |
| Admission | `"v":1` and `"twr-relay admit v1\0"` | `/v1/info` `admission_versions` |
| HTTPS API | the `/v1/` path prefix | `/v1/info`; a later version adds `/v2/` beside `/v1/` |
| Record store | the pkarr relay protocol | Fixed by pkarr |
| iroh relay | iroh's relay protocol | iroh's own negotiation, within iroh 1.x |

Additive changes inside a version (a new optional member, a new message type, a new error code) are announced as `features` strings in `hello` and `welcome`, and a side uses them only if the other announced them. Anything else needs a new version number.

Compatibility rules:

- A relay release keeps serving every format version that a twr release from the last 6 months uses, and at least the versions of the last two twr releases.
- A newer client talking to an older relay negotiates down. A client may drop an old version only once twr's own relays have served a newer one for 6 months.
- A future tag version must use a label that version 1 can't parse (for example a different length), so that an old relay closes it cleanly rather than mis-reading it.
- For a change both sides need, the relays update first, then the clients.

## 13. Conformance and test vectors

- `crates/twr-relay/tests/vectors/v1.json` holds test vectors for version 1:
  - hub names from pins;
  - tags, labels and server names for given keys and windows;
  - hub-auth signatures for a fixed exporter, Ed25519 (reproducible) and P-256 (randomized, so verify only);
  - a moved note, and both admission kinds with their signed bytes;
  - a frame and a record signable.

  Once version 1 is released, that file is frozen: a change to it is a protocol change. `tests/vectors.rs` checks that the Rust code computes exactly the file's values. Client code in other crates and in the phone apps should test against the file directly.
- `crates/twr-relay/tests/conformance.rs` runs relays in-process on loopback, with and without a DNS name, and exercises every section of this page through `twr_relay::client` and raw frames.
- `crates/twr-relay/tests/parse_fuzz.rs` fuzzes what the relay reads before authentication: ClientHellos, certificates, frames, admission bodies, relay URLs and labels.
- `crates/twr-relay/tests/acme.rs`, run by `docs/tools/relay-acme-pebble.sh`, takes the Let's Encrypt path against a local pebble in Docker.
- `twr relay probe <relay URL>` checks a running relay end to end: info, a test hub session, a phone stream through it, the status page, the record store, iroh admission and an iroh relay handshake. `--state <dir>` keeps its keys between runs. Monitoring runs it, and so does the compatibility job below.
- `docs/tools/relay-compat.sh` runs the relay at HEAD against `twr relay probe` from the last two releases, and HEAD's probe against the last two releases' relays. Releases from before the relay existed are skipped and reported as such. CI runs it as the `relay compatibility` job.

## 14. Security properties

What the relay can do, if it is compromised or run by someone hostile:

- See hub names, tags, both sides' IP addresses, connection times and byte counts.
- Block connections, or let in connections that then fail at the hub's pinned key or device token.
- Not read or alter phone traffic, which is TLS 1.3 end to end to the hub's pinned key.
- Not impersonate a hub: the phone checks the pin on every connection.
- Not learn a device token: it travels only inside that TLS.

What an on-path observer of a phone sees: the SNI, so the hub name and a tag that changes every 5 minutes. A tag observed on the path can be replayed for up to 15 minutes to reach the hub, which then still requires the pinned key's TLS and a device token. Encrypted Client Hello would hide the SNI; version 1 doesn't depend on it.

What the relay records: counters (connections, bytes, refusals by reason), per-hub daily byte totals kept 30 days for the monthly quota, admissions and moved notes until they expire. It logs no content, no SNI, no addresses and no keys. Operators' log levels must not change that: the relay's own code has no logging of those at any level, and `twr relay serve` turns off the embedded iroh relay's log, which has, whatever `RUST_LOG` says.

## Changes

| Date | Change |
|---|---|
| 2026-11-23 | First draft of version 1. |
| 2026-11-23 | Built and tested. Clarified: error answers carry the request's type in `ref` (§5.2); each side's yamux limits (§5.1); exempt addresses (§11); the test files (§13); the iroh relay's log (§14). No wire change. |
| 2026-11-23 | `/v1/info` gains `iroh_quic_port` (§8.1, §10.1). The record store ignores `Content-Type` (§9). Additive: older clients ignore the field. |
