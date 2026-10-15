#!/usr/bin/env python3
"""A scripted ACP agent (stdio, NDJSON JSON-RPC) for twr's generic ACP runtime tests.

Usage: stub_agent.py <state dir> [options]
  --no-resume        do not advertise sessionCapabilities.resume
  --no-load          do not advertise loadSession
  --modes JSON       report {"currentModeId": ..., "availableModes": [...]} on session open
  --mode-option JSON report the mode as a configOptions entry of category "mode"
  --refuse-set-mode  answer session/set_mode (and mode set_config_option) with an error
  --models           report a `models` object (session/set_model)
  --model-option     report a configOptions model option (session/set_config_option)
  --effort-option    report a configOptions option of category "thought_level" (low,
                     medium, high; session/set_config_option)
  --auth-required    fail session/new with "Authentication required"
  --model-mode MODE  a model change (session/set_model, or session/set_config_option of the
                     model option) also switches the session to MODE, and its result reports
                     the whole configuration (as ACP results do)
  --config-results   session/set_mode and mode set_config_option results report the whole
                     configuration too
  --ignore-set-mode  accept mode requests without changing the mode
  --prompt-modes JSON session/prompt results report this `modes` object (the agent changed
                     its mode during the prompt)

Every line read from stdin is appended to <state dir>/stdin.jsonl. Sessions persist in
<state dir>/sessions.json (so a new process can resume one). The prompt text picks the
behavior (the last line of the prompt, after any system instructions):
  hello   -> two message chunks, end_turn
  perm    -> a tool call + session/request_permission (allow_once/allow_always/reject_once/
             reject_always); allow_once -> tool completes, end_turn; anything else -> cancelled
  wait    -> waits for session/cancel, then stopReason cancelled
  widen   -> current_mode_update to "yolo", then waits for session/cancel or a mode request
  recall  -> answers with the prompts this session saw before
  other   -> echoes the text, end_turn
"""
import json
import os
import sys
import uuid

state = sys.argv[1]
args = sys.argv[2:]


def opt(name):
    if name in args:
        i = args.index(name)
        return args[i + 1] if i + 1 < len(args) and not args[i + 1].startswith("--") else True
    return None


log = open(os.path.join(state, "stdin.jsonl"), "a")
sessions_file = os.path.join(state, "sessions.json")
with open(os.path.join(state, "run.json"), "w") as f:
    json.dump({"args": sys.argv[1:], "cwd": os.getcwd(), "pid": os.getpid(),
               "env": dict(os.environ)}, f)


def load_sessions():
    try:
        return json.load(open(sessions_file))
    except Exception:
        return {}


def save_sessions(s):
    json.dump(s, open(sessions_file, "w"))


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


def update(sid, u):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": sid, "update": u}})


def chunk(sid, text):
    update(sid, {"sessionUpdate": "agent_message_chunk",
                 "content": {"type": "text", "text": text}})


cur_mode = [None]  # the session's mode once a request changed it


def session_result(sid):
    r = {"sessionId": sid}
    if opt("--modes"):
        r["modes"] = json.loads(opt("--modes"))
        if cur_mode[0] is not None:
            r["modes"]["currentModeId"] = cur_mode[0]
    opts = []
    if opt("--mode-option"):
        o = json.loads(opt("--mode-option"))
        if cur_mode[0] is not None:
            o["currentValue"] = cur_mode[0]
        opts.append(o)
    if opt("--model-option"):
        opts.append({"id": "llm", "name": "Model", "category": "model", "type": "select",
                     "currentValue": "m1", "options": [{"value": "m1"}, {"value": "m2"}]})
    if opt("--effort-option"):
        opts.append({"id": "effort", "name": "Effort", "category": "thought_level",
                     "type": "select", "currentValue": "medium",
                     "options": [{"value": "low"}, {"value": "medium"}, {"value": "high"}]})
    if opts:
        r["configOptions"] = opts
    if opt("--models"):
        r["models"] = {"currentModelId": "m1", "availableModels": [{"modelId": "m1"}]}
    return r


pending = []  # messages read while waiting for something else
next_id = [100]


def next_message():
    if pending:
        return pending.pop(0)
    return read()


def wait_response(rid):
    """Reads until the response to our request `rid`; queues everything else."""
    while True:
        m = read()
        if m.get("id") == rid and "method" not in m:
            return m
        pending.append(m)


