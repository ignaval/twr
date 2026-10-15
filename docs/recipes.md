# Recipes

Setups built from twr's pieces and other tools. None of them is a twr feature: each is a few lines of configuration or a script you adapt.

## A shared jobserver for make and cargo

Several threads building a Rust or C project at once each start as many compile jobs as the machine has CPUs, so four threads on a 4-core machine run sixteen. GNU make's jobserver fixes that across processes: a FIFO holds tokens, and every make, cargo, rustc and `cc`-crate build that finds it in its environment takes a token for each job past its first. Point every worker at one FIFO and they share one pool of compile jobs.

A script that makes the FIFO, puts `N` tokens in it, and keeps it open (a FIFO loses what it holds once nothing has it open):

```sh
#!/bin/sh
# twr-jobserver N: a make jobserver FIFO with N tokens, at $XDG_RUNTIME_DIR/twr-jobserver.
set -eu
n=${1:-4}
fifo=${XDG_RUNTIME_DIR:?}/twr-jobserver
rm -f "$fifo"
mkfifo -m 600 "$fifo"
exec 3<>"$fifo"
printf "%${n}s" '' | tr ' ' + >&3
exec sleep infinity
```

Run it in the background, for example as a transient user service: `systemd-run --user --unit=twr-jobserver ~/bin/twr-jobserver 3`. Then hand the FIFO to every worker in the [config](configuration.md), with the absolute path (twr doesn't expand variables in `env`):

```toml
[threads]
env = { MAKEFLAGS = "-j --jobserver-auth=fifo:/run/user/1000/twr-jobserver", CARGO_MAKEFLAGS = "-j --jobserver-auth=fifo:/run/user/1000/twr-jobserver" }
```

Restart the hub, so runtimes started from then on get them. This was checked with cargo 1.98 and the `cc` crate 1.5: a build waits for a token before each job past its first and puts it back after. A few things to know:

- Every build keeps one job of its own without a token. With 3 tokens and two builds at once, up to five jobs run: give the FIFO a token or two fewer than the CPUs you want builds to use.
- It limits compile jobs, not what they start: `cargo test` runs each test binary with a thread per CPU (`RUST_TEST_THREADS` in `env` caps that), and a test suite that starts its own processes isn't counted.
- A build that is killed (SIGKILL) doesn't put back the tokens it holds, and the pool shrinks. Restart the script to refill it (`systemctl --user restart twr-jobserver` for the unit above) once no build is running: one that still has the old FIFO open goes on using it, a pool of its own.
- GNU make reads the `fifo:` form from version 4.4 on; an older make can't use it.
- Every worker can read and write the FIFO. An agent that takes tokens and never puts them back drains the pool for every build, and one that writes more makes it bigger. It's a shared limit that agents keep by convention, not a guard.
- A thread on a [runner box](runners.md) gets the same `env`. Put the variables in a `[projects.<name>] env` of the projects that build on this machine, or run the script on the box too.

[Jobs](usage.md#jobs) work alongside it: job slots limit how many whole commands (a test suite, a release build) run at once, the jobserver how many compile jobs they and the agents' own builds run between them. [`[threads] cpu_weight`](usage.md#resource-limits) is the other way round: it doesn't limit how much runs, it has the workers give way to everything else when the CPUs are busy.

## A review loop

A second agent that reviews each change before you look at it catches a lot, and twr has the pieces to run one without watching it: [`twr send --after`](usage.md#send-a-prompt-after-another-thread-finishes) to hand work from one thread to the next when it finishes, [jobs](usage.md#jobs) for the tests, and [`twr wait`](usage.md#wait-for-threads) to know when a step is over. Which model reviews, how many rounds and what the reviewer looks for is yours to choose.

A worker that writes the code, and a reviewer on another runtime that works in the repository checkout and only reads:

```sh
twr thread new api --name api
twr thread new api --name api-review --runtime codex --mode read-only --no-worktree
```

The reviewer reads the author's branch, so the author commits its work. Queue the review first, then give the author its task, with the tests run as a job so a long suite doesn't hold its turn:

```sh
twr send api-review --after api "Review git log -p dev..twr/api for bugs, missing tests and docs that disagree with the code. List each finding with its file and line. The author's summary: {report:api}"
twr send api "Add rate limiting to the login endpoint. Run the tests with twr job run -- cargo test, fix what fails, and commit."
```

A prompt held for a thread that is idle waits for its next turn, here the task's, and goes once that turn ends, as if you had sent it then. In the other order, a task that ended before the review was queued would leave it waiting for a turn that never comes. If that turn failed or its report says it gave up, a card asks you whether to send it anyway. The review's findings are in its reply: a thread's report is a short summary of it (at most 400 characters), so `{report:api-review}` would cut them. Read them in the TUI or with `twr events`, and send the fixes to api yourself.

To run rounds until the reviewer has nothing left, a script (it needs `jq`) takes the task, then reads each review's whole reply and sends it to the author, each time queuing the next review first.

```sh
#!/bin/sh
# review-loop "<task>": api does the task, then reviews of twr/api and fixes, up to 5 rounds.
set -eu
task=$1
review='Review git log -p dev..twr/api for bugs, missing tests and docs that disagree with the code. List each finding with its file and line. If there are none, reply with exactly: no findings'
twr send api-review --after api "$review"
twr send api "$task"
round=1
while :; do
  out=$(twr wait api-review --timeout 2h --json)
  grade=$(printf '%s' "$out" | jq -r '.grade // empty')
  turn=$(printf '%s' "$out" | jq -r '.turn')
  # The review's reply: the last final text of the turn twr wait saw end.
  events=$(twr events api-review --hydrate)
  reply=$(printf '%s\n' "$events" | jq -rs --argjson turn "$turn" \
    'map(select(.kind == "agent.text" and .data.final and .turn == $turn)) | last | .data.text // ""')
  [ -n "$reply" ] || { echo "no reply in turn $turn of api-review"; exit 1; }
  verdict=$(printf '%s' "$reply" | tr -d '[:space:].' | tr '[:upper:]' '[:lower:]')
  if [ "$grade" = success ] && [ "$verdict" = nofindings ]; then
    echo "clean after $round round(s)"; exit 0
  fi
  [ "$round" -lt 5 ] || { echo "findings left after 5 rounds"; exit 1; }
  twr send api-review --after api "$review"
  twr send api "Fix these review findings and commit, or say why one is wrong:

$reply"
  round=$((round + 1))
done
```

The prompt held for `api-review` keeps it from counting as finished, so `twr wait` returns once the review is written. It exits 2 when the thread waits on you and 3 when the review failed, and `set -e` then stops the loop for you to look. Only a reply that is `no findings` and nothing else (in any case, with or without a full stop), graded `success`, ends it clean. The next review is queued before the fixes are sent, so it waits for the turn that makes them; queuing the fixes after the review instead (`twr send api --after api-review`) would be refused while the review waits for api, since prompts can't wait for each other in a circle.

Nobody reads the reviewer's reply before it reaches the author: the script sends it as your prompt, with your authority, round after round. The reviewer's own input is the author's work, its commits and, in the first example, its report (`{report:api}`). So text one agent wrote, which may quote a web page, an issue or a file, steers the other as if you had written it. Run the loop on work where you would accept that, and read the rounds afterwards ([security-model.md](security-model.md#text-from-agents)).

The [orchestrator](orchestrator.md) can run the same loop: ask it to review each finished change with `api-review` and to send the findings back to `api`; its `send` tool takes `after` too. [Merge readiness](worktrees.md#merge-readiness) on the author's card then says whether the branch still merges cleanly into its base.
