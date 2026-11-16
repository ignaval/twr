# Privacy

Last updated: 2026-11-11.

twr is software you run yourself: a hub on your own machine, and clients (the terminal UI, the desktop app, the iOS and Android apps) that talk to it. The project runs no service that your data passes through, has no accounts, and collects no analytics, telemetry or crash reports. This page says what the apps do with your data, permission by permission.

## What the project receives

Nothing. The apps do not send your conversations, files, voice, device identifiers or usage to the twr project or to anyone acting for it.

## Where your data goes

- **Your hubs.** The iOS and Android apps connect only to the hubs you pair them with, through the gateway you run beside each hub. Messages, approvals, uploads and terminal sessions go between your phone and that gateway and nowhere else. Over a network the connection is HTTPS with a certificate your phone trusts; the app refuses plain HTTP except, in development builds, to the phone itself.
- **Your agents.** A hub runs the agent programs you chose (Claude Code, Codex, Grok Build, or another) under your own accounts with their providers. What an agent sends to its provider is between you and that provider, under that provider's terms. twr adds nothing to it.
- **On the phone.** The pairing token of each hub is kept in the iOS Keychain or in storage encrypted with an Android Keystore key. The history saved for reading offline is encrypted with a key held in the Keychain or the Android Keystore; drafts and settings are kept in the app's private storage. A hub's token, history and drafts are removed when you forget it, and everything when you delete the app. On Android none of it is included in backups or device-to-device transfers.

## Permissions

| Permission | What the app uses it for |
|---|---|
| Camera | Scanning a hub's one-use pairing code, and taking a photo or video you attach to a thread. |
| Photos | Choosing a photo or video you attach to a thread. |
| Microphone | Voice mode, and the sound of a video you record to attach. |
| Speech recognition | Turning what you say in voice mode into a message. |
| Local network | Reaching a hub on the network your phone is on. |
| Notifications | Telling you a thread needs you. |

An attachment goes to the hub you are connected to and is stored where that hub's thread works.

## Voice

Voice mode uses the phone's own speech recognition (Apple's Speech framework, Android's `SpeechRecognizer`). Where the phone can recognize speech on the device, the audio does not leave it. Where it cannot, the platform sends the audio to Apple or to Google to recognize it, under their terms; the voice bar says which is in use before you speak. twr keeps no recording: the recognized text becomes a message to your hub.

On the desktop, recognition runs on your machine with whisper.cpp or a program you name ([voice.md](voice.md)).

## Notifications

The apps published in the App Store and on Google Play show notifications only while they are connected to a hub, and register with no push service. A build you make yourself can use Apple's or Google's push services with your own keys ([mobile.md](mobile.md#notifications)); a push then carries which hub and workspace has something for you, and never a prompt, a command or a request's text.

## Requests the software makes by itself

- The desktop app and `twr update` ask GitHub for the latest twr release, and download it from there when you update.
- `twr voice setup` downloads the speech model from whisper.cpp's published models, when you run it.

These requests carry no identifier of yours beyond what any download reveals (your IP address, to GitHub or the model's host).

## Children

twr is a developer tool and is not directed at children.

## Changes and contact

Changes to this page are recorded in the repository's history. For a question about privacy, open an issue at <https://github.com/ignaval/twr/issues>, or write to `security@twr.tools` if it should stay private.
