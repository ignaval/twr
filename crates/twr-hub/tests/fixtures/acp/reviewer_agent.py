#!/usr/bin/env python3
"""A scripted ACP agent (stdio, NDJSON JSON-RPC) as a loop's reviewer, for twr's hub tests of
ACP reviewers (crates/twr-hub/tests/loops_acp.rs).

Usage: reviewer_agent.py <state dir> [--no-modes]

Each process start increments <state>/procs and appends its stdin to <state>/stdin.jsonl.
A session reports the modes "default" (current) and "read-only" (none with --no-modes) both
as `modes` and as a `mode` config option, as claude-agent-acp 0.107.0 and codex-acp 2.6.0 do
(a mode set through either is reported by both, with a config_option_update), a model option
(m-large, m-small) and an effort option (medium, xhigh).

Prompt n (from 1, every process counted together in <state>/count) writes its text to
prompt.<n>, its working directory to cwd.<n> and its environment to env.<n>, then:
  fail.<n>    -> prints "boom: prompt <n> failed" to stderr and exits 3
  hold.<n>    -> starts a child (its pid in child.<n>, the agent's in pid.<n>) and waits while
                 the file is there
  tool.<n>    -> runs a shell tool unasked first, announced as claude-agent-acp 0.107.0 does:
                 `tool_call` "Terminal" with an empty rawInput, the command (the file's text)
                 in a later `tool_call_update`, then its completion
  write.<n>   -> writes the file's text to REVIEWED.md in its working directory, unasked (an
                 agent whose read-only mode did not hold)
  perm.<n>    -> asks session/request_permission first (allow_once, allow_always, reject_once,
                 reject_always) and writes the option chosen to perm-answer.<n>
  answers/<n>.txt -> its text is the reply (two chunks); else a reply with no findings in a
                 fenced json block
The prompt result reports usage as the real adapters do: 100 uncached input tokens beside
5000 cached read and 900 cached write ones, 10 output; before it, a usage_update reports the
process's cumulative cost (0.25 USD a prompt).
"""
import json
import os
import sys
import uuid

state = sys.argv[1]
args = sys.argv[2:]
no_modes = "--no-modes" in args


def bump(name):
    path = os.path.join(state, name)
    try:
        n = int(open(path).read().strip()) + 1
    except Exception:
        n = 1
    with open(path, "w") as f:
        f.write(str(n))
    return n


bump("procs")
log = open(os.path.join(state, "stdin.jsonl"), "a")
pending = []
mode = ["default"]
next_id = [100]


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def read():
    line = sys.stdin.readline()
    if not line:
        sys.exit(0)
    log.write(line)
    log.flush()
    return json.loads(line)


def next_message():
    if pending:
        return pending.pop(0)
    return read()


def wait_response(rid):
    while True:
        m = read()
        if m.get("id") == rid and "method" not in m:
            return m
        pending.append(m)


def chunk(sid, text):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {
        "sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}}}})


def config_options():
    opts = [
        {"id": "model", "name": "Model", "category": "model", "type": "select",
         "currentValue": "m-large", "options": [{"value": "m-large"}, {"value": "m-small"}]},
        {"id": "effort", "name": "Effort", "category": "thought_level", "type": "select",
         "currentValue": "medium", "options": [{"value": "medium"}, {"value": "xhigh"}]},
    ]
    if not no_modes:
        opts.insert(0, {"id": "mode", "name": "Mode", "category": "mode", "type": "select",
                        "currentValue": mode[0],
                        "options": [{"value": "default"}, {"value": "read-only"}]})
    return opts


def session_result(sid):
    r = {"sessionId": sid, "configOptions": config_options()}
    if not no_modes:
        r["modes"] = {"currentModeId": mode[0], "availableModes": [
            {"id": "default", "name": "Default"}, {"id": "read-only", "name": "Read only"}]}
    return r


def options_update(sid):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {
        "sessionUpdate": "config_option_update", "configOptions": config_options()}}})


spent = [0.0]


