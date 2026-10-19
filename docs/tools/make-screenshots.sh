#!/bin/sh
# Regenerates docs/img/tui-stack.svg, docs/img/tui-grid.svg and docs/img/tui-compact.svg from
# the real TUI.
#
#   docs/tools/make-screenshots.sh
#
# It builds twr and starts a throwaway hub (TWR_HOME under /tmp) with the scripted test
# runtime (`fake`, TWR_ENABLE_FAKE=1) for the worker threads, and a scripted orchestrator: a
# small ACP agent written below (orch.py) that answers the messages in script.json and sums
# up each hub notice in a sentence. It makes five small git repositories, says hello to the
# orchestrator through the TUI, and drives eight threads into different states: waiting on a
# permission, asking a question, failed, running, done and parked. The orchestrator hears
# about them from the hub, and then the TUI asks it what needs the user. Then it runs `twr` in
# a private tmux server, captures the stack view (160x42), the grid view (160x25) and the
# compact layout's Threads screen (40x20, a phone) with their colors, and turns the captures
# into SVG with docs/tools/ansi2svg.py. No real agent
# runs: the hub and the TUI get a PATH without claude, codex or grok, and HOME is the
# temporary directory. Everything the script started is stopped and removed on exit.
#
# Needs git, tmux and python3. Takes a minute or two (it waits for the hub's 30 s git tick).
# TWR_SHOTS_KEEP=<dir> also copies the raw captures (stack.ans, grid.ans, compact.ans) there.

set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
cargo build --quiet -p twr-cli
twr="$root/target/debug/twr"

work=$(mktemp -d /tmp/twr-shots.XXXXXX)
tmux_name="twrshots-$$"
hub_pid=""

cleanup() {
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
        # The hub is this shell's child: until `wait` reaps it, its pid is not reused.
        if kill -0 "$hub_pid" 2> /dev/null; then
            kill -9 "$hub_pid" 2> /dev/null || true
        fi
        wait "$hub_pid" 2> /dev/null || true
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

mkrepo api Cargo.toml src/main.rs src/routes/login.rs src/routes/mod.rs src/middleware/mod.rs \
    src/config.rs src/webhooks/limiter.rs config/default.toml
mkrepo web package.json src/pages/checkout.tsx src/lib/discounts.ts
mkrepo billing Cargo.toml src/invoice/totals.rs src/ledger/post.rs
mkrepo docs README.md CHANGELOG.md scripts/check-links.sh
mkrepo infra main.tf versions.tf .github/workflows/deploy.yml

export TWR_HOME="$work/twr"
mkdir -p "$TWR_HOME" "$work/home"
cat > "$TWR_HOME/config.toml" << EOF
default_runtime = "fake"
stall_after_secs = 0

[orchestrator]
runtime = "scripted"

[runtimes.scripted]
acp = "python3 $work/orch.py $work/script.json"

[projects.api]
repo = "$work/code/api"
[projects.web]
repo = "$work/code/web"
[projects.billing]
repo = "$work/code/billing"
[projects.docs]
repo = "$work/code/docs"
[projects.infra]
repo = "$work/code/infra"
EOF

# The scripted orchestrator: an ACP agent on stdio that plays the steps script.json gives a
# user message (["tool", title] and ["text", reply]) and answers a hub notice with one
# sentence per update. It reports the model `scripted-1`.
cat > "$work/orch.py" << 'EOF'
import json
import sys

script = json.load(open(sys.argv[1]))
calls = [0]


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def update(sid, u):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": sid, "update": u}})


def text(sid, t):
    update(sid, {"sessionUpdate": "agent_message_chunk",
                 "content": {"type": "text", "text": t}})


def tool(sid, title):
    calls[0] += 1
    cid = "call-%d" % calls[0]
    update(sid, {"sessionUpdate": "tool_call", "toolCallId": cid, "title": title,
                 "kind": "other", "status": "pending", "rawInput": {}})
    update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": cid,
                 "status": "completed"})


def field(line, key):
    at = line.find(key + "=")
    if at < 0:
        return ""
    rest = line[at + len(key) + 1:]
    if rest.startswith('"'):
        try:
            return json.JSONDecoder().raw_decode(rest)[0]
        except ValueError:
            return rest
    return rest.split(" ")[0]


