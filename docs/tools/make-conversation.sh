#!/bin/sh
# Records the conversation twr.tools shows: docs/img/tui-conversation.svg (the desktop TUI,
# 104x30), docs/img/tui-conversation-phone.svg (the compact layout, 46x36) and a still frame
# of each (-still.svg, for reduced motion and the page's pause button).
#
#   docs/tools/make-conversation.sh
#
# Like make-demo.sh, it starts a throwaway hub whose orchestrator and workers are small
# scripted ACP agents (docs/tools/demo/orch.py, and worker.py with --no-push). The
# orchestrator calls twr's real MCP tools: spawn_thread twice, then send for the follow-up to
# ratelimit. "allow ratelimit" is a short command the TUI hands straight to the hub, without
# an orchestrator turn. The workers write and commit files in
# their worktrees and ask for a permission that becomes a card. Two TUIs watch the same hub in
# private tmux servers, a desktop one, where the script types as a user would, and a phone
# one; each is captured about ten times a second. At the end the script opens ratelimit's
# transcript on both, to show the follow-up arriving from the orchestrator.
#
# No real agent runs: the hub and the TUIs get a PATH without claude, codex or grok. HOME is
# a temporary directory and TWR_HOME is unset, so the hub's config and data are the defaults
# under it and the branches are named twr/<name>, as for any user (TWR_HOME would add a hash
# of its path). Everything is removed on exit.
#
# Needs git, tmux and python3. Takes about a minute and a half.
# TWR_DEMO_KEEP=<dir> also copies the raw frames and the hub's log there. TWR_BIN=<path> uses
# that twr binary instead of building one; record with a build of what the page describes,
# so no unreleased UI shows.

set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
if [ -n "${TWR_BIN:-}" ]; then
    twr=$TWR_BIN
else
    cargo build --quiet -p twr-cli
    twr="$root/target/debug/twr"
fi

cols=104
rows=30
pcols=46
prows=36

work=$(mktemp -d /tmp/twr-conversation.XXXXXX)
tmux_name="twrconv-$$"
tmux_phone="twrconvp-$$"
hub_pid=""
rec_pid=""
recp_pid=""

