# Voice mode

Talk to the orchestrator and hear its replies, in the desktop app and on iOS and Android. The
device you talk to records you, turns what
you said into text and sends it to the orchestrator, as if you had typed it. The orchestrator's
messages come back read aloud ([read-aloud.md](read-aloud.md)) on that same device.

- **Push to talk:** click the microphone beside Send, talk, and click again to send. Or hold it
  down while you talk and let go to send. `Cmd-Shift-Space` (`Ctrl-Shift-Space` on Linux) does
  the same from the keyboard.
- **Hands-free:** the waveform button beside it (`Cmd-Shift-L`) listens until you stop it.
  Each thing you say is one message, sent once you pause for two seconds (`[voice] pause_ms`),
  so a pause to think mid-sentence doesn't send half of it. The orchestrator's replies are read
  aloud as they come, and it stops listening while they are (below).

The audio stays on your device. The hub gets the text, never a recording.

## What you hear and say

**Hands-free doesn't listen while twr reads a reply aloud,** nor for 0.8 s after it stops:
what the microphone hears then is twr's own voice, and it would come back as a message. The
voice bar says "Speaking · listening again when it ends". To cut a reply short, click
**Quiet** (it stops reading, and listening starts again a moment later), or **Stop listening**.
If you are already talking when a reply starts, the reply waits until what you said is sent,
then is read in full.

With headphones, where nothing comes back, you can talk over the voice instead:
`listen_while_speaking = true` in `[voice]` keeps hands-free listening while it reads, as push
to talk always does. Then start talking and the voice stops at once, without waiting for the
hub. What you said is recognized, and then:

- **A message.** It goes to the orchestrator, marked as spoken. The message that was being read
  is skipped, and the queue goes on.
- **One of voice mode's own words, said alone.** These control the audio and are never sent:

  | Say | Does |
  |---|---|
  | "stop", "quiet", "be quiet" | stop reading and empty the queue |
  | "pause", "keep reading", "continue" | pause; go on |
  | "skip", "next" | the next message |
  | "repeat", "say that again" | read the last message again |
  | "never mind", "cancel" | drop what is waiting for confirmation |
  | "stop listening", "voice off" | end the hands-free session |

  "Stop the api worker" is a message: only the word alone is a control. They work whenever
  voice mode is listening; while hands-free waits for a reply to end, use the **Quiet** button.
- **Nothing,** or twr's own voice heard back through the speakers: dropped, and the reading
  goes on. A transcript counts as an echo when most of its words, in order, are words of a
  message read aloud a moment ago.

Spoken messages always go to the orchestrator, whichever thread the message box is aimed at.

## Spoken messages never answer by themselves

A recognizer can mishear: "allow" for "a loud", "two" for "too". So a spoken message is never
applied as a short command (an answer to a permission or a question, a mode change), whatever
it says. When one reads as one (`allow it`, `@api yes`, `set all to bypass`), the hub sends
nothing and hands it back. The app shows it: **Review in message box** puts the text in the
message box, where you send it as typed, or edit it; **Discard** drops it. Saying "never mind"
drops it too.

