#!/usr/bin/env python3
"""A scripted stand-in for `rpi --mode rpc` (v0.15.0 frame shapes, see rpc.rs).

State lives next to --session-dir (the zephyr home): log.jsonl records every command,
pid holds the current pid, and a `hang` file makes the next prompt commit its user
message, mark the run resumable, and stop answering (the test then kills -9 it); a
`slow` file holds the seconds each prompt takes. `usage.json` (if present) is the usage each
reply carries: {"totalTokens": n, "costTotal": x}, sent as message.usage.{totalTokens,cost.total}.
`get_stats` always reports 0, as rpi 0.15.0 does for replies.
Auth: the OAuth store is $RPI_CODING_AGENT_DIR/auth.json. `get_auth_status` lists its oauth
entries. `auth refresh --provider P` (the CLI, not the RPC) gives P a fresh expiry, and records
an "auth-refresh" line in ./log.jsonl (an `oauth_fail` file in the home makes it fail).
`auth login --provider P` records an "auth-login" line and stores {"type": "oauth"} for P: with a
`login_url` file in the home it prints a sign-in URL and waits for a `login_go` file (the browser
coming back), otherwise it prompts and reads one stdin line; a `login_fail` file makes it fail.
`auth check --json --provider P` prints {"ready": true} iff P is stored.
Accounts: each start logs {"type": "start", "agentDir", "model"} to ./log.jsonl. A
`rate_limited` file in $RPI_CODING_AGENT_DIR holds an error text: every prompt then ends with
that error (stopReason "error", settle outcome "failed"), as a rate-limited provider does.
A prompt containing WRITE_MEMORY first writes memory/MEMORY.md under the home.
argv.json holds the argv of the latest start.
A prompt containing `bash: CMD` first reports a bash tool call running CMD (tool_execution_start).
Each prompt and resume answers `accepted`, commits the user entry (`session_entry_committed`, as
0.15 does before `agent_start`), starts with `agent_start` and ends with `agent_end`, then
`agent_settled`. A failing turn (`rate_limited`) reports the assistant message (stopReason
"error") with `message_start`/`message_end`/`turn_end` first, and settles with outcome "failed"
and that `errorMessage` as `agent_settled.error`. A prompt containing `[tool]` first runs one fake
`bash` tool call (assistant toolCall, tool_execution_start, tool_execution_end, toolResult); while a
`tool_wait` file exists in the home, the tool keeps running. Each `[call NAME {JSON}]` in a prompt runs
one such call of tool NAME with those (flat) args, succeeding, before the reply.
`get_state` reports thinkingLevel "medium".
A prompt containing `[turns N]` reports N `turn_end` events, then waits for `abort` as `[hang]` does.
A prompt containing `[hang]` never settles until `abort`, which records an aborted assistant
message (stopReason "aborted", `errorMessage` "abort error: Request aborted") and settles with
outcome "aborted"; one that starts with `say: `
replies with the rest; `model?` replies with the current model.
Models: `--list-models` (held while a `list_wait` file exists in the home) prints fake/m1, fake/m2, other/m3 and opencode-go/alpha, the catalogue `set_model` accepts
(plus the started model);
`set_model` switches to one of them and errors on anything else.
The session is <session-dir>/<session-id>.json: {"messages": [...], "pending": bool}.
"""
import json, os, re, sys, time

args = sys.argv[1:]
if "--version" in args:
    print("rpi 0.15.0"); sys.exit(0)
CATALOG = ["fake/m1", "fake/m2", "other/m3", "opencode-go/alpha"]
if "--list-models" in args:
    while os.path.exists("list_wait"):  # cwd is the home
        time.sleep(0.05)
    print("provider  model  context")
    for m in CATALOG:
        print("%s  %s  1M" % tuple(m.split("/", 1)))
    sys.exit(0)
