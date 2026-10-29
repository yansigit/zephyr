#!/usr/bin/env python3
"""A scripted stand-in for `rpi --mode rpc` (v0.12.0 frame shapes, see rpc.rs).

State lives next to --session-dir (the zephyr home): log.jsonl records every command,
pid holds the current pid, and a `hang` file makes the next prompt commit its user
message, mark the run resumable, and stop answering (the test then kills -9 it); a
`slow` file holds the seconds each prompt takes. `stats.json` (if present) is what `get_stats`
reports: {"totalTokens": n, "costTotal": x}, cumulative like rpi's.
Auth: the OAuth store is $RPI_CODING_AGENT_DIR/auth.json. `get_auth_status` lists its oauth
entries. `auth refresh --provider P` (the CLI, not the RPC) gives P a fresh expiry, and records
an "auth-refresh" line in ./log.jsonl (an `oauth_fail` file in the home makes it fail).
A prompt containing WRITE_MEMORY first writes memory/MEMORY.md under the home.
A prompt containing `[hang]` never settles until `abort`; one that starts with `say: `
replies with the rest.
The session is <session-dir>/<session-id>.json: {"messages": [...], "pending": bool}.
"""
import json, os, sys, time

args = sys.argv[1:]
if "--version" in args:
    print("rpi 0.12.0"); sys.exit(0)
if args[:1] == ["auth"]:
    # `rpi auth refresh --provider P`, run in the home as cwd.
    provider = args[args.index("--provider") + 1]
    home = os.getcwd()
    open(os.path.join(home, "log.jsonl"), "a").write(json.dumps({"type": "auth-refresh", "provider": provider}) + "\n")
    d = os.environ.get("RPI_CODING_AGENT_DIR")
    store = json.load(open(os.path.join(d, "auth.json"))) if d and os.path.exists(os.path.join(d, "auth.json")) else {}
    if os.path.exists(os.path.join(home, "oauth_fail")) or store.get(provider, {}).get("type") != "oauth":
        print("error: OAuth refresh failed", file=sys.stderr); sys.exit(1)
    store[provider]["expires"] = int(time.time()) + 3600
    store[provider]["access"] = "fresh-" + str(os.getpid())
    json.dump(store, open(os.path.join(d, "auth.json"), "w"))
    print("OAuth credentials refreshed for \"%s\"." % provider)
    sys.exit(0)
opt = lambda name: args[args.index(name) + 1] if name in args else None
session_dir = opt("--session-dir")
home = os.path.dirname(os.path.abspath(session_dir))
path = os.path.join(session_dir, opt("--session-id") + ".json")
extension = opt("--extension")
open(os.path.join(home, "pid"), "w").write(str(os.getpid()))

def load():
    try:
        return json.load(open(path))
    except FileNotFoundError:
        return {"messages": [], "pending": False}

def save(s):
    json.dump(s, open(path + ".tmp", "w")); os.replace(path + ".tmp", path)

def out(v):
    sys.stdout.write(json.dumps(v) + "\n"); sys.stdout.flush()

def respond(cmd, data=None, error=None):
    v = {"type": "response", "command": cmd["type"], "success": error is None,
         "status": "ok" if error is None else "error", "id": cmd.get("id")}
    if error is None: v["data"] = data; v["result"] = data
    else: v["error"] = error
    out(v)

def user(text): return {"kind": "user", "role": "user", "content": text, "timestamp": 0}
def assistant(text):
    return {"kind": "assistant", "role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop", "timestamp": 0}

def auth_store():
    d = os.environ.get("RPI_CODING_AGENT_DIR")
    p = os.path.join(d, "auth.json") if d else None
    if not p or not os.path.exists(p): return {}
    return json.load(open(p))

def finish(s, text, outcome="completed"):
    msg = assistant(text)
    s["messages"].append(msg); s["pending"] = False; save(s)
    out({"type": "message_start", "message": msg})
    out({"type": "message_end", "message": msg})
    out({"type": "agent_settled", "runId": "r", "outcome": outcome, "aborted": outcome == "aborted",
         "error": None, "summary": None, "pendingResume": False})

