# Notifications

The TUI can alert the terminal it runs in ([tui.md](tui.md#notifications)), but only while it's open. The hub can also run a command of yours when something needs you, whether or not a TUI is open. That command can push to your phone, post to a chat or show a desktop notification. twr makes no network calls of its own: the command does.

```toml
[notify]
command = ["notify-send", "--app-name=twr", "{title}", "{body}"]
# events = ["permission", "question", "confirm", "error", "review", "report", "reply"]  # add "stalled" for stalls
# detail = false        # true: the text of what's asked or reported goes along
# timeout_secs = 30     # the command is ended after this long
```

`command` is a program and its arguments, run without a shell. In any argument, `{title}`, `{body}` and `{workspace}` become the notification's title, body and [workspace](workspaces.md). Unset, which is the default, the hub runs nothing. It reads `[notify]` when it starts, so a change applies after `twr hub stop` and `twr`.

Put `{title}` and `{body}` each in an argument of its own, never inside a script string such as the one after `sh -c`. Their text holds thread names, which the orchestrator may choose, and with `detail = true` what an agent wrote (a command line, a question, a report). Inside a script, a quote or a `$(...)` in that text would run as shell code. As an argument of its own it reaches the program as it is. A script can take the text as arguments (`sh -c '...' sh {title} {body}`, then `$1` and `$2`) or read it from the JSON on stdin, as the macOS and Slack recipes below do.

## What it notifies, and when

The hub decides what's worth telling you by the same rules the TUI uses, and `events` takes the same names as `[tui.notify] events`:

- a new permission, question or confirm card, or a delete card the orchestrator asked for;
- a failure left for you, after any automatic retry;
- a turn to review, a worker that finished a turn, and the orchestrator's reply to your message;
- a stall, if you add `stalled`.

It waits two seconds before it notifies (five for a failure), and sends nothing for an item answered, or a failure retried, in that time. A report rewritten after its notification went out doesn't notify again, also after the hub restarted: what happened before it started is never news, but it remembers what was. What comes within five seconds of a notification goes out together after them, as one notification: `twr: 2 need you` / `api needs permission, web asks`. One command runs at a time.

While you use a TUI connected to the hub, it sends nothing: a TUI tells its hub you're looking while its terminal has focus and you pressed a key, clicked or pasted in the last 30 seconds. The TUI's own `[tui.notify]` rules don't change this. Every hub of your home runs its own `[notify]`, since its workspaces share `config.toml`, and `{workspace}` says which one it was.

## What the command gets

Its arguments, with the placeholders filled in, and one line of JSON on stdin:

```json
{"title": "twr: api", "body": "api needs permission", "workspace": "default",
 "items": [{"event": "permission", "thread": "01J7Q...", "name": "api", "turn": 3, "item": "01J7R..."}]}
```

`items` lists what the notification is about, most urgent first: its kind (`event`), the thread's id and name (`twr` for the hub's own questions, `orchestrator` for its replies), the turn, and the attention item's id when there is one.

By default the title and body name only the thread and the kind of thing: `api needs permission`. What was asked or reported, a command line, a question, a report's summary, can hold secrets, and the command often sends it to a server you don't control. `detail = true` adds it: the body reads `api needs permission: Bash cargo test`, and each item gets `text`.

The title and body never hold control characters, so a placeholder can't start a new line or a new header.

The command runs on the hub's machine, in a session and process group of its own (without a controlling terminal), with the hub's environment minus twr's tokens. After `timeout_secs` (30 by default, at most 600) its whole group is ended. A command that fails or times out is logged in the hub's log and not retried. It counts as an [agent](usage.md#security): it descends from the hub and carries a `TWR_AGENT` marker of its own, so a `twr` it runs, even from a process it left behind, can read the hub but not answer or send anything. When the command exits, times out or the hub stops, whatever is left of its process group is ended too. The command needn't read its stdin: once it exits, the hub stops writing to it, even while a process it left running holds stdin open.

## Recipes

### ntfy: your phone

[ntfy](https://ntfy.sh) pushes to its phone apps over plain HTTP. Pick a topic name nobody can guess, since anyone who knows it can read your notifications, and subscribe to it in the app:

```toml
[notify]
command = ["curl", "-fsS", "-H", "Title: {title}", "-H", "Tags: robot", "-d", "{body}", "https://ntfy.sh/twr-k3ufz9wq2v"]
```

With your own ntfy server, use its URL instead, and add `-H "Authorization: Bearer tk_..."` if it needs a token.

### Desktop: Linux

```toml
[notify]
command = ["notify-send", "--app-name=twr", "{title}", "{body}"]
```

`notify-send` needs your desktop session's D-Bus. A hub started from a terminal in that session has it. A hub started over SSH, or by a service manager without your session's environment, doesn't, and the command fails.

### Desktop: macOS

```toml
[notify]
command = ["osascript", "-e", "on run argv\ndisplay notification (item 2 of argv) with title (item 1 of argv)\nend run", "{title}", "{body}"]
```

The title and body go in as arguments, never into the script's text. [terminal-notifier](https://github.com/julienXX/terminal-notifier) works too: `["terminal-notifier", "-title", "{title}", "-message", "{body}", "-group", "twr"]`.

### Slack

A Slack [incoming webhook](https://api.slack.com/messaging/webhooks) takes JSON, so build it from stdin with `jq` rather than putting the text into JSON yourself:

```toml
[notify]
command = ["sh", "-c", "jq -c '{text: (.title + \": \" + .body)}' | curl -fsS -H 'Content-Type: application/json' --data-binary @- \"$0\"", "https://hooks.slack.com/services/T000/B000/XXXX"]
```

The webhook URL is a secret: keep `config.toml` private to you (`chmod 600`). The hub warns at start, in its log and in `twr info`, when a config with a `[notify]` command can be accessed by other users. `config.show`, which the orchestrator can call, says only whether `command` is set, never what it is.

### A script of your own

Anything that reads the JSON can decide for itself: notify only for some threads, only at night, or only for permissions:

```sh
#!/bin/sh
# ~/bin/twr-notify: only permissions and questions, to ntfy.
jq -e '[.items[].event] | any(. == "permission" or . == "question")' >/dev/null || exit 0
curl -fsS -H "Title: $1" -d "$2" https://ntfy.sh/twr-k3ufz9wq2v
```

```toml
[notify]
command = ["/home/me/bin/twr-notify", "{title}", "{body}"]
```
