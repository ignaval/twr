# Reading aloud

twr can read the orchestrator's messages aloud (and, in the desktop and mobile apps, listen to
what you say back: [voice.md](voice.md)):

- **as they come**, with `/read on` (live mode);
- **on demand**, with `/read N` for the last N messages.

The voice is the one of the machine you listen on: macOS's own voices (its Premium and
Enhanced ones too), twr's own [local neural voices](#twrs-own-voices) (Kokoro, Supertonic,
Piper, on macOS and Linux), speech-dispatcher on Linux, or a program of your own; one per
language if you like ([below](#a-voice-per-language)). When the TUI runs on a server over SSH or mosh, your laptop
does the speaking: the text reaches it over an SSH connection of its own, and nothing plays on
the server.

## What is read

A message is one finished text of the current orchestrator: a reply, or a text it wrote before
a tool call ("Let me check the api thread"). Each one is read when it is complete, not while it
streams.

Never read:

- thinking;
- tool calls and results;
- your own prompts;
- worker threads' texts (except with `/read N` in a dive);
- the hub's updates and cards (with `"attention"` in `[speech] read`, a short sentence says
  that a card needs you: [below](#hearing-when-something-needs-you));
- [summaries](tui.md#routine-updates-and-summaries);
- the turns twr runs for itself: the handoff to a new orchestrator and the old one's summary
  before it, a context compaction (yours, one you accepted from an offer, or the hub's idle
  one).

A thread moved to another runtime or account gets a handoff in front of the next message it
runs; that message's reply is read like any other.

In live mode, `[speech] read = ["replies"]` leaves the commentary out, and `turn_from =
"mine"` reads only the turns you started. `/read N` reads any of the conversation's messages:
you asked for them.

### Hearing when something needs you

For a hands-free session, add `"attention"` to `[speech] read`:

```toml
[speech]
read = ["replies", "commentary", "attention"]
```

The speaker then also says, in live mode, when an attention item is raised in the workspace:

| Item | Said |
|---|---|
| a permission | `api needs your permission` |
| a question | `docs has a question` |
| a failure | `api failed` |
| a review | `api needs your review` |
| a hung turn | `api seems hung` |
| a stall | `api is stalled` |
| the hub's own ("which thread did you mean?", the orchestrator's proposals) | `twr has a question for you` |

Each is a fixed sentence and the thread's name, nothing more: never what the card says, the
command or anything an agent wrote. Answer on screen, or by voice.

- **New items only.** A permission sent again with new input while it is open is not said
  again. One answered within two seconds (five for a failure, which twr may retry by itself)
  is never said.
- **One per thread.** Items of a thread raised close together are said once, as the most
  urgent of them. One still waiting on the speaker is replaced by the thread's next.
- **Never stale.** Once every item an announcement was about is answered or withdrawn (or its
  thread stopped), it is dropped if it still waits behind a message. One being said ends.
- **Live only.** An item raised while the speaker was away (or with live mode off) is never
  said later, nor counted with the missed messages; it is on screen. Stop, pause and skip act
  on announcements as on messages.

A hub older than announcements refuses `"attention"`: the speaker says so once and reads
without them until twr is updated there.

The words are read as written. Only markup changes:

- emphasis, heading marks and quotes go;
- a link reads as its text, and a bare URL as "link to github.com";
- inline code reads as itself when it is short, else as "code";
- a code block is announced ("code block, 12 lines, rust");
- a table is read row by row;
- a path of three parts or more reads as its last two and its line: "src/app.rs, line 120";
- control characters, invisible characters, emoji and box drawing are dropped.

No model rewrites anything.

## Live mode and on-demand reading

| Command | Does |
|---|---|
| `/read on`, `/read off` | read every new message of this workspace's orchestrator, on every speaker connected to its hub; or stop doing so. Nothing said before `/read on` is read |
| `/read`, `/read N` | read the last message, or the last N (up to 50), oldest first. It stops what was being read and empties the queue |
| `/read N --orch` | in a dive, the orchestrator's messages rather than the thread's |
| `/read N --on <speaker>` | only on that speaker (its name or device) |
| `/read this` | the long message selected in the conversation (`Shift-←`, then `Shift-↑`/`Shift-↓`; a short one is never selected: use `/read N`) |
| `/read stop` | stop now and empty the queue; live mode stays as it is |
| `/read pause`, `/read resume` | pause; resume. `say`, `spd` and command engines restart the sentence they were in |
| `/read skip` | end the message being read; go to the next |
| `/read status` | the speakers connected, live mode on each, what is read |
| `/read voice <name>`, `/read rate <wpm>` | set the speakers' voice or speed (80 to 500 words per minute), kept on each speaker's machine |
| `Ctrl-R` | stop, while something is read |

While a speaker reads, the status bar says `🔊 reading 2/5 · Ctrl-R stop` (`[read]` with
`[tui] colors = "none"`), and the message being read has `▶` in front of it.

Live mode belongs to each speaker and each workspace. It is kept on the speaker's machine, so
a laptop can read aloud while a phone doesn't, and it stays on across restarts. When several
workspaces read aloud on one speaker, each message starts with its workspace's name.

In the **local desktop app**, the speaker button in the conversation's top bar toggles
automatic reading for this computer and workspace. Its highlighted speaker means on; the
crossed-out speaker means off. The tooltip names the state and scope. It follows the saved
live setting and changes made through `/read` or voice mode, including when the local TUI's
speaker is doing the speaking. Opening a local desktop conversation attaches the speaker
with its saved live setting. Turning it off stops the current automatic reply and drops
queued automatic replies for this workspace; explicit `/read` requests and other workspaces
keep their existing behavior. Turning it on reads new replies under the usual speech rules.

The button is also available in deep dives: it still controls the **orchestrator's** replies,
not automatic reading of worker messages. Use `/read N` to hear a worker on demand. Playback
pause remains independent of the live setting. A disconnected or unavailable speaker shows
an unknown state instead of claiming live mode is off; reconnecting reads the actual setting.
The button needs no microphone or speech recognition model. It is not offered for remote hub
views or phones, whose speaker bridge does not expose the same local device controls.

A speaker that was away (the laptop slept, the connection dropped, the hub restarted) reads
what was said meanwhile, if it was said in the last two minutes (`[speech] catchup_secs`).
Older messages are counted: "4 earlier messages came while I was disconnected; they're on
screen". No message is read twice by live mode, whatever reconnects. A message whose reading had
started is not read again.

## Speakers

A speaker is what speaks. There are three kinds:

- **The TUI itself.** When the TUI runs on the machine you listen on (not under SSH or mosh),
  it speaks by itself. Nothing else is needed. `[speech] local = "off"` turns that off, and
  `"on"` makes even a remote TUI speak (out of the remote machine's speakers). It is the
  speaker of every workspace, not only the one the TUI shows. A TUI opened with `--socket`, or
  a thread's agent's TUI, never speaks by itself.
- **`twr speak`** on your laptop, for a TUI on another machine:

  ```
  twr speak --ssh myserver
  ```

  It connects to `myserver` with your ssh config and keys, as `ssh myserver twr speech feed`
  does, and reads aloud what that machine's hubs send: every workspace there where live
  reading is on, or only `-w`'s (`twr -w work speak --ssh myserver`). Without `--ssh`, this
  machine's own hubs, the same way. Keys in its terminal: `s` stop, `p`
  pause or resume, `n` skip, `q` quit. It needs twr installed on both machines.
- **`twr remote`**, which does both in one command:

  ```
  twr remote myserver          # mosh myserver -- twr, with twr speak --ssh myserver beside it
  twr remote myserver --ssh    # ssh -t myserver twr instead of mosh
  twr remote myserver -- -w work   # the remote twr opens workspace work
  ```

  Arguments after `--` go to the remote `twr`. The speaker reads aloud every workspace of the
  server where live mode is on (`twr -w work remote myserver`: that one only).

  The speaker runs in the background while the session lasts, logging to `speak.log` in twr's
  data directory, and stops when you quit the session. It has no terminal, so its ssh can't ask
  you anything: it needs your key in an ssh agent (or a ControlMaster connection) and a host
  already in `known_hosts`. Without them, `speak.log` says why.

Only one speaker of a machine speaks for the same hubs. If the TUI's own speaker and a `twr
speak` run on one machine for its own hubs (or two `twr speak --ssh` for one server), the
second one waits and takes over if the first one goes. A speaker for another machine's hubs is
not held up: the TUI's own speaker on your laptop and `twr speak --ssh myserver` both run.

`twr speak --control stdio` is for a program that drives a speaker: controls on stdin and
what it plays on stdout, one line each. The desktop app's voice mode runs one
([voice.md](voice.md#where-it-plays)).

To keep a speaker running whenever you're logged in, `twr speak --ssh myserver --print-service`
prints a LaunchAgent (macOS) or a systemd user unit (Linux) to install yourself. twr installs
none. A service has no terminal either, and may not see your login's ssh agent
(`SSH_AUTH_SOCK`): give its ssh a key it can use without asking, in your ssh config.

### Over SSH and mosh

The text goes from the server to your laptop over an SSH command of its own, never through
your terminal session:

- **No forwarding.** It uses no port forwarding, no socket forwarding and no new listener. It
  works when the server's sshd disables forwarding, and it adds `ForwardAgent=no`,
  `ForwardX11=no` and `ClearAllForwardings=yes`.
- **mosh plays no part.** mosh carries only screen state, has no port forwarding, and drops
  every escape sequence but a few. So the speaker's SSH connection is separate from your mosh
  session.
- **Terminals can't carry speech.** A terminal plays a bell or a tone at most, so text in the
  session could never be heard on your laptop.

What it needs:

- **ssh that doesn't prompt on reconnect:** a key in your ssh agent (the macOS Keychain), or
  a ControlMaster connection. `twr speak`'s first connection runs in your terminal and may ask
  about the host key or a passphrase; its keys start once it is connected. Reconnects run with `BatchMode=yes` and retry with backoff
  (up to a minute). A server that asks for a code on every connection needs you at each
  reconnect.
- **What crosses the network:** the messages' text, a few KB each, inside SSH, plus small
  control and status lines. No audio crosses the network.
- **Where it plays:** the server plays nothing. A server without a sound card works the same.

## Engines and settings

`[speech]` in the config of the machine you listen on (the hub reads none of it):

```toml
[speech]
engine = "auto"          # auto | say | avspeech | spd | command
voice = ""               # the engine's voice; empty: the system's default. "kokoro/af_heart":
                         # one of twr's own (`twr voices list`)
voices = {}              # a voice per language: { en = "Ava (Premium)", es = "kokoro/ef_dora" }
espeak = ""              # the espeak-ng program for Kokoro and Piper; empty: espeak-ng on PATH
rate = 0                 # words per minute (80-500); 0: the engine's default
volume = 1.0             # 0.0-1.0 (avspeech, spd)
read = ["replies", "commentary"]   # and "attention": hear when something needs you
turn_from = "all"        # all | mine
code = "announce"        # announce | read | skip
paths = "short"          # short | full
catchup_secs = 120
local = "auto"           # the TUI speaks by itself: auto (on your own machine) | on | off
command = []             # engine = "command": a program and its arguments, no shell
```

| Engine | Where | Notes |
|---|---|---|
| `auto` | macOS, Linux (the default) | `say` on macOS, `spd` on Linux when `spd-say` is installed, and [twr's own voices](#twrs-own-voices) for a voice named `<model>/<voice>`: each sentence goes to the one its voice belongs to |
| `say` | macOS (the default there) | macOS's `say`, the text on its stdin, `-v` for the voice and `-r` for the rate. Pause restarts the sentence. Checked from `say`'s documentation, not yet on a real Mac by twr's tests |
| `avspeech` | macOS | AVSpeechSynthesizer in-process: pauses mid-sentence, volume. Not yet run on a real Mac; opt in with `engine = "avspeech"` |
| `spd` | Linux (the default when `spd-say` is installed) | speech-dispatcher's `spd-say`, the text on its stdin. The voice depends on its output module, often espeak-ng. Install speech-dispatcher from your distribution |
| `command` | anywhere | your program, run without a shell, each sentence (up to 300 characters) on its stdin; stopping ends its process group. For Piper, espeak-ng, or a hosted voice through your own script |

### macOS's voices

`twr voices list` shows the voices `say` has, the best first: **Premium** and **Enhanced**
ones sound far better than the default ones, and are downloaded in System Settings →
Accessibility → Spoken Content → System voice → Manage Voices (a few hundred MB each).
Then name one: `voice = "Ava (Premium)"`, or `/read voice Ava (Premium)`. Siri's voices
can't be named; `say` uses one only as the system's own voice, with `voice` empty.

### twr's own voices

Neural voices that run on the machine's CPU, downloaded once with `twr voices install`:

| Model | Voices | Download | Notes |
|---|---|---|---|
| `kokoro` | English (US, UK), Spanish: `kokoro/af_heart`, `af_bella`, `am_michael`, `bf_emma`, `ef_dora`, `em_alex`, … | 350 MB | the most natural; needs a recent CPU (it makes a second of speech in a quarter of a second on an Apple M-series chip with four threads) and about 800 MB of memory while it speaks |
| `supertonic` | `supertonic/F1`-`F5`, `M1`-`M5`, each in English and Spanish | 129 MB | twice as fast as Kokoro; its model's licence (OpenRAIL-M) forbids some uses, such as impersonating someone |
| `piper-en`, `piper-es` | `piper-en/lessac`, `piper-es/claude` (Mexican Spanish) | 67 MB each | runs on any machine, sounds more synthetic |

```sh
twr voices list                        # this machine's and twr's voices, installed or not
twr voices install kokoro              # download, check, and time it on this machine
twr voices try kokoro/ef_dora          # hear it ("Hola. Así sueno…"); or say your own text
twr voices try kokoro/af_heart "Hi" --out hi.wav
```

- **What is downloaded.** Each model is one archive of sherpa-onnx's `tts-models` release on
  GitHub, pinned by its size and SHA-256; twr checks both as it downloads, and keeps nothing
  that differs. It unpacks it with the system's `tar` into twr's data directory
  (`speech/<model>`), keeps only the model's files and licence, and replaces an older copy in
  one step.
- **Speed.** Installing times the model on this machine: `twr voices list` says whether it
  keeps up with playback (it makes speech in under 0.6 of the time it plays) or is slow here.
  A slow model still works, but may pause between sentences.
- **espeak-ng.** Kokoro and Piper read phonemes, which the `espeak-ng` program spells out for
  them (GPL; twr runs it, never links it): `brew install espeak-ng`, `pacman -S espeak-ng`, or
  `apt install espeak-ng`. Supertonic needs nothing.
- **Playing.** twr plays its voices itself, on the default output device (CoreAudio, or ALSA
  on Linux, which PipeWire and PulseAudio serve): a message's sentences follow each other with
  no gap, and pause stops mid-word.
- **Builds.** twr's own voices are the `neural` feature of `twr-cli`, on by default: building
  twr on Linux needs ALSA's headers (`alsa-lib` on Arch, `libasound2-dev` on Debian and
  Ubuntu). The static Linux binaries of the release are built without it
  (`--no-default-features`); install twr from source there for these voices.

### A voice per language

`voices` gives each language twr tells apart (English, `en`, and Spanish, `es`) its own voice;
a message is read with the voice of its language, told from its words, and any other (too
short, or mixed) with `voice`. `twr speak --voice-for es=kokoro/ef_dora` does the same for one
speaker. A macOS voice and one of twr's can share the work:

```toml
[speech]
voice = "Ava (Premium)"
voices = { en = "Ava (Premium)", es = "kokoro/ef_dora" }
```

A speaker that is running reads with a voice chosen meanwhile from its next sentence on: each
one looks at `[speech] voice` and `voices` again once a second, when the config file has
changed. `twr speak --voice` and `--voice-for` still beat them, and a voice set with `/read
voice` beats `voice` (not `voices`), as it does at start.

### Choosing a voice in the apps

In the desktop app, **Settings › Read-aloud voice** has a picker for English and one for
Spanish: this computer's voices (on a Mac, its Premium and Enhanced ones first; **Get more Mac
voices…** opens System Settings › Accessibility › Spoken Content) and twr's natural voices.
**Preview** says a sentence with the voice (`twr voices try`). Choosing a natural voice that
isn't downloaded offers its download, with its progress, and saves the choice once it is
installed; a note says what a voice still needs (espeak-ng) or that this computer is too slow
for it. With nothing chosen, a suggestion names the best voice ready, else Kokoro to download.
The choice is `[speech] voice` (the English choice, else the Spanish one) and `voices` in this
computer's config, written in place: every other line stays. A `[speech.voices]` table of its
own, or a `voices` value over several lines, is left alone, and the app says to edit it in
Settings › Configuration.

The phone apps have the same section for the phone's speaker: [mobile.md](mobile.md#voices).
The TUI has `/read voice <name>` for every speaker at once; a voice per language there is
`[speech] voices`, which its own speaker follows as you save it.

macOS's documentation doesn't say whether its ordinary voices work offline. With the `command`
engine, the text goes wherever your program sends it: a hosted voice gets every word of the
conversation.

Windows isn't supported. A phone can't run `twr speak`; the iOS and Android apps' voice mode has
the mobile gateway run a speaker for the phone, which speaks with the phone's own voices or
with twr's natural voices made on the hub's computer ([mobile.md](mobile.md#voices)). On Android, Termux can also read the plain feed (below) with
`termux-tts-speak`.

## The feed, for your own reader

`twr speech feed --text` on the hub's machine prints each new message's sentences, one per
line, as they're said, and nothing else. With `--attention` it also prints each announcement
(`api needs your permission`) as a line when it is made. It is a public interface: pipe it
into anything that reads lines.

```
ssh myserver twr speech feed --text | while IFS= read -r line; do printf '%s\n' "$line" | my-tts; done
```

Each line is plain text, at most 300 characters, with no control characters. With
`--all-workspaces` (every workspace of that machine, rather than the current one or `-w`'s),
each line starts with `<workspace>: `. It counts
as a speaker with live mode always on; messages asked for with `/read N` are printed too, but
no control (`/read stop` and the rest) reaches it. Pass the text to your program on its stdin or as one argument, never through a
shell's `eval` or `sh -c "...$line..."`: the text is the orchestrator's, and the orchestrator
writes what its agents read.

`twr speech feed` without `--text` speaks the speaker protocol `twr speak --ssh` uses
([SKILL.md](../SKILL.md), `speech.*`).

## Who can make it speak

Only you:

- **Your clients only.** The hub's `speech.*` methods answer the TUI, the CLI and speakers on
  your socket, and refuse the orchestrator and every agent. An agent can't start, stop or
  aim speech.
- **Only the conversation's text.** No method says a text it is given: a speaker reads
  messages from the log, by thread and position. Nothing an agent sends can make your laptop
  say something the conversation doesn't hold. Announcements are the hub's own fixed
  sentences; the only word in them that comes from elsewhere is the thread's name.
- **Nothing recorded.** Speech adds nothing to the event log. The speakers and what they play
  are kept in the hub's memory.

See [security-model.md](security-model.md#reading-aloud).
