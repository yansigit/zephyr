#!/usr/bin/env python3
"""A scripted stand-in for `signal-cli --data-dir D -a ACCOUNT jsonRpc` (0.14.10 shapes, see signal.rs).

State lives in D, the --data-dir: log.jsonl gets one line per start,
{"type": "start", "argv": [...], "env": [the keys of the environment it started with, sorted]}, and one per request
line read on stdin, {"type": "request", "request": {...}}. Lines a test appends to feed.jsonl are
envelopes, each sent on stdout as {"jsonrpc": "2.0", "method": "receive", "params": {"envelope",
"account"}} (polled every 50 ms, from the start of the file, so a restarted child replays nothing
it sent before: the read offset is kept in feed.offset). Answers: `listAccounts`
[{"number": ACCOUNT}]; `send` {"timestamp", "results": [one per recipient]}, each of type
"SUCCESS", or "IDENTITY_FAILURE" while an `untrusted` file exists in D; `sendTyping` and `trust` {};
in multi-account mode (no -a), `register` and `verify` {} when they name an "account"; anything else
a JSON-RPC error -32601. An `exit` file in D makes it exit 0 (the file removed). A `slow_send`
file holds the seconds each `send` waits before it is answered (it is logged at once).
"""

import json
import os
import select
import sys
import time

argv = sys.argv[1:]
data = argv[argv.index("--data-dir") + 1]
account = argv[argv.index("-a") + 1] if "-a" in argv else None


def path(name):
    return os.path.join(data, name)


def log(entry):
    with open(path("log.jsonl"), "a") as f:
        f.write(json.dumps(entry) + "\n")


def out(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def answer(req):
    method, params = req.get("method"), req.get("params") or {}
    if method == "listAccounts":
        return {"result": [{"number": account}]}
    if method == "send":
        if os.path.exists(path("slow_send")):
            time.sleep(float(open(path("slow_send")).read() or 1))
        kind = "IDENTITY_FAILURE" if os.path.exists(path("untrusted")) else "SUCCESS"
        results = [
            {"recipientAddress": {"uuid": r, "number": None}, "type": kind}
            for r in params.get("recipient", [])
        ]
        return {"result": {"timestamp": int(time.time() * 1000), "results": results}}
    if method in ("sendTyping", "trust"):
        return {"result": {}}
    if method in ("register", "verify") and account is None and params.get("account"):
        return {"result": {}}
    return {"error": {"code": -32601, "message": f"Method not implemented: {method}"}}


def feed():
    try:
        offset = int(open(path("feed.offset")).read())
    except (OSError, ValueError):
        offset = 0
    try:
        with open(path("feed.jsonl"), "rb") as f:
            f.seek(offset)
            text = f.read()
    except OSError:
        return
    complete = text[: text.rfind(b"\n") + 1]
    for line in complete.decode().splitlines():
        if line.strip():
            out({"jsonrpc": "2.0", "method": "receive",
                 "params": {"envelope": json.loads(line), "account": account}})
    if complete:
        with open(path("feed.offset"), "w") as f:
            f.write(str(offset + len(complete)))


# The environment it was started with: Python's own locale coercion (PEP 538) adds LC_CTYPE to
# os.environ, never to /proc/self/environ.
env = open("/proc/self/environ", "rb").read().split(b"\0")
log({"type": "start", "argv": argv,
     "env": sorted(e.split(b"=", 1)[0].decode() for e in env if e)})
buf = b""
while True:
    if os.path.exists(path("exit")):
        os.remove(path("exit"))
        sys.exit(0)
    feed()
    ready, _, _ = select.select([0], [], [], 0.05)
    if not ready:
        continue
    chunk = os.read(0, 65536)
    if not chunk:
        sys.exit(0)
    buf += chunk
    while b"\n" in buf:
        line, buf = buf.split(b"\n", 1)
        if not line.strip():
            continue
        req = json.loads(line)
        log({"type": "request", "request": req})
        if "id" in req:
            out({"jsonrpc": "2.0", "id": req["id"], **answer(req)})
