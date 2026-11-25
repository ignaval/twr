# Remote hubs and pairing

On desktop, open the hub switcher (**This computer**) and choose **Let other devices
connect** in the **Computers** dialog. It is three steps: how devices reach this computer,
the device access level, and start. By default no certificate is needed: the gateway
serves this computer's own key, which paired devices pin ([mobile.md](mobile.md#pairing-a-phone)).
Devices on the same network, or on a VPN such as Tailscale, connect to one of this
computer's addresses, and the invitation lists them; the listening IP and port wait under
**Advanced**, `0.0.0.0:47321` by default. After starting, the dialog shows the **Gateway key**
(the fingerprint devices pin) and the **Addresses**. The app checks that the local listener
presents that key and belongs to this installation's gateway. It starts a detached gateway
when needed. Closing the app keeps the gateway, hub and agents running; restart the
computer or use the gateway owner controls to stop it.

To use a certificate your devices already trust instead, check **Advanced › Use my own
certificate**: enter a public HTTPS address and choose the TLS certificate chain and its
private key with **Choose the certificate…** and **Choose the private key…** (or **Edit
path** to type an absolute path). The certificate must cover the address and be trusted by
your devices. The app then checks certificate dates, hostname, matching key, HTTPS
connectivity, gateway protocol and this installation's gateway identity. Settings saved by
an earlier version stay in this mode.

Opening Let other devices connect probes gateway status without starting it. One line
says whether remote access is on, with the running version and listener (the gateway's
recent log is under **Details**), and it offers an explicit restart when configuration or
the installed engine changes. Restarting closes gateway terminal sessions; the hub
continues running. If the replacement fails verification, the app cleans it up and
restores the previous working configuration.
If the running gateway's expired or changed certificate prevents safe rollback,
choose **Stop gateway**, then start it with the replacement certificate. Stopping
remote access disconnects devices and gateway terminals while keeping the hub and
threads running.

The app does not open firewall or router ports. A check from the computer verifies that
computer's route; the phone must also be able to reach one of the addresses. Logs for a
gateway started by the app are in `gateway.log` in the app's data directory, with a
bounded rotated backup. In certificate mode the required connectivity check uses the local
listener with the public hostname for SNI and certificate identity, using the operating
system CA trust; the public route check is advisory, allowing networks without NAT
loopback, and TLS certificate renewals reload automatically (failed renewals keep the
previous TLS context and appear in gateway status).

Choose the device access level, finish the check, then create an invitation. Its QR code
holds the gateway's id, its key's fingerprint, the addresses to try, a one-use code (shown
as `ABCD-EFGH-JK23`) and a five-minute expiration; in certificate mode, the HTTPS URL, the
code and the expiration. **Copy invitation** copies it as a `twr://pair#…` link (in
certificate mode, the same text as the QR code) for another computer.
Creating another code replaces the previous one; closing the dialog cancels its
unused invitation when the gateway is reachable. Otherwise the hub still enforces
the five-minute expiration. The invitation shows its granted access, and the hub consumes
a code once.
On iOS or Android, choose **Pair a hub → Scan QR code**, confirm the hub/device
names, and pair. You can still enter the address and code manually: a hub's address or
IP (with `:port` if it isn't 47321) pairs by its key, checked against the code before the
code is sent; an `https://` address pairs in certificate mode. The scanner
uses native camera capture; Android keeps its minimum API level at 24.

Under **Connect to another computer**, paste that invitation (the link, or either
version's JSON) into **Paste invitation**: it fills the hub's address and code, and names
the hub after its host. **Pair and
connect** reads the invitation as the box holds it then: text that isn't a valid
invitation is refused with what to do, and clearing the box clears the address and code it
filled. To type them instead, leave the box empty and open **Enter details manually** (hub
name, hub address, one-use code and this device's name): a host or IP pairs by the hub's
key, an `https://` address in certificate mode. The same tab lists your saved hubs, to
connect to or forget; a hub paired by its key shows **Key and addresses**: its fingerprint
in groups of four, the address in use and the known ones. Both apps reconnect at once
when the network changes (on the desktop, when the computer's addresses change).
Desktop and mobile use the same remote protocol and shared reconnect/framing
logic. Tokens stay in native storage: Electron safeStorage with plaintext
fallback refused, iOS Keychain, or Android Keystore encryption. Tokens never
cross the renderer bridge or appear in a QR code. Cached history uses a separate
encrypted native store. On Omarchy, the app selects GNOME Secret Service unless a password-store option was explicitly supplied. Unlock the keyring before pairing; the app refuses the plaintext backend. Other Linux sessions can use GNOME Keyring or KWallet.

Switching hosts fences old callbacks and removes their transport listeners.
Switching hosts preserves local and remote terminals. Return to the original host to resume their views; quitting the desktop app still asks before closing its local terminals. Agents running in independent hubs continue. Permissions are
checked by the hub for each operation; a read-only device cannot mutate threads,
change settings or open a terminal. Without terminal access a device cannot start or
move a thread onto a command line runtime (`pty:` or `acp:`), only onto an agent
runtime or a configured preset; without admin access it cannot change a thread's
permission mode or move it to another runtime by any path, including a typed command,
the orchestrator acting on its message, or confirming a card
([mobile.md](mobile.md#access-and-revocation)). Forgetting a saved connection removes its
local credentials and encrypted offline state; revoke the device on the hub to
remove its server-side access. A client stops reconnecting and asks you to pair
again only when the gateway answers 401 (it no longer knows the device's token).
Any other refusal (403, a 5xx while the hub is busy) and network loss keep
retrying with backoff, up to 30 seconds apart.

Validation: `npm test -- tests/connections.test.ts tests/desktop-remote.test.ts
 tests/gateway-setup.test.ts tests/desktop-inbox.test.ts tests/mobile-transport.test.ts`.
Native camera checks require an actual camera-enabled iOS/Android device; the
manual code path works in simulators.
