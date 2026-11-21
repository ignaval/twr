# Remote hubs and pairing

On desktop, open the hub switcher (**This computer**) and choose **Let other devices
connect** in the **Computers** dialog. It is three steps. First enter a public HTTPS
address and choose the TLS certificate chain and its private key with **Choose the
certificate…** and **Choose the private key…** (or **Edit path** to type an absolute
path); the listening IP and port wait under **Advanced**, `0.0.0.0:47321` by default.
Then choose the device access level, and start. The certificate must cover the address
and be trusted by your devices. The app checks certificate dates, hostname, matching key, HTTPS
connectivity, gateway protocol and this installation's gateway identity. It
starts a detached gateway when needed. Closing the app keeps the gateway, hub
and agents running; restart the computer or use the gateway owner controls to stop it.
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

The app does not provision certificates or open firewall/router ports. A check
from the computer verifies that computer's route; the phone must also be able to
reach the address. Use the same address your other device will use. Logs for a
gateway started by the app are in `gateway.log` in the app's data directory, with a bounded rotated backup. The required connectivity check
uses the local listener with the public hostname for SNI and certificate identity,
using the operating system CA trust. The public route check is advisory, allowing
networks without NAT loopback. TLS certificate renewals reload automatically; failed
renewals keep the previous TLS context and appear in gateway status.

Choose the device access level, finish the check, then create an invitation. Its
QR code contains only the HTTPS URL, a one-use code and a five-minute expiration;
**Copy invitation** copies the same text for another computer.
Creating another code replaces the previous one; closing the dialog cancels its
unused invitation when the gateway is reachable. Otherwise the hub still enforces
the five-minute expiration. The invitation shows its granted access, and the hub consumes
a code once.
On iOS or Android, choose **Pair a hub → Scan QR code**, confirm the hub/device
names, and pair. You can still enter the address and code manually. The scanner
uses native camera capture; Android keeps its minimum API level at 24.

Under **Connect to another computer**, paste that invitation into **Paste invitation**:
it fills the hub's address and code, and names the hub after its host. **Pair and
connect** reads the invitation as the box holds it then: text that isn't a valid
invitation is refused with what to do, and clearing the box clears the address and code it
filled. To type them instead, leave the box empty and open **Enter details manually** (hub
name, HTTPS hub URL, one-use code and this device's name). The same tab lists your saved hubs, to connect to or forget.
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
([mobile.md](mobile.md#remote-hubs)). Forgetting a saved connection removes its
local credentials and encrypted offline state; revoke the device on the hub to
remove its server-side access. A client stops reconnecting and asks you to pair
again only when the gateway answers 401 (it no longer knows the device's token).
Any other refusal (403, a 5xx while the hub is busy) and network loss keep
retrying with backoff, up to 30 seconds apart.

Validation: `npm test -- tests/connections.test.ts tests/desktop-remote.test.ts
 tests/gateway-setup.test.ts tests/desktop-inbox.test.ts tests/mobile-transport.test.ts`.
Native camera checks require an actual camera-enabled iOS/Android device; the
manual code path works in simulators.