if args[:1] == ["auth"]:
    # The CLI half of auth: `refresh` (the keeper), `login` and `check` (crate::auth), in the home
    # as cwd. `login` is run under `script`, so its stdout is a pty.
    provider = args[args.index("--provider") + 1]
    home = os.getcwd()
    d = os.environ.get("RPI_CODING_AGENT_DIR")
    store_path = os.path.join(d, "auth.json") if d else None
    store = json.load(open(store_path)) if store_path and os.path.exists(store_path) else {}
    if args[1:2] == ["login"]:
        open(os.path.join(home, "log.jsonl"), "a").write(json.dumps({"type": "auth-login", "provider": provider}) + "\n")
        if os.path.exists(os.path.join(home, "login_fail")):
            print("error: sign-in failed", file=sys.stderr); sys.exit(1)
        if os.path.exists(os.path.join(home, "login_url")):
            # An OAuth provider: print the URL and finish when the browser does (a test drops
            # `login_go` in the home, as the browser coming back would).
            print("To sign in, open this URL in a browser:\r\n\r\n\x1b[36mhttps://example.test/login/%s\x1b[0m\r\n" % provider)
            sys.stdout.flush()
            while not os.path.exists(os.path.join(home, "login_go")):
                time.sleep(0.05)
        else:
            # A key or a setup token: paste it at the prompt.
            print("Paste the %s credential for \"%s\":" % (provider, provider)); sys.stdout.flush()
            if not sys.stdin.readline().strip():
                print("error: no credential given", file=sys.stderr); sys.exit(1)
        store[provider] = {"type": "oauth"}
        json.dump(store, open(store_path, "w"))
        print("Signed in to \"%s\"." % provider)
        sys.exit(0)
    if args[1:2] == ["check"]:
        # `rpi auth check --json --provider P`: is the credential stored?
        print(json.dumps({"provider": provider, "ready": provider in store}))
        sys.exit(0)
    # `rpi auth refresh --provider P`.
    open(os.path.join(home, "log.jsonl"), "a").write(json.dumps({"type": "auth-refresh", "provider": provider}) + "\n")
    if os.path.exists(os.path.join(home, "oauth_fail")) or store.get(provider, {}).get("type") != "oauth":
        print("error: OAuth refresh failed", file=sys.stderr); sys.exit(1)
    store[provider]["expires"] = int(time.time()) + 3600
    store[provider]["access"] = "fresh-" + str(os.getpid())
    json.dump(store, open(store_path, "w"))
    print("OAuth credentials refreshed for \"%s\"." % provider)
    sys.exit(0)
opt = lambda name: args[args.index(name) + 1] if name in args else None
session_dir = opt("--session-dir")
home = os.path.dirname(os.path.abspath(session_dir))
path = os.path.join(session_dir, opt("--session-id") + ".json")
extension = opt("--extension")
open(os.path.join(home, "pid"), "w").write(str(os.getpid()))
json.dump(args, open(os.path.join(home, "argv.json"), "w"))  # the flags this child was started with
open(os.path.join(home, "log.jsonl"), "a").write(json.dumps(
    {"type": "start", "agentDir": os.environ.get("RPI_CODING_AGENT_DIR"), "model": opt("--model"),
     "sessionId": opt("--session-id")}) + "\n")
if os.path.isdir(os.environ.get("RPI_CODING_AGENT_DIR") or ""):  # which account started, for tests whose home is temporary
    open(os.path.join(os.environ["RPI_CODING_AGENT_DIR"], "starts.log"), "a").write("%s\n" % opt("--model"))

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
    msg = {"kind": "assistant", "role": "assistant", "content": [{"type": "text", "text": text}], "stopReason": "stop", "timestamp": 0}
    p = os.path.join(home, "usage.json")
    if os.path.exists(p):
        u = json.load(open(p))
        msg["usage"] = {"totalTokens": u.get("totalTokens", 0), "cost": {"total": u.get("costTotal", 0.0)}}
    return msg

def auth_store():
    d = os.environ.get("RPI_CODING_AGENT_DIR")
    p = os.path.join(d, "auth.json") if d else None
    if not p or not os.path.exists(p): return {}
    return json.load(open(p))

def rate_limit():
    d = os.environ.get("RPI_CODING_AGENT_DIR")
    p = os.path.join(d, "rate_limited") if d else None
    return open(p).read() if p and os.path.exists(p) else None

def fail(s, error):
    msg = {"kind": "assistant", "role": "assistant", "content": [], "stopReason": "error",
           "errorMessage": error, "timestamp": 0}
    s["messages"].append(msg); s["pending"] = False; save(s)
    out({"type": "message_start", "message": msg})
    out({"type": "message_end", "message": msg})
    out({"type": "turn_end", "message": msg, "toolResultCount": 0})
    out({"type": "session_entry_committed", "entryId": "e", "entryType": "message", "customType": None, "lane": "main"})
    out({"type": "agent_end", "messageCount": 0, "messages": []})
    out({"type": "agent_settled", "runId": "r", "outcome": "failed", "aborted": False,
         "error": error, "summary": None, "pendingResume": False})

def finish(s, text, outcome="completed"):
    msg = assistant(text)
    s["messages"].append(msg); s["pending"] = False; save(s)
    out({"type": "message_start", "message": msg})
    out({"type": "message_end", "message": msg})
    out({"type": "agent_end", "messageCount": 0, "messages": []})
    out({"type": "agent_settled", "runId": "r", "outcome": outcome, "aborted": outcome == "aborted",
         "error": None, "summary": None, "pendingResume": False})