cleanup() {
    [ -z "$rec_pid" ] || kill "$rec_pid" 2> /dev/null || true
    [ -z "$recp_pid" ] || kill "$recp_pid" 2> /dev/null || true
    tmux -L "$tmux_name" kill-server 2> /dev/null || true
    tmux -L "$tmux_phone" kill-server 2> /dev/null || true
    if [ -n "$hub_pid" ]; then
        # The hub stops its runtimes before it exits.
        kill "$hub_pid" 2> /dev/null || true
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
        cp -r "$work/frames" "$work/framesp" "$work/hub.log" "$TWR_DEMO_KEEP/" 2> /dev/null || true
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

mkrepo api Cargo.toml src/main.rs src/routes/login.rs src/routes/signup.rs src/middleware/mod.rs
mkrepo billing Cargo.toml src/invoice/totals.rs src/ledger/post.rs

mkdir -p "$work/gates" "$work/frames" "$work/framesp" "$work/home/.config/twr"
T() { env -i HOME="$work/home" PATH=/usr/bin:/bin "$twr" "$@"; }
cat > "$work/home/.config/twr/config.toml" << EOF
default_runtime = "scripted"
stall_after_secs = 0

[orchestrator]
runtime = "scripted-orchestrator"

[runtimes.scripted]
acp = "python3 $root/docs/tools/demo/worker.py $work/gates --no-push"

[runtimes.scripted-orchestrator]
acp = "python3 $root/docs/tools/demo/orch.py $work/script.json"

[projects.api]
repo = "$work/code/api"
[projects.billing]
repo = "$work/code/billing"

[tui]
suggest = false
summaries = false
EOF

# What the orchestrator does with each message the user types.
ask="On api, rate limit logins per IP. On billing, round tax once on the invoice total."
follow="Tell ratelimit to cover POST /signup the same way."
cat > "$work/script.json" << EOF
{
  "$ask": [
    ["call", "spawn_thread", {"project": "api", "name": "ratelimit",
      "prompt": "Rate limit POST /login per client IP: 5 attempts a minute, then 429."}],
    ["call", "spawn_thread", {"project": "billing", "name": "invoice-rounding",
      "prompt": "Round tax once on the invoice total instead of per line."}],
    ["text", "Started ratelimit on api and invoice-rounding on billing. I'll tell you when they need you."]
  ],
  "$follow": [
    ["call", "send", {"thread": "ratelimit",
      "prompt": "Also limit POST /signup the same way: 5 attempts a minute per client IP, then 429."}],
    ["text", "Sent to ratelimit. I'll tell you when it's done."]
  ]
}
EOF

env -i HOME="$work/home" PATH=/usr/bin:/bin \
    TWR_CLAUDE_BIN=/bin/false TWR_CODEX_BIN=/bin/false TWR_GROK_BIN=/bin/false \
    "$twr" hub > "$work/hub.log" 2>&1 &
hub_pid=$!
i=0
until T info > /dev/null 2>&1; do
    i=$((i + 1))
    [ "$i" -lt 50 ] || {
        cat "$work/hub.log" >&2
        exit 1
    }
    sleep 0.2
done

# status THREAD: the thread's status column in `twr thread list`.
status() {
    T thread list | awk -v n="$1" '$1 == n { print $2 }'
}

# wait_for THREAD STATUS
wait_for() {
    i=0
    until [ "$(status "$1")" = "$2" ]; do
        i=$((i + 1))
        if [ "$i" -gt 150 ]; then
            echo "make-conversation: $1 never got to $2 (it is $(status "$1"))" >&2
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

# select_thread SERVER KEY THREAD: press KEY until THREAD's card is the selected one (the heavy
# border, its title the project and the name).
select_thread() {
    i=0
    until tmux -L "$1" capture-pane -p | grep -q -E "┏ ([^ ]+ · )?$3 "; do
        i=$((i + 1))
        if [ "$i" -gt 8 ]; then
            echo "make-conversation: could not select $3" >&2
            exit 1
        fi
        tmux -L "$1" send-keys "$2"
        sleep 0.4
    done
}

# tui SERVER COLS ROWS: a TUI in its own tmux server, once it has drawn its first frame (it
# drops what is typed before that).
tui() {
    tmux -L "$1" -f /dev/null new-session -d -x "$2" -y "$3" \
        "env -i HOME='$work/home' PATH=/usr/bin:/bin TERM=xterm-256color TWR_TUI_HIDE_LOAD=1 '$twr'"
    i=0
    until tmux -L "$1" capture-pane -p | grep -q -e "Enter sends" -e "Chat"; do
        i=$((i + 1))
        if [ "$i" -gt 150 ]; then
            echo "make-conversation: the TUI never drew ($1)" >&2
            exit 1
        fi
        sleep 0.2
    done
    sleep 0.6
}

# record SERVER DIR: capture SERVER's pane about ten times a second into DIR.
record() {
    n=0
    while :; do
        f=$(printf '%s/%05d' "$2" "$n")
        tmux -L "$1" capture-pane -e -p > "$f.ans" 2> /dev/null || exit 0
        date +%s.%N > "$f.t"
        n=$((n + 1))
        sleep 0.08
    done
}

# A first visit, so the recorded one isn't the first run (its one-time terminal notice).
tui "$tmux_name" "$cols" "$rows"
tui "$tmux_phone" "$pcols" "$prows"
sleep 2
tmux -L "$tmux_name" kill-server
tmux -L "$tmux_phone" kill-server
sleep 0.5

tui "$tmux_name" "$cols" "$rows"
tui "$tmux_phone" "$pcols" "$prows"
record "$tmux_name" "$work/frames" &
rec_pid=$!
record "$tmux_phone" "$work/framesp" &
recp_pid=$!
sleep 1.5

# Ask for two threads.
type_text "$ask"
sleep 0.8
keys Enter
wait_for ratelimit running
wait_for invoice-rounding running
settle
sleep 3.5

# ratelimit asks to run its tests; the hub puts it in the conversation; answer it there.
touch "$work/gates/ratelimit-perm"
wait_for ratelimit waiting
settle
sleep 2.5
type_text "allow ratelimit"
sleep 0.9
keys Enter
wait_for ratelimit "done"
settle
sleep 1
touch "$work/gates/rounding-perm"
wait_for invoice-rounding "done"
settle
sleep 2.5

# A follow-up for one thread, by name.
type_text "$follow"
sleep 0.8
keys Enter
wait_for ratelimit running
settle
sleep 2.7
wait_for ratelimit "done"
settle
sleep 1.8
date +%s.%N > "$work/poster.t"
sleep 3

# Open ratelimit's transcript (Enter, Enter: its popup, then talk to it), and come back.
select_thread "$tmux_name" S-Up ratelimit
sleep 1
keys Enter
sleep 1
keys Enter
sleep 4.5
keys Escape
sleep 2

# The same on the phone: the Threads screen, the ratelimit card, its popup, in.
tmux -L "$tmux_phone" send-keys -l "/t"
tmux -L "$tmux_phone" send-keys Enter
sleep 1
select_thread "$tmux_phone" Down ratelimit
tmux -L "$tmux_phone" send-keys Enter
sleep 1
tmux -L "$tmux_phone" send-keys Enter
sleep 3.5

kill "$rec_pid" "$recp_pid" 2> /dev/null || true
wait "$rec_pid" 2> /dev/null || true
wait "$recp_pid" 2> /dev/null || true
rec_pid="" recp_pid=""

mkdir -p docs/img
python3 docs/tools/ansi2svg.py --frames "$work/frames" docs/img/tui-conversation.svg --max-hold 3.5 \
    --poster-time "$(cat "$work/poster.t")" --still docs/img/tui-conversation-still.svg \
    --title "twr in use: the user asks the orchestrator for two threads, answers a permission with allow ratelimit, both threads report back, then the user sends ratelimit a follow-up and it reports again"
python3 docs/tools/ansi2svg.py --frames "$work/framesp" docs/img/tui-conversation-phone.svg --max-hold 3.5 \
    --poster-time "$(cat "$work/poster.t")" --still docs/img/tui-conversation-phone-still.svg \
    --title "The same conversation in twr's compact phone layout, 46 columns wide"
ls -l docs/img/tui-conversation*.svg
