"""A scripted ACP worker for docs/tools/make-demo.sh.

It speaks the Agent Client Protocol on stdio and plays the script whose keyword is in the
prompt: replies, tool calls, files it really writes and commits in its working directory,
and permission requests that twr turns into cards. A `gate` step waits until the demo driver
creates a file of that name in the gate directory (argv[1]), so the driver sets the pace.
"""

import json
import os
import subprocess
import sys
import time

GATES = sys.argv[1]

SCRIPTS = {
    "rate limit": [
        ["text", "Adding a per-IP limit to POST /login. First, how a request reaches the handler."],
        ["tool", "Read src/routes/login.rs", "read"],
        ["tool", "Read src/middleware/mod.rs", "read"],
        ["tool", "Grep governor src/", "search"],
        ["text", "The webhooks module already uses governor, keyed by IP, so I'll reuse it."],
        ["write", "src/middleware/rate_limit.rs", "// LoginLimiter: 5 attempts per IP per minute\n"],
        ["tool", "Edit src/routes/login.rs", "edit"],
        ["write", "tests/login_rate_limit.rs", "// the sixth login from one IP in a minute gets 429\n"],
        ["commit", "Rate limit POST /login per client IP"],
        ["gate", "ratelimit-perm"],
        ["perm", "cargo test -p api", "execute"],
        ["tool", "cargo test -p api", "execute"],
        ["sleep", 1.2],
        ["text", "Added a per-IP limiter to POST /login: 5 attempts a minute, then 429 with "
                 "Retry-After. All 31 api tests pass, including the new one."],
    ],
    "invoice": [
        ["text", "Looking at how tax is computed today."],
        ["tool", "Read src/invoice/totals.rs", "read"],
        ["tool", "Read src/ledger/post.rs", "read"],
        ["write", "src/invoice/totals.rs", "// round tax once, on the invoice total\n"],
        ["tool", "Edit src/invoice/totals.rs", "edit"],
        ["commit", "Round tax once on the invoice total"],
        ["gate", "rounding-perm"],
        ["perm", "git push -u origin twr/invoice-rounding", "execute"],
        ["sleep", 0.6],
        ["text", "Tax is now rounded once on the invoice total instead of per line. "
                 "The 12 invoice tests pass and the branch is pushed."],
    ],
    "changelog": [
        ["tool", "git log v0.4.0..HEAD --oneline", "execute"],
        ["write", "CHANGELOG.md", "## 0.5.0\n"],
        ["commit", "Draft the 0.5.0 changelog"],
        ["text", "Drafted the 0.5.0 changelog from 37 commits, with one breaking change."],
    ],
    "accessibility": [
        ["tool", "npx axe http://localhost:5173/checkout", "execute"],
        ["write", "src/pages/checkout.tsx", "// labels for the address inputs\n"],
        ["commit", "Label the checkout address inputs"],
        ["text", "Found 6 problems on checkout and fixed 5. The low-contrast coupon hint needs a design call."],
    ],
}

cwd = {}


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def update(sid, u):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": sid, "update": u}})


calls = [0]


def tool(sid, title, kind, done=True):
    calls[0] += 1
    cid = "call-%d" % calls[0]
    update(sid, {"sessionUpdate": "tool_call", "toolCallId": cid, "title": title,
                 "kind": kind, "status": "pending", "rawInput": {"command": title}})
    time.sleep(0.45)
    if done:
        update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": cid,
                     "status": "completed"})
    return cid


def git(sid, *args):
    subprocess.run(["git", "-c", "user.name=Dana Ruiz", "-c", "user.email=dana@example.com",
                    *args], cwd=cwd[sid], check=True, capture_output=True)


pending = []  # messages read while waiting for a response


def wait_response(rid):
    while True:
        line = sys.stdin.readline()
        if not line:
            sys.exit(0)
        msg = json.loads(line)
        if msg.get("id") == rid and "method" not in msg:
            return msg
        pending.append(msg)


def run(sid, prompt):
    script = next((s for k, s in SCRIPTS.items() if k in prompt.lower()), None)
    if script is None:
        update(sid, {"sessionUpdate": "agent_message_chunk",
                     "content": {"type": "text", "text": "Done."}})
        return
    for step in script:
        kind = step[0]
        if kind == "text":
            update(sid, {"sessionUpdate": "agent_message_chunk",
                         "content": {"type": "text", "text": step[1]}})
            time.sleep(0.4)
        elif kind == "tool":
            tool(sid, step[1], step[2])
        elif kind == "write":
            path = os.path.join(cwd[sid], step[1])
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "a") as f:
                f.write(step[2])
            tool(sid, "Write " + step[1], "edit")
        elif kind == "commit":
            git(sid, "add", "-A")
            git(sid, "commit", "-q", "-m", step[1])
            # Track main, so the card shows how far ahead the thread is.
            git(sid, "branch", "-q", "--set-upstream-to=main")
        elif kind == "sleep":
            time.sleep(step[1])
        elif kind == "gate":
            while not os.path.exists(os.path.join(GATES, step[1])):
                time.sleep(0.1)
        elif kind == "perm":
            calls[0] += 1
            cid = "perm-%d" % calls[0]
            rid = "req-%d" % calls[0]
            send({"jsonrpc": "2.0", "id": rid, "method": "session/request_permission",
                  "params": {"sessionId": sid,
                             "toolCall": {"toolCallId": cid, "title": step[1], "kind": step[2],
                                          "status": "pending", "rawInput": {"command": step[1]}},
                             "options": [
                                 {"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                                 {"optionId": "reject", "name": "Reject", "kind": "reject_once"}]}})
            answer = wait_response(rid)
            outcome = answer.get("result", {}).get("outcome", {})
            if outcome.get("optionId") != "allow":
                update(sid, {"sessionUpdate": "agent_message_chunk",
                             "content": {"type": "text", "text": "\n\nStopped: not allowed."}})
                return


def handle(msg):
    method, mid, p = msg.get("method"), msg.get("id"), msg.get("params", {})
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": 1,
            "agentCapabilities": {"promptCapabilities": {}, "mcpCapabilities": {}},
            "agentInfo": {"name": "scripted-worker", "version": "1"}}})
    elif method == "session/new":
        sid = "w-%d" % (len(cwd) + 1)
        cwd[sid] = p.get("cwd") or os.getcwd()
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "sessionId": sid,
            "models": {"currentModelId": "scripted-1",
                       "availableModels": [{"modelId": "scripted-1", "name": "scripted-1"}]}}})
    elif method == "session/prompt":
        sid = p["sessionId"]
        run(sid, "".join(b.get("text", "") for b in p.get("prompt", [])))
        send({"jsonrpc": "2.0", "id": mid, "result": {"stopReason": "end_turn"}})
    elif mid is not None and method is not None:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found"}})


while True:
    msg = pending.pop(0) if pending else None
    if msg is None:
        line = sys.stdin.readline()
        if not line:
            break
        msg = json.loads(line)
    handle(msg)
