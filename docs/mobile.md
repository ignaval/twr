# Mobile clients

The iOS and Android apps reuse the desktop React interface, Rust presentation state and CLI command catalog. Capacitor provides the platform shell. The app is a client of a separately running hub; it never runs agents or the hub on the phone. Narrow windows use Chat, Needs you, Threads and More; wider windows retain the desktop sidebar and inspector. Native fold events preserve the current conversation, destination and drafts through display changes.

Opening a terminal gives it the available work area on phones, tablets and unfolded foldables. Collapse it to return to the conversation without stopping the shell. A separating horizontal hinge keeps the terminal within its usable pane; controls and output also adapt when the keyboard reduces that space.

## Local development

From the repository root:

```sh
cargo build -p twr-cli -p twr-app
npm --prefix clients ci
npm --prefix clients run mobile:build
```

Android requires Java 21 and the SDK versions in `clients/mobile/android/variables.gradle`. Set `ANDROID_HOME` or create the ignored `clients/mobile/android/local.properties`. Then run `npm --prefix clients run mobile:android`. The debug APK is `clients/mobile/android/app/build/outputs/apk/debug/app-debug.apk`.

For iOS, install Xcode 27.1 or newer, open it to accept its license and install an iOS Simulator runtime. Run `npm --prefix clients run mobile:ios`, or open `clients/mobile/ios/App/App.xcodeproj`. Simulator builds use local ad hoc signing and a Simulator-only Keychain identity; they need no Apple account or paid signing team. Device builds use the actual team’s provisioning identity. The deployment minimum is iOS 17.4, matching the explicit Safari build target. Android requires a system WebView based on Chromium 119 or newer; the native shell checks this before loading the UI. Building with the 27.1 SDK enables iPhone Duo reserved-region handling. The UIKit bridge observes native fold and camera regions; no experimental Safari flags are required.

Native QA selects installed runtime 27.0 for phone/tablet and stable 27.1 for Duo explicitly. `TWR_IOS_RUNTIME` accepts an exact runtime identifier or version for deliberate alternate-runtime testing; an unavailable runtime fails with installation guidance instead of silently selecting a beta. The QA log records the selected runtime version and build.

Start a debug gateway from your own terminal:

```sh
target/debug/twr-mobile-gateway --development --pair --allow-terminal --allow-admin --binary-dir target/debug
```

On Android, run `adb reverse tcp:47321 tcp:47321`. Pair with `http://127.0.0.1:47321` and the one-use code printed in your terminal. iOS Simulator can reach the Mac's loopback directly. Plain HTTP is permitted only by debug builds and only for loopback. Pairing uses a 10-character, 50-bit code (case insensitive), expiring after five minutes and five incorrect attempts. Anyone who can reach the pairing endpoint can consume those attempts; generate a fresh code if pairing is interrupted. The gateway takes 10 pairing requests a minute from one address (an IPv4 address or an IPv6 /64) and 60 from all, and answers 429 past them; a pairing request has 35 seconds. An address whose socket upgrades presented 30 unknown tokens in a minute gets 429 until the minute ends. Before a device's socket is authenticated, its connection counts against 16 per address and 128 in all (beyond them a new connection is closed at once), and is closed when it sends nothing within 10 seconds, takes more than 10 seconds over a request head (an idle keep-alive connection included) or is still unauthenticated after two minutes. Run `--pair --pair-only` to create a new code without starting another listener.

## Remote hubs

Run the release gateway beside the hub with a certificate trusted by the phone:

```sh
twr-mobile-gateway --listen 0.0.0.0:47321 --tls-cert /absolute/cert.pem --tls-key /absolute/key.pem --pair --allow-terminal --allow-admin
```

Use the DNS name covered by that certificate when pairing. A private VPN such as Tailscale can provide reachability; it does not replace HTTPS certificate validation. No hosted relay is included. Certificates are loaded at gateway startup; restart the gateway after renewing them. No gateway autostart service is installed in this release. The gateway and bundled CLI/bridge must come from the same build. The desktop bundle includes the gateway; Arch also installs its command. On macOS it is available in the app's embedded shell or inside `twr.app/Contents/Resources/bin`.

Pairing grants read and operate by default; `--read-only` creates a read-only invitation. The desktop sharing wizard exposes the same access levels. `--allow-terminal` permits remote shells, attach, jobs and interactive CLI flows. `--allow-admin` permits accounts, boxes, configuration and mode changes; configuration can name commands the hub runs (such as `[notify] command`), so admin includes command execution and requires `--allow-terminal` (the gateway refuses an invitation with admin but not terminal). These are owner-granted capabilities: operate can direct agents under the hub's configured permissions, so it is not an OS sandbox or a guarantee that no code runs. The hub validates each device identity, request scope and approval revision. Owner-side pairing cannot be delegated to a device. List or revoke access with `--list-devices` and `--revoke DEVICE_ID`. Revocation disconnects clients and cleans up their remote terminals. The socket upgrade answers 401 only for a token no paired device has, which clients take as revoked; when the device list cannot be read (an I/O error, say), it answers 503 and clients retry, while open sockets and remote terminals stay and their requests are refused until the list reads again. An Origin the gateway does not accept is 403. Forgetting a hub on the phone removes local credentials. Use **Revoke this device** in the connected hub settings to invalidate access on the host too. The desktop command palette includes pairing, listing and revoking devices.

