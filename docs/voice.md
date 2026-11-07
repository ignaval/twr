# Voice mode

Talk to the orchestrator and hear its replies. The device you talk to records you, turns what
you said into text and sends it to the orchestrator, as if you had typed it. The orchestrator's
messages come back read aloud ([read-aloud.md](read-aloud.md)) on that same device.

- **Push to talk:** click the microphone beside Send, talk, and click again to send. Or hold it
  down while you talk and let go to send. `Cmd-Shift-Space` (`Ctrl-Shift-Space` on Linux) does
  the same from the keyboard.
- **Hands-free:** the waveform button beside it (`Cmd-Shift-L`) listens until you stop it.
  Each thing you say is one message, sent once you pause. The orchestrator's replies are read
  aloud as they come.

The audio stays on your device. The hub gets the text, never a recording.

## What you hear and say

While twr reads aloud, start talking: the voice stops at once, without waiting for the hub.
What you said is recognized, and then:

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

  "Stop the api worker" is a message: only the word alone is a control.
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
twr voice status           # names what is missing, with the command that downloads the model
```

The model goes to `voice/ggml-base.en.bin` in twr's data directory, and `twr voice status`
prints the `curl` command for it with the right path. Run it, then `twr voice status` again: it
says `voice: whisper (on this machine)`. The app shows the same message when you turn voice on
before recognition is set up. `base.en` is 148 MB and recognizes a
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
```

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
machine, the app's waits, and voice mode pauses and resumes the one that speaks through the hub.

`twr speak --control stdio` is how any program can drive a speaker: controls on stdin, one per
line (`pause`, `resume`, `stop`, `skip`, `quit`; anything else is ignored), and what it plays
on stdout, one JSON object per line when it changes: `{"type": "state", "workspace", "playing",
"paused", "queued", "live", "error"}`. No text of the conversation is written there. The end of
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

## Limits

- One device at a time. Two devices listening in one workspace each send what they hear.
- Echo cancellation is the system's (the browser's on the desktop). With loud speakers and no
  headphones, a hands-free session may still hear twr now and then. Its own voice is dropped
  when recognized as such, but headphones are the reliable way.
- The TUI has no microphone: voice mode is in the desktop and mobile apps.
