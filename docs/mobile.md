# Mobile clients

The iOS and Android apps reuse the desktop React interface, Rust presentation state and CLI command catalog. Capacitor provides the platform shell. The app is a client of a separately running hub; it never runs agents or the hub on the phone. Narrow windows use Chat, Needs you, Threads and More; wider windows retain the desktop sidebar and inspector. Native fold events preserve the current conversation, destination and drafts through display changes. Request buttons behave as on the desktop: a tap answers only once the request's options have been on screen for 600 ms, including after the app returns to the foreground ([desktop workflows](desktop.md#workflows-and-cli-coverage)).

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

Native QA prefers runtime 27.0 for phone/tablet and 27.1 for Duo. When that runtime is not installed (each Xcode installs only its own, such as 27.2), it uses the newest installed runtime that has a device of the kind being tested and logs which one. `TWR_IOS_RUNTIME` chooses deliberately: an exact runtime identifier is used as it is or the run fails with installation guidance; a version that several runtimes share (Xcode 27.2's `iOS-27-2` and the Duo's `iOS-27-2-iPhone19-4`) resolves to the one with a device of that kind, the plain `iOS-<major>-<minor>` runtime for phone and tablet. The QA log records the selected runtime version and build.

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

Use the DNS name covered by that certificate when pairing. A private VPN such as Tailscale can provide reachability; it does not replace HTTPS certificate validation. No hosted relay is included. The gateway reloads its certificate and key when they change, checking every 5 seconds or at once on SIGHUP, with no restart; a renewal it cannot read or load keeps the previous certificate and shows as failed in gateway status. No gateway autostart service is installed in this release. The gateway and bundled CLI/bridge must come from the same build. The desktop bundle includes the gateway; Arch also installs its command. On macOS it is available in the app's embedded shell or inside `twr.app/Contents/Resources/bin`.

Pairing grants read and operate by default; `--read-only` creates a read-only invitation. The desktop sharing wizard exposes the same access levels. `--allow-terminal` permits remote shells, attach, jobs and interactive CLI flows, and threads whose runtime is a command line (`pty:<command>`, `acp:<command>`, for a new thread, a move or the orchestrator). Without it a device picks only a built-in agent runtime (Claude Code, Codex, Grok Build, with an account) or a `[runtimes.<name>]` preset you configured, or leaves the runtime to the project's. `--allow-admin` permits accounts, boxes, configuration and mode changes, and the hub holds every path to a mode change to it: `twr mode`, a new thread in a mode other than `manual`, `plan`, `read-only` or `dontAsk`, a short command such as "set api to bypass" (refused, nothing sent), the orchestrator acting on the device's message (it is refused the change), and Confirm on a card that changes a mode (the card stays open for you or an admin device). Without admin a device also starts threads only in a configured project, or in a folder or git repository that isn't (and doesn't hold) your home folder and keeps clear of twr's data, `TWR_HOME` and credentials folders such as `~/.ssh`; anywhere else is refused with what to do instead. A folder outside git is fine, such as `~/notes`. Moving a thread to another runtime (yourself, or by asking the orchestrator for a move it would put on a card) and opening or moving a thread outside an account pin need admin the same way. Configuration can name commands the hub runs (such as `[notify] command`), so admin includes command execution and requires `--allow-terminal` (the gateway refuses an invitation with admin but not terminal). The only twr commands the gateway runs for a device outside a terminal are `twr ws list [--json]` (read) and `twr ws new <name>` (operate), exactly so: any other subcommand, flag (`ws list --detail`, which prints every hub's threads and requests as you read them, included) or argument is refused, and the rest of the CLI runs in a remote terminal, which needs terminal. These are owner-granted capabilities: operate can direct agents under the hub's configured permissions, so it is not an OS sandbox or a guarantee that no code runs. The hub validates each device identity, request scope and approval revision. Owner-side pairing cannot be delegated to a device. List or revoke access with `--list-devices` and `--revoke DEVICE_ID`. Revocation disconnects clients and cleans up their remote terminals. The socket upgrade answers 401 only for a token no paired device has, which clients take as revoked; when the device list cannot be read (an I/O error, say), it answers 503 and clients retry, while open sockets and remote terminals stay and their requests are refused until the list reads again. An Origin the gateway does not accept is 403. Forgetting a hub on the phone removes local credentials. Use **Revoke this device** in the connected hub settings to invalidate access on the host too. The desktop command palette includes pairing, listing and revoking devices.

