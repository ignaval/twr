# Mobile clients

The iOS and Android apps reuse the desktop React interface, Rust presentation state and CLI command catalog. Capacitor provides the platform shell. The app is a client of a separately running hub; it never runs agents or the hub on the phone. Narrow windows use Chat, Needs you, Threads and More; wider windows retain the desktop sidebar and inspector. Native fold events preserve the current conversation, destination and drafts through display changes.

## Local development

From the repository root:

```sh
cargo build -p twr-cli -p twr-app
npm --prefix desktop ci
npm --prefix desktop run mobile:build
```

Android requires Java 21 and the SDK versions in `desktop/mobile/android/variables.gradle`. Set `ANDROID_HOME` or create the ignored `desktop/mobile/android/local.properties`. Then run `npm --prefix desktop run mobile:android`. The debug APK is `desktop/mobile/android/app/build/outputs/apk/debug/app-debug.apk`.

For iOS, install Xcode 27.1 or newer, open it to accept its license and install an iOS Simulator runtime. Run `npm --prefix desktop run mobile:ios`, or open `desktop/mobile/ios/App/App.xcodeproj`. Simulator builds need no paid signing team. The deployment minimum is iOS 15. Building with the 27.1 SDK enables iPhone Duo reserved-region handling. The UIKit bridge observes native fold and camera regions; no experimental Safari flags are required.

Start a debug gateway from your own terminal:

```sh
target/debug/twr-mobile-gateway --development --pair --allow-terminal --allow-admin --binary-dir target/debug
```

On Android, run `adb reverse tcp:47321 tcp:47321`. Pair with `http://127.0.0.1:47321` and the one-use code printed in your terminal. iOS Simulator can reach the Mac's loopback directly. Plain HTTP is permitted only by debug builds and only for loopback. Pairing uses a 10-character, 50-bit code (case insensitive), expiring after five minutes and five incorrect attempts. Anyone who can reach the pairing endpoint can consume those attempts; generate a fresh code if pairing is interrupted. Run `--pair --pair-only` to create a new code without starting another listener.

## Remote hubs

Run the release gateway beside the hub with a certificate trusted by the phone:

```sh
twr-mobile-gateway --listen 0.0.0.0:47321 --tls-cert /absolute/cert.pem --tls-key /absolute/key.pem --pair --allow-terminal --allow-admin
```

Use the DNS name covered by that certificate when pairing. A private VPN such as Tailscale can provide reachability; it does not replace HTTPS certificate validation. No hosted relay is included. Certificates are loaded at gateway startup; restart the gateway after renewing them. No gateway autostart service is installed in this release. The gateway and bundled CLI/bridge must come from the same build. The desktop bundle includes the gateway; Arch also installs its command. On macOS it is available in the app's embedded shell or inside `twr.app/Contents/Resources/bin`.

Pairing always grants read and operate. `--allow-terminal` permits remote shells, attach, jobs and interactive CLI flows. `--allow-admin` permits accounts, boxes, configuration and mode changes. These are owner-granted capabilities: operate can direct agents under the hub's configured permissions, so it is not an OS sandbox or a guarantee that no code runs. The hub validates each device identity, request scope and approval revision. Owner-side pairing cannot be delegated to a device. List or revoke access with `--list-devices` and `--revoke DEVICE_ID`. Revocation disconnects clients and cleans up their remote terminals. Forgetting a hub on the phone removes local credentials. Use **Revoke this device** in the connected hub settings to invalidate access on the host too. The desktop command palette includes pairing, listing and revoking devices.

Tokens are stored in iOS Keychain or Android Keystore-encrypted private storage. Tokens stay in native code; the web UI receives metadata only. Gateway companions receive credentials over private stdin rather than process arguments or environment variables. This protects process inspection, not malicious processes with unrestricted access to the same OS user's files; run untrusted agents in appropriate OS isolation.

Uploads are explicit and never sent as messages automatically. Files are limited to 25 MiB, checked by size and SHA-256, and stored in private, git-excluded `.twr-uploads` directories in the selected local worker's worktree. Uploading directly into a worker on a remote box is not supported yet; the UI reports this instead of writing to the wrong machine.

Remote terminals survive temporary connection loss. Mobile controls include Escape, Tab, Control and arrows. A reconnect restores the transcript and terminal history. While the app remains alive, reconnect reuses each pending mutation's original ID and payload. The gateway returns its durable receipt or waits for the original operation, without executing a second copy. Receipts live in the root twr data directory under `mobile/requests`, are bounded and expire after 24 hours. If the app or gateway was terminated with an uncertain outcome, check hub state before manually submitting the operation again. Gateway restarts end its PTYs; socket reconnects preserve them.

## Notifications

**Test notification** in Remote hub settings exercises the native system notification and tap routing without paid credentials. Remote background push requires Apple or Firebase configuration; a local notification test does not prove APNs/FCM delivery.

For Android, add the ignored `desktop/mobile/android/app/google-services.json` from a Firebase Android app matching `tools.twr.mobile.dev`. For iOS, use a push-capable Apple team and a local copy of `desktop/mobile/ios/push.local.example.xcconfig`; add `DEVELOPMENT_TEAM = YOUR_TEAM_ID` there, then run `npm --prefix desktop run mobile:ios:push`. Debug Xcode builds include the local file when present. This opt-in adds `CODE_SIGN_ENTITLEMENTS` and the sandbox `aps-environment`; the default simulator build intentionally omits them. For distribution, set `TWR_APNS_ENVIRONMENT = production` and the gateway APNs `sandbox` value to false, then sign with the corresponding distribution profile. Do not commit signing keys or provisioning files.

The gateway accepts `--push-config /absolute/push.json`. That file and any private keys must be owned by you with mode 0600. Example structure:

```json
{
  "apns": {
    "key_file": "/absolute/AuthKey.p8",
    "key_id": "YOUR_KEY_ID",
    "team_id": "YOUR_TEAM_ID",
    "topic": "tools.twr.mobile.dev",
    "sandbox": true
  },
  "fcm_service_account": "/absolute/firebase-service-account.json"
}
```

Omit either provider if unused. Enable remote push in the app after configuring it. Registration is scoped to a device and workspace. Device grants hide admin sections and disable terminals when those capabilities were not granted. Notifications use the shared attention policy, hold delays and rate gap; resolved requests are suppressed and active foreground viewers are excluded. Payloads contain routing metadata, not prompts, tool commands or request summaries. Tapping opens Needs you; notification buttons never approve a request.

## Verification status

Android API 36 ARM64 phone and Pixel Fold emulator QA passes real native pairing/storage, device identities, direct chat, approvals, uploads, remote PTYs, reconnect/history, request deduplication and actual Android system notification delivery/tap routing. Cover, unfolded, half-open book and horizontal fold transitions preserve drafts. Additional regression QA verifies ordered keystrokes, 3 MiB output without a full reconnect, a lost mutation reply recovering exactly once, scope-aware controls and cross-hub notification routing. `desktop/mobile/scripts/android-qa.mjs` runs against a disposable real Rust hub and fake runtime; use `TWR_ANDROID_SERIAL` to select the emulator and `TWR_SYSTEM_PICKER_QA=1` for the actual document-picker test.

On this Mac, Xcode installation through the App Store CLI stalled for over an hour and was stopped. Swift syntax, plist and project-file checks passed, but **iOS has not been compiled or run**. iOS phone, iPad and Duo simulator QA remain blocked on Xcode and its runtimes. Production APNs/FCM delivery and store signing also remain unverified until credentials are configured. The app must not be described as verified on both platforms yet.