def notice_reply(notice):
    lines = [l for l in notice.splitlines() if l.startswith("- thread=")]
    failed = {field(l, "thread") for l in lines if field(l, "reason") == "error"}
    out = []
    for line in lines:
        name, reason = field(line, "thread"), field(line, "reason")
        summary = field(line, "summary").rstrip(".")
        if " report " in line:
            if name not in failed:
                out.append("%s is done: %s." % (name, summary))
        elif reason == "permission":
            out.append("%s waits for a permission (%s); answer it on its card." % (
                name, summary))
        elif reason == "question":
            out.append("%s asks: %s" % (name, summary))
        elif reason == "error":
            out.append("%s failed: %s." % (name, summary.split(": ", 1)[-1]))
        elif reason == "review":
            out.append("%s needs a look: %s." % (name, summary))
    return " ".join(out) or "Nothing that needs you."


def run_prompt(sid, prompt):
    # The message is the prompt's last paragraph (the instructions come first, once), after
    # the hub's context when it gives some (`[user message]` marks where the message starts).
    message = prompt.strip().split("\n\n")[-1].split("[user message]\n")[-1].strip()
    if message.startswith("[twr notice]"):
        tool(sid, "twr list_threads")
        text(sid, notice_reply(message))
        return
    for kind, value in script.get(message, [["text", "Noted."]]):
        if kind == "tool":
            tool(sid, value)
        else:
            text(sid, value)


while True:
    line = sys.stdin.readline()
    if not line:
        break
    msg = json.loads(line)
    method, mid, p = msg.get("method"), msg.get("id"), msg.get("params", {})
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"promptCapabilities": {}, "mcpCapabilities": {}},
            "agentInfo": {"name": "scripted-orchestrator", "version": "1"}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "sessionId": "scripted-1",
            "models": {"currentModelId": "scripted-1",
                       "availableModels": [{"modelId": "scripted-1", "name": "scripted-1"}]}}})
    elif method == "session/prompt":
        run_prompt(p["sessionId"], "".join(b.get("text", "") for b in p.get("prompt", [])))
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    elif mid is not None and method is not None:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found"}})
EOF
cat > "$work/script.json" << 'EOF'
{
  "Morning! I'm cutting the 0.5.0 release today. Keep an eye on the threads and tell me when one needs me.": [
    ["text", "Morning. I'll watch the threads as they start and tell you when one needs you."]
  ],
  "What needs me right now?": [
    ["tool", "twr attention_list"],
    ["tool", "twr list_threads"],
    ["text", "Three things. ratelimit wants to run cargo test -p api auth:: to check its new login limiter: select its card and press Enter, then 1 allows. invoice-rounding asks whether tax is rounded per line or once on the invoice total. deploy-fix stopped because run 81719 still holds the Terraform state lock; I can retry it once that run is done."],
    ["text", "\n\nThe rest is fine: checkout-ui is building the coupon field, and the changelog, the quick start and the accessibility fixes are done."]
  ]
}
EOF

# The agent binaries point at `false`, in case one of them is in /usr/bin.
env -i HOME="$work/home" PATH=/usr/bin:/bin TWR_HOME="$TWR_HOME" TWR_ENABLE_FAKE=1 \
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
        if [ "$i" -gt 100 ]; then
            echo "make-screenshots: $1 never got to $2 (it is $(status "$1"))" >&2
            exit 1
        fi
        sleep 0.2
    done
}

# commit THREAD PROJECT FILE MESSAGE: a commit on the thread's branch, in its worktree.
commit() {
    wt="$TWR_HOME/data/worktrees/$2/$1"
    mkdir -p "$wt/$(dirname "$3")"
    printf '%s\n' "$4" >> "$wt/$3"
    git -C "$wt" add -A
    git -C "$wt" commit -q -m "$4"
}

new() {
    "$twr" thread new "$1" --name "$2" > /dev/null
}

# wait_orchestrator: until the orchestrator has no turn running (it is idle between turns).
wait_orchestrator() {
    i=0
    until [ "$(status orchestrator)" = idle ]; do
        i=$((i + 1))
        if [ "$i" -gt 150 ]; then
            echo "make-screenshots: the orchestrator is still $(status orchestrator)" >&2
            exit 1
        fi
        sleep 0.2
    done
}

# first_frame: until the TUI in the tmux server has drawn its first frame (the input box's
# title): it drops what is typed before that, and right after it.
first_frame() {
    i=0
    until tmux -L "$tmux_name" capture-pane -p | grep -q "Enter sends"; do
        i=$((i + 1))
        if [ "$i" -gt 150 ]; then
            echo "make-screenshots: the TUI never drew" >&2
            exit 1
        fi
        sleep 0.2
    done
    sleep 0.5
}

