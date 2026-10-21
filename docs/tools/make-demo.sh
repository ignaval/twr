#!/bin/sh
# Records docs/img/tui-demo.svg: an animated capture of the real TUI in use.
#
#   docs/tools/make-demo.sh
#
# It builds twr and starts a throwaway hub (TWR_HOME under /tmp) whose orchestrator and
# workers are small scripted ACP agents (docs/tools/demo/orch.py and worker.py). The
# orchestrator calls twr's real MCP tools: it starts the threads and relays the user's answer.
# The workers write and commit files in their worktrees and ask for permissions that become
# cards. Then the script types into `twr` in a private tmux server as a user would, records the
# screen about ten times a second, and turns the recording into one animated SVG with
# docs/tools/ansi2svg.py. No real agent runs: the hub and the TUI get a PATH without claude,
# codex or grok, and HOME is the temporary directory. Everything is removed on exit.
#
# Needs git, tmux and python3. Takes about a minute.
# TWR_DEMO_KEEP=<dir> also copies the raw frames and the hub's log there. TWR_BIN=<path> uses
# that twr binary instead of building one.

set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
if [ -n "${TWR_BIN:-}" ]; then
    twr=$TWR_BIN
else
    cargo build --quiet -p twr-cli
    twr="$root/target/debug/twr"
fi

cols=128
rows=32

work=$(mktemp -d /tmp/twr-demo.XXXXXX)
tmux_name="twrdemo-$$"
hub_pid=""
rec_pid=""

