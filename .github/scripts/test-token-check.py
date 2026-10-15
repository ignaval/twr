#!/usr/bin/env python3
"""Checks how a twr build treats the test user token (TWR_TEST_USER_TOKEN_FILE).

    python3 .github/scripts/test-token-check.py --twr <twr binary> --expect refuse|accept

It starts `<twr> hub` on a throwaway home (TWR_HOME, a new temporary directory) with
TWR_TEST_USER_TOKEN_FILE naming a 0600 file that holds a token, waits for the hub's socket,
then runs a raw protocol client with an agent marker in its environment (TWR_AGENT=x:y). The
client says `hello` with `user_token` set to that token, then asks for `hub.shutdown`, which
only the user may.

- `--expect refuse` (a release build, the one users install): the hub ignores the token, so
  the client stays an agent: `hello` answers `identity: agent:...` and `hub.shutdown` is
  `unauthorized`. The script then stops the hub with SIGTERM.
- `--expect accept` (a debug build, the one the tests run): the hub honours the token, the
  client is the user (`identity: user:...`) and the hub shuts down when asked.

Exit status 0 when the build behaves as expected. CI runs it with `--expect refuse` on the
x86_64 Linux release build (.github/workflows/ci.yml and release.yml); locally:

    cargo build --release -p twr-cli && python3 .github/scripts/test-token-check.py \
        --twr target/release/twr --expect refuse
    cargo build -p twr-cli && python3 .github/scripts/test-token-check.py \
        --twr target/debug/twr --expect accept

Standard library only.
"""

import argparse
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

TOKEN = "release-token-check-0123456789abcdef"
WAIT = 30.0


def fail(msg):
    print(f"test-token-check: {msg}", file=sys.stderr)
    sys.exit(1)


def call(conn, reader, rid, method, params):
    """Sends one request and returns its response (stream items, if any, are skipped)."""
    conn.sendall((json.dumps({"id": rid, "method": method, "params": params}) + "\n").encode())
    while True:
        line = reader.readline()
        if not line:
            fail(f"the hub closed the connection before answering {method}")
        msg = json.loads(line)
        if msg.get("id") == rid and "stream" not in msg:
            return msg


def client(sock_path, token, expect):
    """The client, run with TWR_AGENT in its environment (see main)."""
    if not os.environ.get("TWR_AGENT"):
        fail("the client must run with TWR_AGENT set")
    conn = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    conn.settimeout(WAIT)
    conn.connect(sock_path)
    reader = conn.makefile("r", encoding="utf-8")
    hello = call(conn, reader, 1, "hello",
                 {"client": "token-check", "protocol": 1, "user_token": token})
    print(f"hello: {json.dumps(hello)}")
    if "result" not in hello:
        fail(f"hello failed: {hello}")
    identity = hello["result"].get("identity", "")
    stop = call(conn, reader, 2, "hub.shutdown", None)
    print(f"hub.shutdown: {json.dumps(stop)}")
    if expect == "refuse":
        if not identity.startswith("agent:"):
            fail(f"the hub took the token: hello says identity {identity!r}, not an agent's")
        code = stop.get("error", {}).get("code")
        if code != "unauthorized":
            fail(f"hub.shutdown from an agent presenting the token was not refused as "
                 f"unauthorized: {stop}")
    else:
        if identity != "user:token-check":
            fail(f"the hub ignored the token: hello says identity {identity!r}")
        if stop.get("result") != {"ok": True}:
            fail(f"hub.shutdown from the token's holder failed: {stop}")
    print(f"ok: the hub {'refused' if expect == 'refuse' else 'accepted'} the token")


def wait_for_socket(path, hub):
    deadline = time.monotonic() + WAIT
    while time.monotonic() < deadline:
        if hub.poll() is not None:
            return f"the hub exited with {hub.returncode} before it listened"
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            probe.connect(path)
            return None
        except OSError:
            time.sleep(0.1)
        finally:
            probe.close()
    return f"no hub listened on {path} within {WAIT:.0f} s"


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--twr", help="the twr binary whose hub is checked")
    ap.add_argument("--expect", choices=["refuse", "accept"], required=True)
    ap.add_argument("--client", nargs=2, metavar=("SOCKET", "TOKEN"), help=argparse.SUPPRESS)
    args = ap.parse_args()
    if args.client:
        client(args.client[0], args.client[1], args.expect)
        return
    if not args.twr:
        fail("--twr is required")
    twr = os.path.abspath(args.twr)

    home = tempfile.mkdtemp(prefix="twr-token-")
    ok = False
    hub = None
    log_path = os.path.join(home, "hub-output.log")
    try:
        token_file = os.path.join(home, "user-token")
        fd = os.open(token_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as f:
            f.write(TOKEN + "\n")
        # Nothing of the caller's twr setup reaches the hub (a run from inside a twr thread
        # has TWR_AGENT, TWR_WORKSPACE, ...).
        env = {k: v for k, v in os.environ.items() if not k.startswith("TWR_")}
        env["TWR_HOME"] = os.path.join(home, "home")
        env["TWR_TEST_USER_TOKEN_FILE"] = token_file
        os.mkdir(env["TWR_HOME"], 0o700)
        sock_path = os.path.join(env["TWR_HOME"], "hub.sock")
        with open(log_path, "w") as log:
            hub = subprocess.Popen([twr, "hub"], env=env, stdin=subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT)
        why = wait_for_socket(sock_path, hub)
        if why:
            fail(why)
        client_env = dict(env, TWR_AGENT="x:y")
        done = subprocess.run(
            [sys.executable, "-I", os.path.abspath(__file__), "--expect", args.expect,
             "--client", sock_path, TOKEN],
            env=client_env, timeout=2 * WAIT)
        if done.returncode != 0:
            fail("the client's check failed")
        if args.expect == "accept":
            try:
                hub.wait(timeout=WAIT)
            except subprocess.TimeoutExpired:
                fail("the hub accepted hub.shutdown but did not exit")
        ok = True
    finally:
        if hub is not None and hub.poll() is None:
            hub.send_signal(signal.SIGTERM)
            try:
                hub.wait(timeout=WAIT)
            except subprocess.TimeoutExpired:
                hub.kill()
                hub.wait()
                print("test-token-check: the hub ignored SIGTERM; killed", file=sys.stderr)
                ok = False
        if not ok and os.path.exists(log_path):
            print("--- the hub's output ---", file=sys.stderr)
            with open(log_path, errors="replace") as f:
                sys.stderr.write(f.read())
        shutil.rmtree(home, ignore_errors=True)
    if not ok:
        sys.exit(1)


if __name__ == "__main__":
    main()