def run_tool(s, name="bash", args={"command": "true"}, cid="t1"):
    call = {"kind": "assistant", "role": "assistant", "stopReason": "toolUse", "timestamp": 0,
            "content": [{"type": "toolCall", "id": cid, "name": name, "arguments": args}]}
    result = {"kind": "toolResult", "role": "toolResult", "toolCallId": cid, "toolName": name,
              "content": [{"type": "text", "text": "ok"}], "isError": False, "timestamp": 0}
    s["messages"].append(call); save(s)
    out({"type": "message_start", "message": call}); out({"type": "message_end", "message": call})
    out({"type": "tool_execution_start", "toolCallId": cid, "toolName": name, "args": args})
    while os.path.exists(os.path.join(home, "tool_wait")):
        time.sleep(0.05)
    out({"type": "tool_execution_end", "toolCallId": cid, "toolName": name, "isError": False,
         "result": {"content": result["content"]}})
    s["messages"].append(result); save(s)
    out({"type": "message_start", "message": result}); out({"type": "message_end", "message": result})

out({"type": "ready", "protocol": "rpi", "piCompatibilityTarget": "1.2.0",
     "supportedCommands": ["get_state", "get_commands", "get_messages", "get_stats", "queue_message", "clear_queue",
                           "prompt", "resume_pending", "abort", "ping", "stop", "get_auth_status", "set_model"],
     "supportedEvents": ["session_entry_committed"],
     "semantics": {"promptResponse": "accepted", "settlementEvent": "agent_settled", "agentEndIsTerminal": False},
     "limits": {"frameBytes": 1048576, "maxConcurrentPrompts": 1}, "parityComplete": False})
queue = []
hanging = False
model = opt("--model")
catalog = set(CATALOG) | ({model} if model else set())
for line in sys.stdin:
    cmd = json.loads(line)
    open(os.path.join(home, "log.jsonl"), "a").write(line)
    kind, s = cmd["type"], load()
    if kind == "prompt":
        respond(cmd, {"accepted": True})
        s["messages"].append(user(cmd["message"])); save(s)
        if "WRITE_MEMORY" in cmd["message"]:
            open(os.path.join(home, "memory", "MEMORY.md"), "a").write("remembered: " + cmd["message"] + "\n")
        # As rpi 0.15: the prompt is committed (before agent_start), but no message events carry it.
        out({"type": "session_entry_committed", "entryId": "u", "entryType": "message", "customType": None, "lane": "main"})
        out({"type": "agent_start"})
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
                    respond(c, error="a run is already active"); continue
                respond(c, {"aborted": True}); s["pending"] = False
                # As rpi 0.15: a cancelled run still records an aborted assistant message.
                msg = {"kind": "assistant", "role": "assistant", "content": [], "stopReason": "aborted",
                       "errorMessage": "abort error: Request aborted", "timestamp": 0}
                s["messages"].append(msg); save(s)
                out({"type": "message_start", "message": msg})
                out({"type": "message_end", "message": msg})
                out({"type": "turn_end", "message": msg, "toolResultCount": 0})
                out({"type": "session_entry_committed", "entryId": "e", "entryType": "message", "customType": None, "lane": "main"})
                out({"type": "agent_end", "messageCount": 0, "messages": []})
                out({"type": "agent_settled", "runId": "r", "outcome": "aborted", "aborted": True,
                     "error": None, "summary": None, "pendingResume": False})
                break
            continue
        if rate_limit() is not None:
            fail(s, rate_limit()); continue
        turns = re.search(r"\[turns (\d+)\]", cmd["message"])
        if turns:
            for _ in range(int(turns.group(1))):
                out({"type": "turn_end"})
            hanging = True; continue
        if "[hang]" in cmd["message"]:
            hanging = True; continue
        if "bash: " in cmd["message"]:
            out({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash",
                 "args": {"command": cmd["message"].split("bash: ", 1)[1]}})
            finish(s, "ran"); continue
        if "[tool]" in cmd["message"]:
            run_tool(s)
        for i, (name, args) in enumerate(re.findall(r"\[call (\w+) (\{[^{}]*\})\]", cmd["message"])):
            run_tool(s, name, json.loads(args), "c%d" % i)
        if cmd["message"].endswith("model?"):
            finish(s, "model: %s" % model); continue
        if cmd["message"].startswith("say: "):
            finish(s, cmd["message"][5:]); continue
        finish(s, "echo: " + cmd["message"])
    elif kind == "resume_pending":
        if not s["pending"]:
            respond(cmd, {"resumed": False}); continue
        respond(cmd, {"accepted": True})
        out({"type": "agent_start"})
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
        respond(cmd, {"totalTokens": 0, "costTotal": 0.0})
    elif kind == "set_model":
        want = "%s/%s" % (cmd.get("provider"), cmd.get("modelId"))
        if want in catalog:
            model = want; respond(cmd, {"modelId": cmd.get("modelId"), "provider": cmd.get("provider")})
        else:
            respond(cmd, error="unknown model: %s" % cmd.get("modelId"))
    elif kind == "get_state":
        respond(cmd, {"sessionId": opt("--session-id"), "activeOperation": None, "thinkingLevel": "medium"})
    elif kind == "stop":
        respond(cmd, {"stopped": True}); break
    else:
        respond(cmd, error="unknown command type: " + kind)