cleanup() {
    [ -z "$rec_pid" ] || kill "$rec_pid" 2> /dev/null || true
    tmux -L "$tmux_name" kill-server 2> /dev/null || true
    rm -f "${TMUX_TMPDIR:-/tmp}/tmux-$(id -u)/$tmux_name"
    if [ -n "$hub_pid" ]; then
        kill "$hub_pid" 2> /dev/null || true
        # The hub stops its runtimes before it exits.
        i=0
        while kill -0 "$hub_pid" 2> /dev/null && [ "$i" -lt 50 ]; do
            sleep 0.2
            i=$((i + 1))
        done
        if kill -0 "$hub_pid" 2> /dev/null; then
            kill -9 "$hub_pid" 2> /dev/null || true
        fi
        wait "$hub_pid" 2> /dev/null || true
    fi
    if [ -n "${TWR_DEMO_KEEP:-}" ]; then
        cp -r "$work/frames" "$work/hub.log" "$TWR_DEMO_KEEP/" 2> /dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

export GIT_AUTHOR_NAME="Dana Ruiz" GIT_AUTHOR_EMAIL="dana@example.com"
export GIT_COMMITTER_NAME="Dana Ruiz" GIT_COMMITTER_EMAIL="dana@example.com"
export GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null

# mkrepo NAME FILE...: a repository with one commit holding FILEs.
mkrepo() {
    dir="$work/code/$1"
    shift
    mkdir -p "$dir"
    git -C "$dir" init -q -b main
    for f in "$@"; do
        mkdir -p "$dir/$(dirname "$f")"
        printf '%s\n' "// $f" > "$dir/$f"
    done
    git -C "$dir" add -A
    git -C "$dir" commit -q -m "Initial commit"
}

mkrepo api Cargo.toml src/main.rs src/routes/login.rs src/middleware/mod.rs
mkrepo billing Cargo.toml src/invoice/totals.rs src/ledger/post.rs
mkrepo web package.json src/pages/checkout.tsx
mkrepo docs README.md CHANGELOG.md

mkdir -p "$work/gates" "$work/frames" "$work/home"
export TWR_HOME="$work/twr"
mkdir -p "$TWR_HOME"
cat > "$TWR_HOME/config.toml" << EOF
default_runtime = "scripted"
stall_after_secs = 0

[orchestrator]
runtime = "scripted-orchestrator"

[runtimes.scripted]
acp = "python3 $root/docs/tools/demo/worker.py $work/gates"

[runtimes.scripted-orchestrator]
acp = "python3 $root/docs/tools/demo/orch.py $work/script.json"

[projects.api]
repo = "$work/code/api"
[projects.billing]
repo = "$work/code/billing"
[projects.web]
repo = "$work/code/web"
[projects.docs]
repo = "$work/code/docs"
EOF

# What the orchestrator does with each message the user types.
ask="Two things. On api, rate limit logins per IP. On billing, round tax once on the invoice total."
cat > "$work/script.json" << EOF
{
  "Morning! I'm cutting the 0.5.0 release today. Tell me when a thread needs me.": [
    ["text", "Morning. I'll watch the threads and tell you when one needs you."]
  ],
  "$ask": [
    ["call", "spawn_thread", {"project": "api", "name": "ratelimit",
      "prompt": "Rate limit POST /login per client IP: 5 attempts a minute, then 429."}],
    ["call", "spawn_thread", {"project": "billing", "name": "invoice-rounding",
      "prompt": "Round tax once on the invoice total instead of per line."}],
    ["text", "Started ratelimit on api and invoice-rounding on billing. I'll tell you when they need you."]
  ],
  "allow ratelimit": [
    ["call", "respond_attention", {"attn_id": "{attn:ratelimit}", "answer": "allow"}],
    ["text", "Allowed: ratelimit is running its tests."]
  ]
}
EOF

env -i HOME="$work/home" PATH=/usr/bin:/bin TWR_HOME="$TWR_HOME" \
    TWR_CLAUDE_BIN=/bin/false TWR_CODEX_BIN=/bin/false TWR_GROK_BIN=/bin/false \
    "$twr" hub > "$work/hub.log" 2>&1 &
hub_pid=$!
i=0
until "$twr" info > /dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || {
        cat "$work/hub.log" >&2
        exit 1
    }
    sleep 0.2
done

# status THREAD: the thread's status column in `twr thread list`.
status() {
    "$twr" thread list | awk -v n="$1" '$1 == n { print $2 }'
}

# wait_for THREAD STATUS
wait_for() {
    i=0
    until [ "$(status "$1")" = "$2" ]; do
        i=$((i + 1))
        if [ "$i" -gt 150 ]; then
            echo "make-demo: $1 never got to $2 (it is $(status "$1"))" >&2
            tail -20 "$work/hub.log" >&2
            exit 1
        fi
        sleep 0.2
    done
}

# settle: until the orchestrator has no turn running and none queued.
settle() {
    sleep 0.6
    wait_for orchestrator idle
    sleep 0.4
    wait_for orchestrator idle
}

keys() {
    tmux -L "$tmux_name" send-keys "$@"
}

# type_text TEXT: type TEXT a couple of characters at a time, as a person does.
type_text() {
    s=$1
    while [ -n "$s" ]; do
        c=$(printf '%s' "$s" | cut -c1-2)
        s=${s#"$c"}
        keys -l -- "$c"
        sleep 0.06
    done
}

# select_thread THREAD: press Shift-Up until THREAD's card is the selected one (the heavy border,
# its title the project and the name).
select_thread() {
    i=0
    until tmux -L "$tmux_name" capture-pane -p | grep -q -E "┏ ([^ ]+ · )?$1 "; do
        i=$((i + 1))
        if [ "$i" -gt 8 ]; then
            echo "make-demo: could not select $1" >&2
            exit 1
        fi
        keys S-Up
        sleep 0.4
    done
}

# tui: a TUI in the tmux server, once it has drawn its first frame (the input box's title):
# it drops what is typed before that, and right after it.
tui() {
    tmux -L "$tmux_name" -f /dev/null new-session -d -x "$cols" -y "$rows" \
        "env -i HOME='$work/home' PATH=/usr/bin:/bin TERM=xterm-256color TWR_TUI_HIDE_LOAD=1 TWR_HOME='$TWR_HOME' '$twr'"
    i=0
    until tmux -L "$tmux_name" capture-pane -p | grep -q "Enter sends"; do
        i=$((i + 1))
        if [ "$i" -gt 150 ]; then
            echo "make-demo: the TUI never drew" >&2
            exit 1
        fi
        sleep 0.2
    done
    sleep 0.6
}

# Before the recording: two finished threads and a greeting, so the screen isn't empty.
"$twr" thread new docs --name changelog > /dev/null
"$twr" send changelog "Draft the changelog for 0.5.0." > /dev/null
"$twr" thread new web --name a11y-audit > /dev/null
"$twr" send a11y-audit "Run an accessibility audit of checkout and fix what you can." > /dev/null
wait_for changelog "done"
wait_for a11y-audit "done"

# What is typed goes to the orchestrator.
tui
keys -l "Morning! I'm cutting the 0.5.0 release today. Tell me when a thread needs me."
keys Enter
settle
tmux -L "$tmux_name" kill-server
sleep 0.5

# The recording: a fresh TUI, captured about ten times a second.
tui
(
    n=0
    while :; do
        f=$(printf '%s/frames/%05d' "$work" "$n")
        tmux -L "$tmux_name" capture-pane -e -p > "$f.ans" 2> /dev/null || exit 0
        date +%s.%N > "$f.t"
        n=$((n + 1))
        sleep 0.08
    done
) &
rec_pid=$!
sleep 1.5

# Ask for two threads (what is typed goes to the orchestrator).
type_text "$ask"
sleep 0.6
keys Enter
wait_for ratelimit running
settle
sleep 2.5

# invoice-rounding asks to push; answer it in its card's action popup.
touch "$work/gates/rounding-perm"
wait_for invoice-rounding waiting
settle
sleep 2.5
select_thread invoice-rounding
sleep 1.2
keys Enter
sleep 1.2
date +%s.%N > "$work/poster.t"
sleep 1.3
keys 1
wait_for invoice-rounding "done"
settle
sleep 2.5

# ratelimit asks to run its tests; answer it in the conversation (Esc lets go of the card
# selected above first).
touch "$work/gates/ratelimit-perm"
wait_for ratelimit waiting
settle
sleep 2.5
keys Escape
sleep 0.4
type_text "allow ratelimit"
sleep 0.6
keys Enter
wait_for ratelimit "done"
settle
sleep 3

# Dive into ratelimit for its transcript (Enter, Enter: its popup, then talk to it), and come
# back.
select_thread ratelimit
sleep 1
keys Enter
sleep 1
keys Enter
sleep 4
keys Escape
sleep 3.5

kill "$rec_pid" 2> /dev/null || true
wait "$rec_pid" 2> /dev/null || true
rec_pid=""

mkdir -p docs/img
python3 docs/tools/ansi2svg.py --frames "$work/frames" docs/img/tui-demo.svg --max-hold 3.5 \
    --poster-time "$(cat "$work/poster.t")" \
    --title "twr in use: the user asks the orchestrator for two threads, answers one permission in its card's popup and one in the conversation, and opens a thread's transcript"
ls -l docs/img/tui-demo.svg