To answer a request by voice, ask the orchestrator in your own words ("let the api worker run
the migration"): the orchestrator relays it under the usual rules
([orchestrator.md](orchestrator.md)), or use the request's buttons.

A spoken message is sent at once, about what is on screen now. It never waits in the offline
outbox: with the hub disconnected, voice is off.

## Setting up recognition

On the desktop, twr recognizes speech with [whisper.cpp](https://github.com/ggml-org/whisper.cpp)
on your machine:

```sh
brew install whisper-cpp   # macOS; on Linux, whisper.cpp from your distribution or from source
twr voice setup            # downloads the model whisper.cpp recognizes with (148 MB, once)
twr voice status           # says `voice: whisper (on this machine)`, or what is still missing
```

`twr voice setup` puts whisper's `base.en` at `voice/ggml-base.en.bin` in twr's data directory,
the model an empty `[voice] model` means. It downloads it from whisper.cpp's own models on
Hugging Face over HTTPS, and keeps it only once it is whole. In the desktop app, turning voice on
before the model is there offers **Download model**, which does the same. `base.en` recognizes a
short message in well under a second on Apple silicon once loaded (the first one after a start
takes a few seconds). For other languages use a multilingual model (`ggml-base.bin`) and set
`language`. The names of your threads are passed to the recognizer as words to expect, so
"api" is heard as `api`.

`[voice]` in the config of the machine with the microphone (the hub reads none of it):

```toml
[voice]
engine = "auto"        # auto | whisper | command
whisper = ""           # whisper.cpp's CLI; empty: whisper-cli (or whisper-cpp) on PATH
model = ""             # empty: voice/ggml-base.en.bin in twr's data directory
language = "en"        # as whisper names it (en, es, de...), or "auto"
command = []           # engine = "command": a program and its arguments, run without a shell
timeout_secs = 60
pause_ms = 2000        # hands-free: the silence that ends a message (500-10000)
listen_while_speaking = false  # hands-free listens while replies are read (headphones)
```

`pause_ms` and `listen_while_speaking` are read by the desktop app each time voice mode turns
on. A shorter pause sends sooner and cuts more sentences short; a longer one waits longer
after you finish. Phones read no `[voice]`: they always pause for 2 s and stop listening
while replies are read.

`engine = "command"` runs your program instead, with no shell: the recording on its stdin (a
16 kHz mono 16-bit WAV, at most two minutes), the text on its stdout, the expected words in
`TWR_VOICE_HINT`. For a hosted recognizer:

```sh
#!/bin/sh
# ~/.local/bin/stt-openai: twr voice's command engine with OpenAI's transcription API.
exec curl -sS https://api.openai.com/v1/audio/transcriptions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -F model=gpt-4o-mini-transcribe -F response_format=text -F file=@-\;filename=speech.wav
```

With a command engine the audio goes wherever your program sends it, and the app says so while
it listens ("audio leaves this device").

`twr voice transcribe < speech.wav` prints the text of one recording, for testing a setup or
for scripts (`--json` for `{"text": ...}`, `--expect "api docs"` for words to expect).

## Where it plays

The desktop app starts a speaker of its own when you turn voice on: `twr speak --control stdio
--announce --name "<computer> desktop"` for this machine's hubs, with live reading turned on for
the workspace. `--announce` makes it also say when something needs you ("api needs your
permission"), as `"attention"` in `[speech] read` does
([read-aloud.md](read-aloud.md#hearing-when-something-needs-you)): a fixed sentence with the
thread's name, never what the request says. Answer it on screen, or ask the orchestrator. It is an ordinary speaker ([read-aloud.md](read-aloud.md#speakers)): `/read status`
lists it, and it stops when the app quits. If the TUI's own speaker already speaks for this
machine, the app's waits, and voice mode pauses and resumes the one that speaks through the hub:
speakers of one machine share a device id, which `twr speak --control stdio` prints first
(`{"type": "speaker", "device", "name"}`).

`twr speak --control stdio` is how any program can drive a speaker: controls on stdin, one per
line (`pause`, `resume`, `stop`, `skip`, `quit`; anything else is ignored), and what it plays
on stdout, one JSON object per line when it changes: `{"type": "state", "workspace", "playing",
"paused", "queued", "live", "error"}` (`playing` counts an announcement being said too). No text of the conversation is written there. The end of
stdin quits it.

## Microphone and privacy

- **Only while voice is on.** The desktop app asks for the microphone the first time you turn
  voice on (macOS asks once, in System Settings > Privacy & Security > Microphone). Its window
  is granted the microphone only after you turned voice on, only for audio, and only for the
  app's own page. Push to talk records only while the button is down or until you click again;
  hands-free records until you stop it.
- **No recordings kept.** Each recording goes to the recognizer and is gone: whisper.cpp reads
  a private file (mode 0600, in a private temporary directory) that is removed when it is done.
  Nothing is written to the event log but the text you sent, as for a typed message.
- **The echo filter only listens to this device.** It compares what it hears with what the app
  read aloud lately; it never sends audio or text anywhere to do so.

## On a phone

The iOS and Android apps have the same two buttons. Recognition is the platform's own (iOS's
Speech framework, Android's `SpeechRecognizer`), on the device where the platform supports it,
and the replies are said with the platform's voice. The phone can't run `twr speak`, so the
mobile gateway runs a speaker for it on the hub's machine and sends it each sentence to say;
pausing, skipping and stopping act on the phone at once. Voice works while the app is in the
foreground. See [mobile.md](mobile.md#voice) for permissions, which platforms recognize on the
device, and Android's limits on hands-free listening.

## Limits

- One device at a time. Two devices listening in one workspace each send what they hear.
- Echo cancellation is the system's, and on the desktop it can't remove twr's voice: the
  browser cancels only what it plays itself, and `twr speak` plays through the system. That is
  why hands-free stops listening while a reply is read. With `listen_while_speaking`, or when
  another speaker of this computer reads (the TUI's), so the app can't tell when it plays, a
  hands-free session may hear twr: its own voice is dropped when recognized as such, but
  headphones are the reliable way.
- The TUI has no microphone: voice mode is in the desktop and mobile apps.
- Phones listen only in the foreground; locking the screen or switching apps ends the session.