def prompt(msg):
    p = msg["params"]
    sid = p["sessionId"]
    text = "".join(b.get("text", "") for b in p["prompt"])
    n = bump("count")
    here = lambda name: os.path.join(state, f"{name}.{n}")
    open(here("prompt"), "w").write(text)
    open(here("cwd"), "w").write(os.getcwd())
    open(here("env"), "w").write("\n".join(f"{k}={v}" for k, v in os.environ.items()))
    if os.path.exists(here("fail")):
        sys.stderr.write(f"boom: prompt {n} failed\n")
        sys.stderr.flush()
        sys.exit(3)
    if os.path.exists(here("hold")):
        import subprocess
        import time
        child = subprocess.Popen(["sleep", "600"])
        open(here("child"), "w").write(str(child.pid))
        open(here("pid"), "w").write(str(os.getpid()))
        while os.path.exists(here("hold")):
            time.sleep(0.05)
        child.kill()
    if os.path.exists(here("tool")):
        command = open(here("tool")).read().strip()
        tid = f"tool-{n}"
        base = {"sessionId": sid}
        for update in [
            {"sessionUpdate": "tool_call", "toolCallId": tid, "title": "Terminal",
             "kind": "execute", "status": "pending", "rawInput": {}, "content": []},
            {"sessionUpdate": "tool_call_update", "toolCallId": tid, "title": command,
             "rawInput": {"command": command}},
            {"sessionUpdate": "tool_call_update", "toolCallId": tid, "status": "completed",
             "content": [{"type": "content", "content": {"type": "text", "text": "f81cb73 a"}}]},
        ]:
            send({"jsonrpc": "2.0", "method": "session/update",
                  "params": dict(base, update=update)})
    if os.path.exists(here("write")):
        with open("REVIEWED.md", "w") as f:
            f.write(open(here("write")).read())
    if os.path.exists(here("perm")):
        call = {"toolCallId": f"call-{n}", "title": "rm -rf build", "kind": "execute",
                "status": "pending", "rawInput": {"command": "rm -rf build"}}
        send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid,
              "update": dict(call, sessionUpdate="tool_call")}})
        next_id[0] += 1
        rid = next_id[0]
        send({"jsonrpc": "2.0", "id": rid, "method": "session/request_permission",
              "params": {"sessionId": sid, "toolCall": call, "options": [
                  {"optionId": "once", "name": "Allow", "kind": "allow_once"},
                  {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
                  {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                  {"optionId": "never", "name": "Never", "kind": "reject_always"}]}})
        r = wait_response(rid)
        outcome = r.get("result", {}).get("outcome", {})
        open(here("perm-answer"), "w").write(outcome.get("optionId", outcome.get("outcome", "")))
    answer = os.path.join(state, "answers", f"{n}.txt")
    if os.path.exists(answer):
        reply = open(answer).read()
    else:
        reply = "I read the diff.\n```json\n{\"findings\": []}\n```\n"
    half = len(reply) // 2
    chunk(sid, reply[:half])
    chunk(sid, reply[half:])
    spent[0] += 0.25
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": {
        "sessionUpdate": "usage_update", "used": 6000, "size": 200000,
        "cost": {"amount": spent[0], "currency": "USD"}}}})
    return {"stopReason": "end_turn", "usage": {
        "inputTokens": 100, "cachedReadTokens": 5000, "cachedWriteTokens": 900,
        "outputTokens": 10, "totalTokens": 6010}}


def handle(msg):
    method = msg.get("method")
    mid = msg.get("id")
    p = msg.get("params", {})
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": 1, "agentCapabilities": {"loadSession": True},
            "agentInfo": {"name": "reviewer-agent", "version": "0.0.1"}}})
    elif method == "session/new":
        send({"jsonrpc": "2.0", "id": mid, "result": session_result("rev-" + uuid.uuid4().hex[:8])})
    elif method == "session/set_mode":
        mode[0] = p.get("modeId")
        options_update(p.get("sessionId"))
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "session/set_config_option" and p.get("configId") == "mode":
        mode[0] = p.get("value")
        send({"jsonrpc": "2.0", "method": "session/update", "params": {
            "sessionId": p.get("sessionId"), "update": {
                "sessionUpdate": "current_mode_update", "currentModeId": mode[0]}}})
        send({"jsonrpc": "2.0", "id": mid, "result": {"configOptions": config_options()}})
    elif method in ("session/set_config_option", "session/set_model"):
        send({"jsonrpc": "2.0", "id": mid, "result": {}})
    elif method == "session/prompt":
        send({"jsonrpc": "2.0", "id": mid, "result": prompt(msg)})
    elif method == "session/cancel":
        pass
    elif mid is not None and method is not None:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found"}})


while True:
    handle(next_message())