def run_prompt(msg, sid, sessions):
    blocks = msg["params"]["prompt"]
    text = "".join(b.get("text", "") for b in blocks)
    sessions.setdefault(sid, {"prompts": []})
    seen = list(sessions[sid]["prompts"])
    sessions[sid]["prompts"].append(text)
    save_sessions(sessions)
    word = text.strip().splitlines()[-1].strip() if text.strip() else ""
    rid = msg["id"]
    if word == "hello":
        chunk(sid, "Hel")
        chunk(sid, "lo!")
        return {"stopReason": "end_turn"}
    if word == "recall":
        chunk(sid, "before: " + " | ".join(p.strip().splitlines()[-1] for p in seen))
        return {"stopReason": "end_turn"}
    if word == "perm":
        call = {"toolCallId": "call-1", "title": "touch hello.txt", "kind": "edit",
                "status": "pending", "rawInput": {"path": "hello.txt"}}
        update(sid, dict(call, sessionUpdate="tool_call"))
        next_id[0] += 1
        pid = next_id[0]
        send({"jsonrpc": "2.0", "id": pid, "method": "session/request_permission",
              "params": {"sessionId": sid, "toolCall": call, "options": [
                  {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
                  {"optionId": "once", "name": "Allow", "kind": "allow_once"},
                  {"optionId": "never", "name": "Never", "kind": "reject_always"},
                  {"optionId": "no", "name": "Reject", "kind": "reject_once"}]}})
        r = wait_response(pid)
        outcome = r.get("result", {}).get("outcome", {})
        if outcome.get("outcome") == "selected" and outcome.get("optionId") == "once":
            update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": "call-1",
                         "status": "completed",
                         "content": [{"type": "content",
                                      "content": {"type": "text", "text": "done"}}]})
            chunk(sid, "Created hello.txt")
            return {"stopReason": "end_turn"}
        return {"stopReason": "cancelled"}
    if word in ("wait", "widen"):
        if word == "widen":
            update(sid, {"sessionUpdate": "current_mode_update", "currentModeId": "yolo"})
        while True:
            m = read()
            if m.get("method") == "session/cancel":
                return {"stopReason": "cancelled"}
            if m.get("method") in ("session/set_mode", "session/set_config_option"):
                handle(m, sessions)
                continue
            pending.append(m)
    chunk(sid, "echo: " + word)
    return {"stopReason": "end_turn"}


def handle(msg, sessions):
    method = msg.get("method")
    mid = msg.get("id")
    p = msg.get("params", {})
    if method == "initialize":
        caps = {"promptCapabilities": {"image": False}, "mcpCapabilities": {"http": False}}
        if not opt("--no-load"):
            caps["loadSession"] = True
        if not opt("--no-resume"):
            caps["sessionCapabilities"] = {"resume": {}}
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "protocolVersion": 1, "agentCapabilities": caps,
            "agentInfo": {"name": "stub-agent", "version": "0.0.1"},
            "authMethods": [{"id": "stub-login", "name": "Stub login"}]}})
    elif method == "session/new":
        if opt("--auth-required"):
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32000, "message": "Authentication required"}})
            return
        sid = "sess-" + uuid.uuid4().hex[:8]
        sessions[sid] = {"prompts": [], "mcp": p.get("mcpServers")}
        save_sessions(sessions)
        send({"jsonrpc": "2.0", "id": mid, "result": session_result(sid)})
    elif method in ("session/resume", "session/load"):
        sid = p.get("sessionId")
        if sid not in sessions:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32002, "message": "no such session"}})
            return
        if method == "session/load":
            # Replays history before answering (twr drops it).
            for t in sessions[sid]["prompts"]:
                update(sid, {"sessionUpdate": "user_message_chunk",
                             "content": {"type": "text", "text": t}})
                chunk(sid, "(replayed)")
        r = session_result(sid)
        if method == "session/resume":
            r.pop("sessionId")
        send({"jsonrpc": "2.0", "id": mid, "result": r})
    elif method in ("session/set_mode", "session/set_config_option", "session/set_model"):
        if p.get("configId") == "effort":
            send({"jsonrpc": "2.0", "id": mid, "result": {}})
            return
        model = method == "session/set_model" or p.get("configId") == "llm"
        if opt("--refuse-set-mode") and not model:
            send({"jsonrpc": "2.0", "id": mid,
                  "error": {"code": -32603, "message": "mode change refused"}})
            return
        if not model and not opt("--ignore-set-mode"):
            cur_mode[0] = p.get("modeId", p.get("value"))
        elif opt("--model-mode"):
            cur_mode[0] = opt("--model-mode")
        result = {}
        if (model and opt("--model-mode")) or (not model and opt("--config-results")):
            result = session_result(p.get("sessionId"))
            result.pop("sessionId")
        send({"jsonrpc": "2.0", "id": mid, "result": result})
    elif method == "session/prompt":
        r = run_prompt(msg, p["sessionId"], sessions)
        if opt("--prompt-modes"):
            r["modes"] = json.loads(opt("--prompt-modes"))
        send({"jsonrpc": "2.0", "id": mid, "result": r})
    elif method == "session/cancel":
        pass
    elif mid is not None and method is not None:
        send({"jsonrpc": "2.0", "id": mid,
              "error": {"code": -32601, "message": "method not found"}})


def main():
    sessions = load_sessions()
    while True:
        handle(next_message(), sessions)


main()
