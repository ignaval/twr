# Running a twr relay

A twr relay lets a phone reach a hub that it can't connect to directly: a hub at home behind a router while the phone is out on mobile data, or a server whose provider blocks incoming connections. Linked runners use the same relay for their iroh connections and address records.

twr will run relays of its own. You can also run one yourself on any small server. It is the same program, `twr relay serve`, in the normal `twr` binary.

The relay never decrypts phone traffic. The phone and the hub run TLS end to end, and the phone checks the hub's own key on every connection. The relay sees which hub a connection is for, the addresses at both ends, the times and the byte counts. It can't read or change what passes through, and it can't pretend to be your hub. [relay-protocol.md](relay-protocol.md) is the wire protocol, and §14 there lists what the relay can and can't do.

> **Status.** The relay server is built and tested. Hubs, phones and `twr-link` don't use it yet: that client work comes in a later release. Until then, a relay you run answers `twr relay probe` and nothing else uses it.

## Self-hosting, the short way

You need a server with a public IP address and TCP port 443 open. Any provider works. A $5 machine with 1 CPU and 1 GB of memory is enough.

1. Install twr on the server as `/usr/local/bin/twr`, where the service runs it from (see [install.md](install.md)):

   ```sh
   curl -fsSL https://twr.tools/install.sh | sudo TWR_INSTALL_DIR=/usr/local/bin sh
   ```

2. Install the service, with the server's public address:

   ```sh
   sudo /usr/local/bin/twr relay serve --install --public-host 203.0.113.9
   ```

   This creates a system user `twr-relay`, `/etc/twr-relay/relay.toml`, the state directory `/var/lib/twr-relay` and the systemd service `twr-relay.service`, then starts it. It prints the relay URL to give your hubs:

   ```text
   relay://203.0.113.9#mi0NrZW-k0zcdhogN2tLIeaTzp7gEvFre-31mbjw4K4
   ```

   The part after `#` is the relay key's pin. Hubs check the relay by that pin, so no DNS name and no certificate are needed. `twr relay url` prints it again later.

3. Open the ports in the provider's firewall, if it has one:

   | Port | For |
   |---|---|
   | TCP 443 | phones, hubs, the HTTPS API and its record store |
   | TCP 7443 | iroh's relay for linked runners (without a DNS name, iroh needs a port of its own) |
   | UDP 7842 | iroh's address discovery (optional; improves direct runner connections) |

4. Check it from anywhere, with the URL from step 2:

   ```sh
   twr relay probe 'relay://203.0.113.9#mi0NrZW-k0zcdhogN2tLIeaTzp7gEvFre-31mbjw4K4'
   ```

   The probe acts as a test hub with a test phone, sends data through the relay, and checks the record store and iroh's relay. Every line should say `ok`.

### Without a DNS name: what changes

Phones connect to a relay by a TLS server name of the form `<hub>-<tag>.<suffix>`. The tag is a short-lived credential the relay checks before it passes anything on. Without DNS, the suffix is `relay.invalid`, a name that never resolves. The phone connects to the relay's IP address and still sends that server name, so the relay checks credentials exactly as with DNS.

A phone whose TLS stack can't send a server name to an IP address can use **port mode** instead. You give each of your hubs a port of its own in `relay.toml`:

```toml
[[port_hub]]
hub = "q3m…26 characters…"     # the hub's relay name
listen = "[::]:4431"
```

The relay then passes every connection on that port to that hub without checking a credential. The hub still requires its own key and the device's token, as an open port on a server does today. Set `allow_unauthenticated = false` to accept only connections that carry a credential.

## Self-hosting with a DNS name

With a name, the relay gets a Let's Encrypt certificate by itself, and hubs check it as any HTTPS client does.

1. Point a name and its wildcard at the server, for example:

   ```text
   relay.example.com.     300 IN A     203.0.113.9
   *.relay.example.com.   300 IN A     203.0.113.9
   ```

   (and `AAAA` records for IPv6). The wildcard covers the phones' names and `iroh.relay.example.com`.

2. Install with the name and a contact address for Let's Encrypt:

   ```sh
   sudo /usr/local/bin/twr relay serve --install --hostname relay.example.com \
       --acme-contact mailto:you@example.com
   ```

   The relay uses ACME's tls-alpn-01 challenge on port 443 itself, so port 80 stays closed. You don't need to create a Let's Encrypt account by hand. The account and certificates are kept in `/var/lib/twr-relay/acme` and renewed automatically.

3. Only TCP 443 and UDP 7842 need to be open. The relay URL is `relay://relay.example.com`.

## Updating

The service runs `/usr/local/bin/twr`. `twr update` replaces the binary it runs as, so run it as root, then restart:

```sh
sudo /usr/local/bin/twr update
sudo systemctl restart twr-relay
```

A restart drains connected hubs first: they reconnect, to another relay if they have one. Phones reconnect by themselves.

## Configuration

`/etc/twr-relay/relay.toml`. Every key is optional, apart from what the table marks. `twr relay serve --config <file>` runs with another file, and `twr relay serve --hostname … --public-host … --listen … --state-dir …` runs without one.

**`[relay]`**

| Key | Default | Meaning |
|---|---|---|
| `hostname` | none | The relay's DNS name. Without one the relay runs without DNS. |
| `public_host` | `hostname` | The address phones and hubs connect to. Required without `hostname`. |
| `public_port` | the first `listen` port | The public port, if a port forward sits in front |
| `region` | `default` | A short name shown to hubs, such as `eu` |
| `listen` | `["[::]:443"]` | Where to listen. `[::]` takes IPv4 too. |
| `state_dir` | `/var/lib/twr-relay` | The key, the ACME cache and `state.json` |
| `metrics_listen` | `127.0.0.1:9469` | Counters in Prometheus text at `/metrics`. Must be loopback unless `metrics_public = true`. |
| `phone_suffixes` | `[hostname]`, or `["relay.invalid"]` | The suffixes of phone server names |