out({"type": "ready", "protocol": "rpi", "piCompatibilityTarget": "1.2.0",
     "supportedCommands": ["get_state", "get_commands", "get_messages", "get_stats", "queue_message", "clear_queue",
                           "prompt", "resume_pending", "abort", "ping", "stop", "get_auth_status"],
     "supportedEvents": ["session_entry_committed"],
     "semantics": {"promptResponse": "accepted", "settlementEvent": "agent_settled", "agentEndIsTerminal": False},
     "limits": {"frameBytes": 1048576, "maxConcurrentPrompts": 1}, "parityComplete": False})
queue = []
hanging = False
for line in sys.stdin:
    cmd = json.loads(line)
    open(os.path.join(home, "log.jsonl"), "a").write(line)
    kind, s = cmd["type"], load()
    if kind == "prompt":
        respond(cmd, {"accepted": True})
        s["messages"].append(user(cmd["message"])); save(s)
        if "WRITE_MEMORY" in cmd["message"]:
            open(os.path.join(home, "memory", "MEMORY.md"), "a").write("remembered: " + cmd["message"] + "\n")
        out({"type": "message_start", "message": user(cmd["message"])})
        out({"type": "message_end", "message": user(cmd["message"])})
        if os.path.exists(os.path.join(home, "slow")):
            time.sleep(float(open(os.path.join(home, "slow")).read() or 1))
        if os.path.exists(os.path.join(home, "hang")):
            os.remove(os.path.join(home, "hang"))
            s["pending"] = True; save(s)
            # Stay busy until aborted, then settle with no reply, as a cancelled run does.
            for line in sys.stdin:
                c = json.loads(line)
                open(os.path.join(home, "log.jsonl"), "a").write(line)
                if c["type"] != "abort":
                    respond(c, error="busy"); continue
                respond(c, {"aborted": True}); s["pending"] = False; save(s)
                out({"type": "agent_settled", "runId": "r", "outcome": "aborted", "aborted": True,
                     "error": None, "summary": None, "pendingResume": False})
                break
            continue
        if "[hang]" in cmd["message"]:
            hanging = True; continue
        if cmd["message"].startswith("say: "):
            finish(s, cmd["message"][5:]); continue
        finish(s, "echo: " + cmd["message"])
    elif kind == "resume_pending":
        if not s["pending"]:
            respond(cmd, {"resumed": False}); continue
        respond(cmd, {"accepted": True})
        last = [m for m in s["messages"] if m["kind"] == "user"][-1]
        finish(s, "resumed: " + last["content"])
    elif kind == "get_messages":
        respond(cmd, {"messages": s["messages"], "total": len(s["messages"]), "pendingResume": s["pending"]})
    elif kind == "get_commands":
        names = ["zephyr-policy"] if extension and os.path.exists(extension) else []
        respond(cmd, {"commands": [{"name": n, "description": ""} for n in names]})
    elif kind == "get_auth_status":
        store = auth_store()
        respond(cmd, {"providers": [{"provider": p, "ready": True, "source": "oauth", "status": "ready",
                                     "oauth": True, "oauthAvailable": True}
                                    for p, c in store.items() if c.get("type") == "oauth"]})
    elif kind == "queue_message":
        queue.append(cmd["message"]); respond(cmd, {"accepted": True, "entryId": "q", "queue": "followUp"})
    elif kind == "clear_queue":
        respond(cmd, {"cleared": len(queue), "steering": [], "followUp": queue}); queue = []
    elif kind == "abort":
        respond(cmd, {"aborted": True})
        if hanging:
            hanging = False; finish(s, "", "aborted")
    elif kind == "get_stats":
        stats = {"totalTokens": 0, "costTotal": 0.0}
        if os.path.exists(os.path.join(home, "stats.json")):
            stats.update(json.load(open(os.path.join(home, "stats.json"))))
        respond(cmd, stats)
    elif kind == "get_state":
        respond(cmd, {"sessionId": opt("--session-id"), "activeOperation": None})
    elif kind == "stop":
        respond(cmd, {"stopped": True}); break
    else:
        respond(cmd, error="unknown command type: " + kind)