# tui_say MESSAGE: type MESSAGE into the TUI, as the user does, and wait for the
# orchestrator's answer.
tui_say() {
    tmux -L "$tmux_name" -f /dev/null new-session -d -x 160 -y 42 \
        "env -i HOME='$work/home' PATH=/usr/bin:/bin TERM=xterm-256color TWR_HOME='$TWR_HOME' '$twr'"
    first_frame
    # What is typed goes to the orchestrator.
    tmux -L "$tmux_name" send-keys -l "$1"
    tmux -L "$tmux_name" send-keys Enter
    sleep 1
    wait_orchestrator
    tmux -L "$tmux_name" kill-server
    sleep 0.3
}

tui_say "Morning! I'm cutting the 0.5.0 release today. Keep an eye on the threads and tell me when one needs me."

# The fake runtime plays a prompt line by line: `!tool`, `!write`, `!perm`, `!ask` and
# `!sleep` lines become tool calls, files, cards and pauses; other lines are its reply.

new infra tf-upgrade
"$twr" send tf-upgrade "!tool Bash terraform init -upgrade
!tool Bash terraform plan
Upgraded the AWS provider to 5.x. The plan shows no resource changes." > /dev/null
wait_for tf-upgrade "done"
"$twr" park tf-upgrade > /dev/null

new docs changelog
commit changelog docs CHANGELOG.md "Draft the 0.5.0 changelog"
"$twr" send changelog "!tool Bash git log v0.4.0..HEAD --oneline
Drafted the 0.5.0 changelog from 37 commits: features, fixes and one breaking change (the --json output of thread list)." > /dev/null
wait_for changelog "done"

new web a11y-audit
"$twr" send a11y-audit "!tool Bash npx axe http://localhost:5173/checkout
!write src/pages/checkout.tsx // labels for the address inputs
!write src/components/AddressModal.tsx // focus returns to the trigger on close
Found 6 problems on checkout: 4 inputs without labels, low contrast on the coupon hint and a focus trap in the address modal. Fixed the labels and the trap. The contrast needs a design call." > /dev/null
wait_for a11y-audit "done"

new docs quickstart
commit quickstart docs README.md "Rewrite the quick start"
commit quickstart docs docs/configuration.md "Move the config reference to its own page"
"$twr" send quickstart "!tool Read README.md
!tool Edit README.md
!tool Bash ./scripts/check-links.sh
Rewrote the quick start around three commands and moved the config reference to its own page. All 41 links resolve." > /dev/null
wait_for quickstart "done"

new web checkout-ui
commit checkout-ui web src/components/PriceSummary.tsx "Extract PriceSummary from the checkout page"
# A commit on main after the thread branched: the card shows it one behind.
printf '%s\n' '"vite": "5.4.2"' >> "$work/code/web/package.json"
git -C "$work/code/web" commit -q -am "Bump vite to 5.4.2"
"$twr" send checkout-ui "!tool Read src/pages/checkout.tsx
!tool Edit src/pages/checkout.tsx
Moved the price breakdown into a PriceSummary component and committed it. Checkout renders the same and the snapshot tests pass." > /dev/null
wait_for checkout-ui "done"
"$twr" send checkout-ui "Now the coupon field. Reading how discounts are applied today.
!tool Read src/lib/discounts.ts
!tool Grep applyCoupon src/
!write src/components/CouponField.tsx // coupon input with inline validation
!sleep 86400000" > /dev/null
wait_for checkout-ui running

new infra deploy-fix
"$twr" send deploy-fix "!tool Bash gh run view 81723 --log-failed
!tool Read .github/workflows/deploy.yml
!error terraform plan failed: the state lock is held by run 81719" > /dev/null
wait_for deploy-fix error

new billing invoice-rounding
"$twr" send invoice-rounding "Checking how invoice totals round tax.
!tool Read src/invoice/totals.rs
!tool Read src/ledger/post.rs
!ask Round tax per line or once on the invoice total?|per line|on the total" > /dev/null
wait_for invoice-rounding waiting