Tokens are stored in iOS Keychain or Android Keystore-encrypted private storage. Tokens stay in native code; the web UI receives metadata only. Gateway companions receive credentials over private stdin rather than process arguments or environment variables. This protects process inspection, not malicious processes with unrestricted access to the same OS user's files; run untrusted agents in appropriate OS isolation.

Uploads are explicit and never sent as messages automatically. The shared attachment manager provides previews, progress, cancellation and verified resume, with a 25 MiB limit. Local and remote-box workers store files in private git-excluded `.twr-uploads` directories under the selected thread's worktree. See [thread attachments](attachments.md) for security checks and restart limits.

Remote terminals survive temporary connection loss. Mobile controls include Escape, Tab, Control and arrows. A reconnect restores the transcript and terminal history. While the app remains alive, reconnect reuses each pending mutation's original ID and payload. The gateway returns its durable receipt or waits for the original operation, without executing a second copy. Receipts live in the root twr data directory under `mobile/requests`, are bounded and expire after 24 hours. Plain messages also persist in the encrypted outbox across app termination and retain their original IDs for safe retries within 23 hours. Expired or uncertain outcomes require review before a new message is submitted. Other pending operations still require checking hub state after app termination. Gateway restarts end its PTYs; socket reconnects preserve them.

## Voice

The composer's microphone talks to the orchestrator: tap it to talk and tap again to send, hold it and let go, or start a hands-free session that sends one message per utterance and reads the replies aloud. The phone's own recognizer turns speech into text; the text goes through `say` like a typed message (marked as spoken), so a transcript that reads as an answer or a mode change comes back for confirmation instead. No audio leaves the phone except to the platform's recognizer when it is not on the device (below). Saying only "stop", "pause", "skip", "repeat" or "stop listening" controls the voice and is never sent.

| | iOS | Android |
|---|---|---|
| Recognizer | Speech framework (`SFSpeechRecognizer`) from the microphone (`AVAudioEngine`), thread names as contextual strings | `SpeechRecognizer`, thread names as biasing strings on Android 13+ |
| On the device | When the recognizer supports it (`requiresOnDeviceRecognition`); otherwise the audio goes to Apple's servers | With `createOnDeviceSpeechRecognizer` (Android 12+) when the language is installed; otherwise the default recognizer, asked to prefer offline, whose service may use the network |
| Permissions | Speech Recognition and Microphone, asked the first time voice starts | Microphone (`RECORD_AUDIO`), asked the first time voice starts |
| Echo | `.playAndRecord` in `.voiceChat` mode with voice processing: the system's echo cancellation | the recognizer's own audio path; no app-level echo cancellation |
| Voice | `AVSpeechSynthesizer` | `TextToSpeech` |

The voice bar says whether recognition stays on the device; a denied permission shows how to allow it in Settings. Hands-free ends an utterance after about 2 s of silence, so a pause mid-sentence doesn't send half of it. It doesn't listen while the phone reads a reply aloud, nor for 0.8 s after, so the reply heard back is never sent; tap **Quiet** to cut a reply short. If you are already talking when a reply starts, it waits until what you said is sent. Push to talk works while a reply is read: the phone's voice pauses at once. Android has no continuous recognition: hands-free restarts the recognizer after each result, no-match or timeout, with a short pause in which a word can be missed, and some phones play a sound at each restart. Voice is foreground only on both platforms: leaving the app stops listening and the voice, and ends a hands-free session.

### The phone's speaker

A phone can't run `twr speak`, so the gateway runs a speaker for it on the hub's machine. Starting hands-free starts it for the connected workspace and turns live reading on for that phone only (`/read status` lists it as "<device name> phone"; `/read off` turns it off like any speaker, and the next hands-free session turns it on again). It attaches to the hub like `twr speak`, and sends each sentence to the phone, which says it and acknowledges it; controls (pause, skip, stop) act on the gateway's speaker directly, without a hub round trip. Its device id, live switch and the last message it read are the phone's own, in `mobile/speakers/<device>/speech-state.json` under twr's data directory, so it never stands by for the hub machine's own speaker: a TUI on that machine and the phone both read aloud. A sentence waits up to 30 seconds for a lost connection and is said again in full; the speaker ends when the phone stops it, switches workspace, has had no connection for a minute, or is revoked.

Listening needs only the `read` scope: the speaker carries what the hub's speech feed sends a speaker (the orchestrator's finished messages, and those `/read N` asks for), which a read device can already open. Sending what you say needs `operate`, as typing does; asking the hub to repeat a message (`speech.request`) does too. The phone can only start, stop and control its speaker and acknowledge sentences: no call takes text, so it can never make the hub or another speaker say anything. The hub refuses `speech.attach` on a device's own connection, so the gateway attaches its speaker as your user, and checks the device's grant and revocation itself.