On launch the app connects to the hub it was last connected to, if it is still paired; an unreachable hub opens with its saved history, as tapping **Connect** does. It tries once: if that fails, the hub list shows the error. The hub button in the header leads back to the list (**Switch hub**). Switching hub, Forget and Revoke clear the remembered hub, so the next launch opens the list.

A transcript export is written as plain text to the app's private cache (`exports/`) only for the share sheet: it is deleted when the sheet closes (on Android a minute later, for the app you shared it with to read it), and the folder is emptied at launch and when you forget a hub.

Tokens are stored in iOS Keychain or Android Keystore-encrypted private storage. Tokens stay in native code; the web UI receives metadata only. On Android, none of the app's data goes into a backup or a device-to-device transfer: its Keystore key never leaves the phone, so saved hubs could not be decrypted on a new one. If they can't be (restored from another phone, or the key was lost), Android drops them and says "Saved hubs couldn't be restored on this device; pair again." Gateway companions receive credentials over private stdin rather than process arguments or environment variables. This protects process inspection, not malicious processes with unrestricted access to the same OS user's files; run untrusted agents in appropriate OS isolation.

Uploads are explicit and never sent as messages automatically. The shared attachment manager provides previews, progress, cancellation and verified resume, with a 25 MiB limit. Local and remote-box workers store files in private git-excluded `.twr-uploads` directories under the selected thread's worktree. See [thread attachments](attachments.md) for security checks and restart limits.

Thirty seconds after the app leaves the foreground it closes its hub connection, so the gateway's heartbeats stop waking the radio; returning to the app reconnects. Remote terminals survive temporary connection loss. Mobile controls include Escape, Tab, Control and arrows. A reconnect restores the transcript and terminal history. While the app remains alive, reconnect reuses each pending mutation's original ID and payload. The gateway returns its durable receipt or waits for the original operation, without executing a second copy. Receipts live in the root twr data directory under `mobile/requests`, are bounded and expire after 24 hours. Plain messages also persist in the encrypted outbox across app termination and retain their original IDs for safe retries within 23 hours. Expired or uncertain outcomes require review before a new message is submitted. Other pending operations still require checking hub state after app termination. Gateway restarts end its PTYs; socket reconnects preserve them.

The message field grows with what you write, wrapped lines included, at every text size up to the system's largest: to four lines (three on a very short screen) and never past 40% of the height the keyboard leaves, then the text scrolls inside it. It shrinks again when the text is deleted or sent. Upload, the microphone, hands-free and Send stay above the keyboard, on their own row under the message below 520 points of width.

## Voice

The composer's microphone talks to the orchestrator: tap it to talk and tap again to send, hold it and let go, or start a hands-free session that sends one message per utterance and reads the replies aloud. The phone's own recognizer turns speech into text; the text goes through `say` like a typed message (marked as spoken), so a transcript that reads as an answer or a mode change comes back for confirmation instead. No audio leaves the phone except to the platform's recognizer when it is not on the device (below). Saying only "stop", "pause", "skip", "repeat" or "stop listening" controls the voice and is never sent.

| | iOS | Android |
|---|---|---|
| Recognizer | Speech framework (`SFSpeechRecognizer`) from the microphone (`AVAudioEngine`), thread names as contextual strings | `SpeechRecognizer`, thread names as biasing strings on Android 13+ |
| On the device | When the recognizer supports it (`requiresOnDeviceRecognition`); otherwise the audio goes to Apple's servers | With `createOnDeviceSpeechRecognizer` (Android 12+) when the language is installed; otherwise the default recognizer, asked to prefer offline, whose service may use the network |
| Permissions | Speech Recognition and Microphone, asked the first time voice starts | Microphone (`RECORD_AUDIO`), asked the first time voice starts |
| Echo | `.playAndRecord` in `.voiceChat` mode with voice processing: the system's echo cancellation | the recognizer's own audio path; no app-level echo cancellation |
| Voice | `AVSpeechSynthesizer` | `TextToSpeech` |