new api ratelimit
commit ratelimit api src/net.rs "Extract client_ip() from the login handler"
"$twr" send ratelimit "I'll add a per-IP limit to POST /login. First, how a request reaches the handler.
!tool Read src/routes/login.rs
!tool Read src/routes/mod.rs
!tool Read src/middleware/mod.rs
!tool Grep RateLimit|governor src/
!tool Read src/webhooks/limiter.rs
!tool Read src/config.rs
!tool Grep trusted_proxies src/
There is no limiter on login yet. The webhooks module already uses governor, keyed by IpAddr, so I'll reuse it instead of adding a dependency.
Plan:
1. A LoginLimiter keyed by client IP: 5 attempts per minute, burst 5.
2. Answer 429 with Retry-After when the bucket is empty.
3. Take the client IP from X-Forwarded-For only behind a trusted proxy (server.trusted_proxies).
!write src/middleware/rate_limit.rs // LoginLimiter: a per-IP token bucket for POST /login
!tool Edit src/middleware/mod.rs
!tool Edit src/routes/login.rs
!tool Edit src/config.rs
!tool Edit config/default.toml
The limits come from config (login.max_attempts, login.window_secs) and default to the values above.
!write tests/login_rate_limit.rs // six logins from one IP in a minute: the sixth gets 429
Added a test: six logins from one IP inside a minute, and the sixth must get 429 with Retry-After: 60.
It uses a paused clock, so it runs in milliseconds.
Running the auth tests to check nothing else broke.
!perm Bash cargo test -p api auth::" > /dev/null
wait_for ratelimit waiting

# Git snapshots of running threads come with the hub's 30 s tick; meanwhile the hub tells the
# orchestrator what happened.
sleep 32
wait_orchestrator
tui_say "What needs me right now?"

# capture NAME COLS ROWS KEYS...: run the TUI at COLS x ROWS, press KEYS, capture the screen.
# The TUI opens with nothing selected; Tab selects the first card (or tile).
capture() {
    name=$1 cols=$2 rows=$3
    shift 3
    tmux -L "$tmux_name" -f /dev/null new-session -d -x "$cols" -y "$rows" \
        "env -i HOME='$work/home' PATH=/usr/bin:/bin TERM=xterm-256color TWR_HOME='$TWR_HOME' '$twr'"
    first_frame
    # A pause after each key, as a person presses them.
    for k in "$@"; do
        tmux -L "$tmux_name" send-keys "$k"
        sleep 0.3
    done
    # The TUI loads transcripts as it goes: wait until the screen stops changing. The working
    # icon moves on its own (_ ▁ … ▆): its frames are not changes.
    : > "$work/$name.txt"
    i=0
    while :; do
        sleep 1
        tmux -L "$tmux_name" capture-pane -p |
            sed -e 's/▁/_/g' -e 's/▂/_/g' -e 's/▃/_/g' -e 's/▄/_/g' \
                -e 's/▅/_/g' -e 's/▆/_/g' > "$work/$name.new"
        if cmp -s "$work/$name.new" "$work/$name.txt"; then
            break
        fi
        mv "$work/$name.new" "$work/$name.txt"
        i=$((i + 1))
        if [ "$i" -gt 30 ]; then
            echo "make-screenshots: the $name screen never settled" >&2
            exit 1
        fi
    done
    # A working icon is captured with its cursor up, where it shows (for a breath at most:
    # with nothing working, no frame ever shows).
    i=0
    while [ "$i" -lt 40 ] &&
        ! tmux -L "$tmux_name" capture-pane -p | grep -q -e '▅' -e '▆'; do
        sleep 0.03
        i=$((i + 1))
    done
    tmux -L "$tmux_name" capture-pane -e -p > "$work/$name.ans"
    tmux -L "$tmux_name" kill-server
    sleep 0.3
}

capture stack 160 42 Tab
capture grid 160 25 C-g Tab
# A phone: below 60 columns the TUI is compact. `/t` shows the Threads screen, the first card
# selected; three Downs (past the permission, the question and the failure) select the running
# one, the cards following it.
capture compact 40 20 /t Enter Down Down Down

[ -z "${TWR_SHOTS_KEEP:-}" ] || cp "$work/stack.ans" "$work/grid.ans" "$work/compact.ans" "$TWR_SHOTS_KEEP/"

# The fake runtime's directive lines must not show (the permission thread's transcript
# scrolls its prompt out of view).
if grep -q '!perm\|!tool\|!write' "$work/stack.ans" "$work/grid.ans" "$work/compact.ans"; then
    echo "make-screenshots: a prompt's directive lines are on screen; lengthen the reply" >&2
    exit 1
fi

mkdir -p docs/img
python3 docs/tools/ansi2svg.py "$work/stack.ans" docs/img/tui-stack.svg \
    --title "twr stack view: the conversation with the orchestrator, and the worker thread cards with a permission and a question"
python3 docs/tools/ansi2svg.py "$work/grid.ans" docs/img/tui-grid.svg \
    --title "twr grid view: one tile per worker thread"
python3 docs/tools/ansi2svg.py "$work/compact.ans" docs/img/tui-compact.svg \
    --title "twr on a phone: the compact layout's tab bar over the Threads screen's cards"
ls -l docs/img/tui-stack.svg docs/img/tui-grid.svg docs/img/tui-compact.svg