**`[tls]`**

| Key | Default | Meaning |
|---|---|---|
| `mode` | `acme` | `acme` (needs `hostname`), `self-signed` (hubs pin the key) or `files` |
| `acme_contact` | none | `mailto:` contact for the ACME account |
| `acme_directory` | Let's Encrypt production | The ACME directory URL |
| `acme_ca_roots` | none | Extra PEM roots for the ACME directory, for a test CA such as pebble |
| `cert`, `key` | none | PEM files, for `mode = "files"`. They are re-read every hour. |

**`[iroh]`**

| Key | Default | Meaning |
|---|---|---|
| `enabled` | `true` | Serve iroh's relay for linked runners |
| `hostname` | `iroh.<hostname>` | iroh's name on the main port |
| `listen` | none | iroh's own port, required without `hostname` (`--install` uses `[::]:7443`) |
| `quic` | `true` | iroh's QUIC address discovery (QAD), UDP |
| `quic_listen` | `[::]:7842` | QAD's address |

**`[limits]`**: the defaults are docs/relay-protocol.md §11's.

| Key | Default |
|---|---|
| `phones_per_hub` | 32 |
| `phone_connections_per_hub` | 8 |
| `monthly_bytes_per_hub` | 20000000000 (20 GB in any 30 days) |
| `bytes_per_second`, `burst_bytes` | 10000000 and 32000000, per connection and direction, and for iroh's relay per client |
| `idle_seconds` | 600 |
| `hello_seconds` | 5 (to a complete ClientHello) |
| `hub_hello_seconds` | 10 (to a valid `hello`) |
| `new_hubs_per_ip_per_hour` | 10 |
| `phone_attempts_per_ip_per_minute` | 60 |
| `pending_per_ip` | 16 |
| `record_puts_per_ip_per_minute` | 60 |
| `pairing_admissions_per_ip_per_hour`, `pair_admissions_per_ip_per_hour` | 10 and 60 |
| `max_connections`, `max_hubs`, `max_records`, `max_admitted` | 20000, 50000, 100000, 200000 |
| `record_ttl_seconds` | 7200 |
| `exempt_addresses` | none: addresses or prefixes the per-address limits skip, such as your monitoring hosts |

An IPv6 address counts as its /64.

**`[deny]`**: `hubs` (hub names), `endpoints` (iroh endpoint ids, 64 hex characters) and `addresses` (addresses or prefixes such as `198.51.100.0/24`). A denied hub or endpoint is refused at authentication, and a denied address before anything is read.

**`[[port_hub]]`**: `hub`, `listen`, `public_port` and `allow_unauthenticated` (default `true`). See [port mode](#without-a-dns-name-what-changes).

## Monitoring

- `twr relay probe <relay URL>` checks a relay end to end and exits 1 if anything fails. `--json` prints one object, `--connect <address>` checks one server before DNS points at it, and `--state <dir>` keeps the probe's keys between runs. Use `--state` for a probe every minute: the relay then sees one hub and one admitted pair rather than a new one each time, which its per-address limits would refuse. With `--state` the probe posts its admission again only when it has under a day left; the iroh check still uses it on every run, and posts it again if the relay has lost it. Also list the monitoring hosts in `limits.exempt_addresses`.
- `curl -s 127.0.0.1:9469/metrics` gives the counters: `twr_relay_hubs_connected`, `twr_relay_phone_connections_active` and `_total`, `twr_relay_bytes_total{direction}`, `twr_relay_refusals_total{reason}`, `twr_relay_records_stored`, `twr_relay_iroh_admitted`, `twr_relay_iroh_denied_total`, `twr_relay_cert_expiry_seconds` (-1 before the first certificate), `twr_relay_up` and `twr_relay_build_info{version}`.

## What the relay keeps

- **Counters** in memory: connections, bytes, refusals by reason.
- **`state.json`** in the state directory, mode 0600:
  - iroh admissions until they expire, at most 30 days;
  - hubs' "moved to" notes for 90 days;
  - per-hub daily byte totals for 30 days, for the monthly quota.
- **Phone credentials** in memory, for 7 days after a hub's session ends.
- **Signed records** in memory, for 2 hours.
- **The log** (the journal) holds start, stop and certificate events. It never holds addresses, server names, tags, keys or content, at any log level. The embedded iroh relay's own log carries endpoint ids and addresses, so `twr relay serve` turns it off whatever `RUST_LOG` says.

If you run a relay for other people, you are processing their IP addresses. Say so in your own privacy notice.

## Troubleshooting

- **`twr relay probe` fails at `info`.** The relay isn't reachable on that port, or the pin is wrong. `journalctl -u twr-relay` shows whether it started. `ss -ltnp | grep 443` shows whether it is listening.
- **ACME never gets a certificate.** Let's Encrypt must reach the relay on port 443 at `hostname` and at `iroh.<hostname>`. Check both DNS records, and that nothing else (a web server, a load balancer that ends TLS) holds port 443. `twr_relay_cert_expiry_seconds` stays at -1 until the first certificate arrives.
- **`phone_round_trip` fails but `hub_session` passes.** The phone route the relay announces (`public_host`, `public_port`) isn't reachable from where the probe runs, for example behind a port forward. Set `public_host` and `public_port` to what the outside sees.
- **Refusals.** `curl -s 127.0.0.1:9469/metrics | grep refusals` names the reason, for example `bad_tag`, `hub_offline`, `phone_rate` or `quota`.
