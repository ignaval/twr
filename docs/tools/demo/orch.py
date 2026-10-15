"""A scripted orchestrator for docs/tools/make-demo.sh.

It speaks the Agent Client Protocol on stdio, like any ACP orchestrator, and starts the twr
MCP server the hub gives it in `session/new`. A user message plays the steps the script
(argv[1], JSON) gives it: ["call", tool, arguments] calls that twr tool for real,
["text", reply] answers. In arguments, "{attn:NAME}" stands for the open attention item of
thread NAME, as the hub's notices name it. A hub notice gets one sentence per update.
"""

import json
import os
import subprocess
import sys

script = json.load(open(sys.argv[1]))
mcp = None
attn = {}  # thread name -> attn_id of its open item
calls = [0]


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def update(sid, u):
    send({"jsonrpc": "2.0", "method": "session/update",
          "params": {"sessionId": sid, "update": u}})


def text(sid, t):
    update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": t}})


class Mcp:
    def __init__(self, server):
        env = dict(os.environ)
        for e in server.get("env") or []:
            env[e["name"]] = e["value"]
        self.proc = subprocess.Popen([server["command"], *server.get("args", [])], env=env,
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.next_id = 0
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "scripted-orchestrator",
                                                   "version": "1"}})
        self.write({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def write(self, obj):
        self.proc.stdin.write(json.dumps(obj) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params):
        self.next_id += 1
        self.write({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        while True:
            msg = json.loads(self.proc.stdout.readline())
            if msg.get("id") == self.next_id:
                return msg

    def call(self, name, arguments):
        return self.request("tools/call", {"name": name, "arguments": arguments})


def fill(value):
    if isinstance(value, str) and value.startswith("{attn:"):
        return attn.get(value[6:-1], "")
    return value


def call(sid, name, arguments):
    calls[0] += 1
    cid = "call-%d" % calls[0]
    args = {k: fill(v) for k, v in arguments.items()}
    update(sid, {"sessionUpdate": "tool_call", "toolCallId": cid, "title": "twr " + name,
                 "kind": "other", "status": "pending", "rawInput": args})
    result = mcp.call(name, args)
    sys.stderr.write("orch: %s %s -> %s\n" % (name, json.dumps(args), json.dumps(result)))
    update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": cid,
                 "status": "failed" if "error" in result else "completed"})


def field(line, key):
    at = line.find(" " + key + "=")
    if at < 0:
        return ""
    rest = line[at + len(key) + 2:]
    if rest.startswith('"'):
        try:
            return json.JSONDecoder().raw_decode(rest)[0]
        except ValueError:
            return rest
    return rest.split(" ")[0]


def notice_reply(notice):
    out = []
    for line in notice.splitlines():
        if not line.startswith("- thread="):
            continue
        name = line[len("- thread="):].split(" ")[0]
        reason = field(line, "reason")
        summary = field(line, "summary").rstrip(".")
        if " report " in line:
            out.append("%s is done: %s." % (name, summary))
        elif reason == "permission":
            attn[name] = field(line, "attn_id")
            # "Allow execute: git push" -> "git push"
            command = summary.removeprefix("Allow ").rstrip("?").split(": ", 1)[-1]
            out.append("%s wants to run %s. Allow it?" % (name, command))
        elif reason == "question":
            out.append("%s asks: %s" % (name, summary))
        elif reason == "error":
            out.append("%s failed: %s." % (name, summary))
    return " ".join(out)


def run_prompt(sid, prompt):
    # The message is the prompt's last paragraph (the instructions come first, once), after
    # the hub's context when it gives some (`[user message]` marks where the message starts).
    message = prompt.strip().split("\n\n")[-1].split("[user message]\n")[-1].strip()
    if message.startswith("[twr notice]"):
        sys.stderr.write("orch notice: %s\n" % message)
        reply = notice_reply(message)
        if reply:
            text(sid, reply)
        return
    for step in script.get(message, [["text", "Noted."]]):
        if step[0] == "call":
            call(sid, step[1], step[2])
        else:
            text(sid, step[1])


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
        servers = p.get("mcpServers") or []
        if servers and mcp is None:
            mcp = Mcp(servers[0])
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
