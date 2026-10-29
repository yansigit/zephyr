#!/usr/bin/env python3
"""A scripted stand-in for `rpi --mode rpc` (v0.12.0 frame shapes, see rpc.rs).

State lives next to --session-dir (the zephyr home): log.jsonl records every command,
pid holds the current pid, and a `hang` file makes the next prompt commit its user
message, mark the run resumable, and stop answering (the test then kills -9 it).
The session is <session-dir>/<session-id>.json: {"messages": [...], "pending": bool}.
"""
import json, os, sys, time

args = sys.argv[1:]
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

def finish(s, text):
    msg = assistant(text)
    s["messages"].append(msg); s["pending"] = False; save(s)
    out({"type": "message_end", "message": msg})
    out({"type": "agent_settled", "runId": "r", "outcome": "completed", "aborted": False,
         "error": None, "summary": None, "pendingResume": False})

out({"type": "ready", "protocol": "rpi", "piCompatibilityTarget": "1.2.0",
     "supportedCommands": ["get_state", "get_commands", "get_messages", "queue_message", "clear_queue",
                           "prompt", "resume_pending", "abort", "ping", "stop"],
     "supportedEvents": ["session_entry_committed"],
     "semantics": {"promptResponse": "accepted", "settlementEvent": "agent_settled", "agentEndIsTerminal": False},
     "limits": {"frameBytes": 1048576, "maxConcurrentPrompts": 1}, "parityComplete": False})
queue = []
for line in sys.stdin:
    cmd = json.loads(line)
    open(os.path.join(home, "log.jsonl"), "a").write(line)
    kind, s = cmd["type"], load()
    if kind == "prompt":
        respond(cmd, {"accepted": True})
        s["messages"].append(user(cmd["message"])); save(s)
        if os.path.exists(os.path.join(home, "hang")):
            os.remove(os.path.join(home, "hang"))
            s["pending"] = True; save(s)
            time.sleep(3600)
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
    elif kind == "queue_message":
        queue.append(cmd["message"]); respond(cmd, {"accepted": True, "entryId": "q", "queue": "followUp"})
    elif kind == "clear_queue":
        respond(cmd, {"cleared": len(queue), "steering": [], "followUp": queue}); queue = []
    elif kind == "abort":
        respond(cmd, {"aborted": True})
    elif kind == "get_state":
        respond(cmd, {"sessionId": opt("--session-id"), "activeOperation": None})
    elif kind == "stop":
        respond(cmd, {"stopped": True}); break
    else:
        respond(cmd, error="unknown command type: " + kind)