Not verified on hardware: real microphone input, recognition, on-device availability, echo cancellation and the platform voices have only been built (iOS Simulator and Android debug builds), not exercised; Simulators and emulators have no usable microphone path in this repository's QA.

## Notifications

**Test notification** in Remote hub settings exercises the native system notification and tap routing without paid credentials. Remote background push requires Apple or Firebase configuration; a local notification test does not prove APNs/FCM delivery.

For Android, add the ignored `clients/mobile/android/app/google-services.json` from a Firebase Android app matching `tools.twr.mobile.dev`. For iOS, use a push-capable Apple team and a local copy of `clients/mobile/ios/push.local.example.xcconfig`; add `DEVELOPMENT_TEAM = YOUR_TEAM_ID` there, then run `npm --prefix clients run mobile:ios:push`. Debug Xcode builds include the local file when present. This opt-in adds `CODE_SIGN_ENTITLEMENTS` and the sandbox `aps-environment`; the default simulator build intentionally omits them. For distribution, set `TWR_APNS_ENVIRONMENT = production` and the gateway APNs `sandbox` value to false, then sign with the corresponding distribution profile. Do not commit signing keys or provisioning files.

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

Omit either provider if unused. Enable remote push in the app after configuring it. Registration is scoped to a device and workspace. After opt-in, the active hub refreshes its registration on connection and foreground resume without prompting again. Token-change events also update the active opted-in hub; a closed hub client cannot submit late requests through another hub’s connection. Device grants hide admin sections and disable terminals when those capabilities were not granted. Notifications use the shared attention policy, hold delays and rate gap; resolved requests are suppressed and active foreground viewers are excluded. Payloads contain routing metadata, not prompts, tool commands or request summaries. Tapping opens Needs you; notification buttons never approve a request.

Dark is the first-launch default. Appearance controls in the hub sheet and pairing screen are local preferences available to every device scope; they do not require hub admin access. Native system bars and backgrounds follow Dark, Light and System. Keyboard-visible layouts hide the bottom navigation and retain a reachable dismiss-keyboard control.

## Verification status

Remote push can be disabled for a hub from its settings. Forget unregisters this device's push tokens when the hub is reachable; if it is offline, the app warns that host-side revocation is still needed. Forget and Revoke both remove that hub's local drafts and editor sessions.

Uploads support ordinary directories as well as Git worktrees. They are limited to 25 MiB per file, four pending uploads and 128 MiB per device over ten minutes. Cancelled uploads, failed commits and expired pending uploads return their reservation; cancelling an already committed file does not. Requests over 2 MiB are rejected locally with a size error, and gateway responses over 16 MiB return a bounded error instead of repeatedly reconnecting.

Android API 36 ARM64 phone and Pixel Fold emulator QA passes real native pairing/storage, device identities, direct chat, approvals, uploads, remote PTYs, reconnect/history, request deduplication and actual Android system notification delivery/tap routing. Cover, unfolded, half-open book and horizontal fold transitions preserve drafts. Additional regression QA verifies ordered keystrokes, 3 MiB output without a full reconnect, a lost mutation reply recovering exactly once, scope-aware controls and cross-hub notification routing. `clients/mobile/scripts/android-qa.mjs` runs against a disposable real Rust hub and fake runtime; use `TWR_ANDROID_SERIAL` to select the emulator and `TWR_SYSTEM_PICKER_QA=1` for the actual document-picker test.

Native iPhone, iPhone Duo and iPad integration passes in the Xcode 27.1 Simulator runner (phone/tablet runtime 27.0, Duo runtime 27.1): Keychain pairing, streaming, revision-bound approvals, uploads, ordered terminal input, a real TCP disconnect/reconnect and a socket surviving beyond 40 seconds. Dark, Light and System native appearance cases pass. Separate Upload/Send hit targets and actual compact-layout sending are verified in native WKWebView. The Duo test dispatches real Simulator hinge events and requires two stable CoreMotion readings before checking viewport changes, UIKit reserved regions, dialogs and preserved drafts.

Credential-free iOS notification QA uses provisional authorization and verifies the native plugin callback against the notification actually delivered to Notification Center. A real iOS Notification Center tap is not automated in this suite; Android system notification delivery and tap routing are tested. Production APNs/FCM delivery, store signing, physical-device behavior and battery/power remain unverified until the appropriate credentials and hardware are available. Local iOS development still requires a full Xcode installation; the repository's Simulator QA runs in CI.

## Shared desktop and mobile improvements

The desktop [remote setup wizard](remote-clients.md) creates expiring QR invitations; mobile can scan them or use manual pairing. Both clients share grouped attention, conversation search, [layout and encrypted offline recovery](desktop.md#layout-and-offline-recovery), and the attachment manager. Settings for appearance, density and text are available to read-only devices without granting hub administration.