The voice bar says whether recognition stays on the device; a denied permission shows how to allow it in Settings. Hands-free ends an utterance after about 2 s of silence, so a pause mid-sentence doesn't send half of it. It doesn't listen while the phone reads a reply aloud, nor for 0.8 s after, so the reply heard back is never sent; tap **Quiet** to cut a reply short. If you are already talking when a reply starts, it waits until what you said is sent. Push to talk works while a reply is read: the phone's voice pauses at once. Android has no continuous recognition: hands-free restarts the recognizer after each result, no-match or timeout, with a short pause in which a word can be missed, and some phones play a sound at each restart. Voice is foreground only on both platforms: leaving the app stops listening and the voice, and ends a hands-free session. On iOS, voice mode gives the audio session back once it stops listening and speaking, so music or a podcast it paused resumes. A call, Siri or another app's audio stops listening and the voice, and the voice bar says so; a hands-free session listens again when the interruption ends, if iOS says it may resume. Plugging in or removing headphones restarts the microphone in hands-free, and ends a push-to-talk recording with what it heard.

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

Omit either provider if unused. Enable remote push in the app after configuring it. Registration is scoped to a device and workspace. After opt-in, the active hub refreshes its registration, without prompting again, each time a workspace connects (the first connection, switching workspace in the app, and every reconnect) and on foreground resume, so every workspace you open on the phone can notify it. Token-change events also update the active opted-in hub; a closed hub client cannot submit late requests through another hub’s connection. Device grants hide admin sections and disable terminals when those capabilities were not granted. Notifications use the shared attention policy, hold delays and rate gap; resolved requests are suppressed and active foreground viewers are excluded. Payloads contain routing metadata, not prompts, tool commands or request summaries. Tapping opens Needs you; notification buttons never approve a request. Opening the app or tapping a notification removes twr's delivered notifications, and a new push for a hub's workspace replaces the one still shown: APNs `apns-collapse-id` and `thread-id` and the FCM `tag` are a hash of the gateway id and workspace, which carries no content.

Dark is the first-launch default. Appearance controls in the hub sheet and pairing screen are local preferences available to every device scope; they do not require hub admin access. Native system bars and backgrounds follow Dark, Light and System. Keyboard-visible layouts hide the bottom navigation and retain a reachable dismiss-keyboard control.

## Publishing

The store builds are separate from the development builds:

| | Development build | Store build |
|---|---|---|
| Application id | `tools.twr.mobile.dev` | `tools.twr.mobile` |
| Made by | `npm run mobile:android`, `npm run mobile:ios` (Debug) | `npm run mobile:release:android`, `npm run mobile:release:ios` (Release) |
| Remote push | Yours to configure ([Notifications](#notifications)) | None: no push entitlement or Firebase app, and the app shows no **Enable remote push** |
| Plain HTTP to the phone itself | Allowed | Refused |

Both can be installed on one phone; each keeps its own hubs and data. A store build has no remote push because only its publisher's Apple and Firebase keys can reach it, and a gateway you host has none of them: its notifications arrive while the app is connected. `npm run mobile:build:store` is the web layer of a store build (`TWR_MOBILE_APP_ID=tools.twr.mobile`, `VITE_TWR_REMOTE_PUSH=0`); the two release commands run it first.

The version is the clients' (`clients/package.json`). The build number both stores want to grow with every upload is made from it: 0.4.0 is 40000, and `TWR_MOBILE_BUILD_NUMBER` (0 to 99) is added to it for another upload of the same version (`node mobile/scripts/build-number.mjs` prints it).

### Android

`npm run mobile:release:android` writes `clients/mobile/android/app/build/outputs/bundle/release/app-release.aab`. It is signed when `TWR_ANDROID_KEYSTORE` names a keystore file, with `TWR_ANDROID_KEYSTORE_PASSWORD`, `TWR_ANDROID_KEY_ALIAS` and `TWR_ANDROID_KEY_PASSWORD`; without them it is unsigned, which is what CI compiles. The keystore is the upload key: Google Play signs what phones install with its own key (Play App Signing). Make one once and keep it and its passwords outside the repository:

```sh
keytool -genkeypair -keystore ~/.config/twr-release/android-upload.jks -alias upload -keyalg RSA -keysize 4096 -validity 10000
```

A local `google-services.json` made for `tools.twr.mobile.dev` stops a release build, which looks in it for `tools.twr.mobile`: move it aside for the release build.

### iOS

`npm run mobile:release:ios` archives the Release configuration, signs it with Xcode's automatic signing for the team in `TWR_IOS_TEAM_ID`, and exports an `.ipa` to `clients/mobile/ios/App/output/`. After `npm run mobile:build:store`, `node mobile/scripts/ios-release.mjs --upload` sends it to App Store Connect instead, and `--check` archives without signing, with no Apple account. On a Mac whose Xcode is signed in to the team nothing else is needed. Elsewhere an App Store Connect API key signs and uploads: `TWR_ASC_KEY_PATH` (the `.p8` file, an absolute path), `TWR_ASC_KEY_ID` and `TWR_ASC_ISSUER_ID`. The app's record, with the bundle id `tools.twr.mobile`, has to exist in App Store Connect before the first upload.

`Info.plist` declares `ITSAppUsesNonExemptEncryption` false: the app's only encryption is the system's (HTTPS, the Keychain, CryptoKit for its own files). Review that declaration if the app ever ships encryption of its own.

### From GitHub

`.github/workflows/mobile-release.yml` makes both store builds from `main`, by hand: `gh workflow run mobile-release.yml --ref main -f build=0`. Its jobs run in the protected `release` environment and wait for a maintainer's approval. The Android job keeps the signed bundle as the `twr-android-release` artifact, to upload in the Play Console, where a new app's first bundle has to be uploaded by hand. The iOS job uploads to App Store Connect, where the build appears in TestFlight once processed. The environment's secrets:

- `ANDROID_UPLOAD_KEYSTORE` (the keystore file in base64), `ANDROID_UPLOAD_KEYSTORE_PASSWORD`, `ANDROID_UPLOAD_KEY_ALIAS`, `ANDROID_UPLOAD_KEY_PASSWORD`.
- `APPLE_TEAM_ID` (the desktop app's signing uses it too), `ASC_API_KEY` (the contents of the `.p8` file), `ASC_API_KEY_ID`, `ASC_API_ISSUER_ID`. The key needs a role that may create signing certificates and upload builds (Admin does).

A job whose secrets are missing fails and names them.

### Not verified

The signed paths have not run with real credentials: Xcode's signing and the upload to App Store Connect, the workflow, and a bundle accepted by Google Play. What has run: the store web layer, a release bundle signed with a throwaway key (application id `tools.twr.mobile`, version code 40003 for build 3), and an unsigned archive of the iOS Release configuration.

Both stores also ask for things outside the repository: a privacy policy ([privacy.md](privacy.md)), screenshots, the stores' own privacy forms, and a way for their reviewers to use an app that does nothing until it is paired with a hub.

## Verification status

Remote push can be disabled for a hub from its settings. Forget unregisters this device's push tokens when the hub is reachable; if it is offline, the app warns that host-side revocation is still needed. Forget and Revoke both remove that hub's local drafts and editor sessions.

Uploads support ordinary directories as well as Git worktrees. They are limited to 25 MiB per file, four pending uploads and 128 MiB per device over ten minutes. Cancelled uploads, failed commits and expired pending uploads return their reservation; cancelling an already committed file does not. Requests over 2 MiB are rejected locally with a size error, and gateway responses over 16 MiB return a bounded error instead of repeatedly reconnecting.

Android API 36 ARM64 phone and Pixel Fold emulator QA passes real native pairing/storage, device identities, direct chat, approvals, uploads, remote PTYs, reconnect/history, request deduplication and actual Android system notification delivery/tap routing. Cover, unfolded, half-open book and horizontal fold transitions preserve drafts. Additional regression QA verifies ordered keystrokes, 3 MiB output without a full reconnect, a lost mutation reply recovering exactly once, scope-aware controls and cross-hub notification routing. `clients/mobile/scripts/android-qa.mjs` runs against a disposable real Rust hub and fake runtime; use `TWR_ANDROID_SERIAL` to select the emulator and `TWR_SYSTEM_PICKER_QA=1` for the actual document-picker test.

Native iPhone, iPhone Duo and iPad integration passes in the Xcode 27.1 Simulator runner (phone/tablet runtime 27.0, Duo runtime 27.1): Keychain pairing, streaming, revision-bound approvals, uploads, ordered terminal input, a real TCP disconnect/reconnect and a socket surviving beyond 40 seconds. Dark, Light and System native appearance cases pass. Separate Upload/Send hit targets and actual compact-layout sending are verified in native WKWebView. The Duo test dispatches real Simulator hinge events and requires two stable CoreMotion readings before checking viewport changes, UIKit reserved regions, dialogs and preserved drafts.

Review loops run on the phone in both suites (`clients/mobile/scripts/loop-qa.mjs`), against the fixture hub with a fake thread and a fake Codex reviewer: the Review loop button on the card and the report, the setup card as a sheet (a tap in its first 600 ms answers nothing; it traps focus and gives it back), the editor, a running loop's chip and inspector with Pause, Resume and Stop, its end, a second run without a card, Settings › Loops, no start or answer while offline, and a `read` device that sees all of it and changes nothing. Each step saves full-screen PNGs in light and dark (the sheet and editor also sideways and at the largest text) to `TWR_LOOP_QA_SHOTS` (default `clients/test-results/loops`), and fails on what an audit of the screen finds: an unnamed control, a target under 44 points, low contrast, clipped text, a control under a system area or under the tab bar. On iOS the host drives XCTest's `testReviewLoops` and its taps are the page's own events (an app-hosted XCTest cannot touch the screen); on Android they are `adb` taps. `TWR_IOS_LOOP_ONLY=1` runs only the loop test. After the loops, the same audit covers the surfaces every screen shares (`surfacesQA`): the thread cards in several colours, a thread's details, the conversation's header and the whole composer with a draft and the keyboard open, in portrait, sideways, at the largest text and, on Android, at 390 and 360 points (`wm size`), saved as `<platform>-surfaces-*.png`. At each of those the composer also gets a message that wraps and a long one: the field must grow with the wrapped lines, stop at its cap and scroll inside, shrink back when the text goes, and keep every control above the keyboard; the iOS Simulator shows no software keyboard, so there the composer gets the height a keyboard leaves. `TWR_SURFACES_QA_ONLY=1` runs only that audit (on iOS with `TWR_IOS_LOOP_ONLY=1`). A loop's end shows on its card, in the inspector and in Latest thread reports (an implementation loop's commits, then `not committed: …` for any repository that was not); it sends no phone notification, since mobile push covers requests only. The phone suites do not drive an implementation loop yet: its surfaces are covered by the unit tests and the desktop's `scripts/loops-integration.mjs`, which share the phone's code.

Credential-free iOS notification QA uses provisional authorization and verifies the native plugin callback against the notification actually delivered to Notification Center. A real iOS Notification Center tap is not automated in this suite; Android system notification delivery and tap routing are tested. Production APNs/FCM delivery, store signing, physical-device behavior and battery/power remain unverified until the appropriate credentials and hardware are available. Local iOS development still requires a full Xcode installation; the repository's Simulator QA runs in CI.

## Shared desktop and mobile improvements

The desktop [remote setup wizard](remote-clients.md) creates expiring QR invitations; mobile can scan them or use manual pairing. Both clients share grouped attention, conversation search, [layout and encrypted offline recovery](desktop.md#layout-and-offline-recovery), the attachment manager, and [loops](desktop.md#loops): the Review loop button, Send as implementation loop (on a phone a `↻` button beside Send, which opens the choice), the loop chip, the inspector's Loops section with **Review loop** and **Implementation loop…**, the New thread form's **Run as an implementation loop** switch, the setup card (a sheet on a phone, with **Run without a loop** when it holds a task, whose text scrolls inside it, and a **Sweep** row when the hub audits the sweep), the sweep's audits listed in the inspector like rounds, **Open report** (the loop's `report.md`, read through the hub and shown in a sheet on a phone, since a phone cannot open the hub's files; a `read` device can open it too) and Settings › Loops. Starting or controlling a loop, answering its setup card and saving loop settings need `operate` and a live connection. On a phone the inspector covers the conversation, so a notice such as `api started a review loop` shows at the top of the inspector, in its flow, rather than as a toast over its controls. Settings for appearance, density and text are available to read-only devices without granting hub administration.
