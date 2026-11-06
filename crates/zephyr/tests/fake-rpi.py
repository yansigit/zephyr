#!/usr/bin/env python3
"""A scripted stand-in for `rpi --mode rpc` (v0.18.0 frame shapes, see rpc.rs).

State lives next to --session-dir (the zephyr home): log.jsonl records every command,
pid holds the current pid, and a `hang` file makes the next prompt commit its user
message, mark the run resumable, and never finish it, answering only what needs no run (the
test then kills -9 it, or aborts it); a `wedge`
file does the same but never takes another command, `abort` included (a wedged rpi); a
`slow` file holds the seconds each prompt takes. `usage.json` (if present) is the usage each
reply carries: {"totalTokens": n, "costTotal": x}, sent as message.usage.{totalTokens,cost.total}.
`get_stats` always reports 0, as rpi does for replies.
Auth: the OAuth store is $RPI_CODING_AGENT_DIR/auth.json. `get_auth_status` lists its oauth
entries. `auth refresh --provider P` (the CLI, not the RPC) gives P a fresh expiry, and records
an "auth-refresh" line in ./log.jsonl (an `oauth_fail` file in the home makes it fail; a
`refresh_hang` file makes it write its pid to `refresh_pid` in the home and hang).
`auth login --provider P` records an "auth-login" line and stores {"type": "oauth"} for P: with a
`login_url` file in the home it prints a sign-in URL and waits for a `login_go` file (the browser
coming back), otherwise it prompts and reads one stdin line; a `login_fail` file makes it fail.
`auth check --json --provider P` prints {"ready": true} iff P is stored.
Accounts: each start logs {"type": "start", "agentDir", "model"} to ./log.jsonl. A
`rate_limited` file in $RPI_CODING_AGENT_DIR holds an error text: every prompt then ends with
that error (stopReason "error", settle outcome "failed"), as a rate-limited provider does.
A prompt containing WRITE_MEMORY first writes memory/MEMORY.md under the home.
argv.json holds the argv of the latest start. With a `tool_sleep` file in the home, each start
leaves a `sleep 300` running in its process group, as a tool would, its pid in `tool_pid`.
A prompt containing `bash: CMD` first reports a bash tool call running CMD (tool_execution_start).
With `--no-tools` (a /btw child) none of the tool markers below (`bash: `, `[tool]`, `[call ...]`)
runs anything, so its prompt is echoed whole; with `--no-session` there is no session file (the
session lives in memory), the home is the cwd, and `pid` is left to main's child. The start line in
log.jsonl records `noSession` and `noTools`.
Each prompt and resume answers `accepted`, commits the user entry (`session_entry_committed`, as
rpi does before `agent_start`), starts with `agent_start` and ends with `agent_end`, then
`agent_settled`. A failing turn (`rate_limited`) reports the assistant message (stopReason
"error") with `message_start`/`message_end`/`turn_end` first, and settles with outcome "failed"
and that `errorMessage` as `agent_settled.error`. A prompt containing `[tool]` first runs one fake
`bash` tool call (assistant toolCall, tool_execution_start, tool_execution_end, toolResult); while a
`tool_wait` file exists in the home, the tool keeps running, and (as rpi does) the commands that need
no run (`get_messages`, `queue_message`, ...) are answered meanwhile, the rest after. Each `[call NAME {JSON}]` in a prompt runs
one such call of tool NAME with those (flat) args, succeeding, before the reply.
A `gateway` call plays the policy plugin: it waits up to 5 s for the daemon's answer in
$ZEPHYR_GATEWAY_DIR/<call id>, consumes it, makes it the tool result, and the reply gets a
`gateway: <answer>` line per call. It classifies as `zephyr_policy::tools::gateway_kind` does: a
read (GATEWAY_COMMANDS) and a proposable action (/model P/M, /new, /switch ID, /pause, /reload)
both wait for the daemon; anything else is refused at once
("tool error: ... is not available to the agent"), with no wait. While it waits it answers the commands that need no run
(as rpi does) and keeps the rest for after; an `abort` ends it with "Operation aborted", and the
run settles "aborted" with no reply. A `task` call waits for its answer the same way (reply line
`task: <answer>`); one the plugin would refuse (an action other than start with a goal, list, or
stop with an id) gets "tool error: bad task call" at once. `exec` and `process` calls wait the same
way (up to 30 s for `exec`, whose answer comes when its command ends or yields), and so do `vault`
calls; an `exec` with an empty command gets "tool error: `command` is empty" at once.
`queue_message` with `deliverAs: "steer"` is held (answering `queue: "steer"`) and delivered, as rpi
does, after the running tool's result: `turn_start`, the user message's `message_start`/
`message_end` (role and kind `user`), then the fake's answer `echo: <text>` (none when the text
holds `[quiet]`), before the run carries on to its own reply. A prompt holding `[stop-after-steer]`
ends right after answering a steer, with no text of its own (as a model that only answers the
steered message does). One holding `[pend-after-steer]` stops right after answering a steer, its
run left pending (as a crash would leave it) until `resume_pending`. The router's continuation of such a request (its text asks for exactly
NO_REPLY when finished) is answered `NO_REPLY`, unless the request holds `[unfinished]`. One sent while idle waits for the
next prompt. A plain `queue_message` is a follow-up: never run, only cleared by `clear_queue`, which
empties both, or by `abort`, which (as rpi 0.18 does, running or idle) takes both queues back and
answers them as `steer`/`followUp` lists of {text, images}. `agent_settled` carries `reclaimed`:
null, or empty lists for a run the abort ended. Message events carry `messageId`, the message's
`entryId`, and `turn_end` carries `pendingMessages`, the queued count.
`get_state` reports `context.window` 200000 and the thinking level: "medium" until
`set_thinking_level` sets one of `get_available_thinking_levels` (off..high, or xhigh); any other
level is refused "invalid command payload", as rpi does.
A prompt containing `[turns N]` reports N `turn_end` events, then waits for `abort` as `[hang]` does.
A prompt containing `[hang-pending]` hangs as `[hang]` does and marks the session resumable, as a
run cut mid-flight leaves it (`resume_pending` then answers `resumed: <last user text>`; while a
`resume_wait` file exists it first stays open, taking commands, so a steer sent then is held).
A prompt containing `[progress]` first writes an assistant message `progress: halfway`, so a run
cut later has partial work to report.
A prompt containing `[hang]` never settles until `abort`, which records an aborted assistant
message (stopReason "aborted", `errorMessage` "abort error: Request aborted") and settles with
outcome "aborted"; one that starts with `say: `
replies with the rest; `model?` replies with the current model.
Models: `--list-models` (held while a `list_wait` file exists in the home) prints fake/m1, fake/m2, other/m3 and opencode-go/alpha, the catalogue `set_model` accepts
(plus the started model);
`set_model` switches to one of them and errors on anything else.
Sessions are laid out as rpi 0.18 lays them out (checked against the real binary): at start,
<session-dir>/--<cwd, / as ->--/<ISO stamp>_<session-id>.jsonl with a header line
{"kind": "header", "version": 4, "id", ...}, an empty `.lock` and a `.progress.jsonl` companion
({"session_id", ...}); a later start reopens the newest file for that id, and exits with an error
unless its header and companion name the id (as `/fork`'s copy must). Each message is appended as
{"kind": "entry", "type": "message", "id", "parentId", "message"}, `navigate` appends
{"kind": "lane", "leafId"}, a compaction an entry of type "compaction". Entry ids are `e<n>`. A
run is resumable while an `operation_started` record has no `operation_finished` after it; unlike
rpi, which starts one for every run, the fake writes one only for the markers that say so (`hang`,
`wedge`, `[hang-pending]`, `[pend-after-steer]`), so a run killed otherwise is not resumed.
`get_messages` gives each message on the path to the leaf its `entryId`.
`navigate {targetId}` (idle) puts the leaf on that entry, dropping everything after it from the
context, and answers {"outcome": "completed", "leafId": ...}; with no `targetId` it empties the
context (the root).
`compact` (idle, optional `customInstructions`) answers `accepted`, replaces the messages with
one `compactionSummary`, and settles with outcome "compacted" and a `summary`, as rpi does.
One run at a time (`maxConcurrentPrompts: 1`): from an accepted `prompt` or `resume_pending` to its
settle, a `prompt` (whatever its `streamingBehavior`), `compact` or `resume_pending` is refused with
rpi's error "a run is already active", and `navigate` with "cannot navigate while an operation is
active".
Not modelled: `--extension` is only checked to exist (for the policy beacon in `get_commands`),
never loaded, so no policy veto runs; `--exclude-tools` is recorded in argv.json but ignored.
While a `tool_fail` file exists in the home, every tool call other than gateway/task/exec/process/vault
ends with `isError` and the error `tool error: <the file's text>`.
"""
import glob, json, os, queue as queues, re, sys, threading, time

args = sys.argv[1:]
if "--version" in args:
    print("rpi 0.18.0"); sys.exit(0)
CATALOG = ["fake/m1", "fake/m2", "other/m3", "opencode-go/alpha"]
# zephyr_policy::tools::GATEWAY_COMMANDS
GATEWAY_COMMANDS = ["/status", "/model", "/switch", "/rules", "/memory diff", "/memory log", "/job list"]
def gateway_kind(c):
    """zephyr_policy::tools::gateway_kind: "read", "propose" or None."""
    if c in GATEWAY_COMMANDS:
        return "read"
    cmd, _, arg = c.partition(" ")
    word = arg != "" and all(33 <= ord(ch) <= 126 for ch in arg)
    ok = {"/new": c == cmd, "/pause": c == cmd, "/reload": c == cmd, "/switch": word,
          "/model": word and re.fullmatch(r"[^/]+/.+", arg) is not None}.get(cmd, False)
    return "propose" if ok else None
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
    if os.path.exists(os.path.join(home, "refresh_hang")):
        open(os.path.join(home, "refresh_pid"), "w").write(str(os.getpid()))
        time.sleep(300)
    if os.path.exists(os.path.join(home, "oauth_fail")) or store.get(provider, {}).get("type") != "oauth":
        print("error: OAuth refresh failed", file=sys.stderr); sys.exit(1)
    store[provider]["expires"] = int(time.time()) + 3600
    store[provider]["access"] = "fresh-" + str(os.getpid())
    json.dump(store, open(store_path, "w"))
    print("OAuth credentials refreshed for \"%s\"." % provider)
    sys.exit(0)
opt = lambda name: args[args.index(name) + 1] if name in args else None
session_dir = opt("--session-dir")
ephemeral = "--no-session" in args  # a /btw child: no session file, and the cwd is the home
tools = "--no-tools" not in args
home = os.getcwd() if ephemeral else os.path.dirname(os.path.abspath(session_dir))
memory = []  # the session's lines while ephemeral
extension = opt("--extension")
if not ephemeral:  # `pid` is main's, for the tests that kill it
    open(os.path.join(home, "pid"), "w").write(str(os.getpid()))
json.dump(args, open(os.path.join(home, "argv.json"), "w"))  # the flags this child was started with
open(os.path.join(home, "log.jsonl"), "a").write(json.dumps(
    {"type": "start", "agentDir": os.environ.get("RPI_CODING_AGENT_DIR"), "model": opt("--model"),
     "sessionId": opt("--session-id"), "noSession": ephemeral, "noTools": not tools}) + "\n")
if os.path.exists(os.path.join(home, "tool_sleep")):  # a tool left running in the child's group
    import subprocess
    open(os.path.join(home, "tool_pid"), "w").write(str(subprocess.Popen(["sleep", "300"]).pid))
if os.path.isdir(os.environ.get("RPI_CODING_AGENT_DIR") or ""):  # which account started, for tests whose home is temporary
    open(os.path.join(os.environ["RPI_CODING_AGENT_DIR"], "starts.log"), "a").write("%s\n" % opt("--model"))

def session_file():
    """As rpi 0.18: <session-dir>/--<cwd>--/<ISO stamp>_<id>.jsonl, made at start with its header
    line, an empty `.lock` and a `.progress.jsonl` companion; an existing one for the id is reopened,
    and refused unless the header and the companion name that id."""
    sid = opt("--session-id")
    folder = os.path.join(session_dir, "--%s--" % os.getcwd().strip("/").replace("/", "-"))
    found = sorted(glob.glob(os.path.join(glob.escape(folder), "*_%s.jsonl" % sid)))
    if found:
        head = json.loads(open(found[-1]).readline())
        progress = json.loads(open(found[-1] + ".progress.jsonl").readline())
        if head.get("id") != sid or progress.get("session_id") != sid:
            print("error: session file %s is not session %s" % (found[-1], sid), file=sys.stderr); sys.exit(1)
        return found[-1]
    os.makedirs(folder, exist_ok=True)
    ms = int(time.time() * 1000)
    stamp = time.strftime("%Y-%m-%dT%H-%M-%S", time.gmtime(ms // 1000)) + "-%03dZ" % (ms % 1000)
    p = os.path.join(folder, "%s_%s.jsonl" % (stamp, sid))
    open(p + ".lock", "w").close()
    open(p + ".progress.jsonl", "w").write(json.dumps(
        {"format": "assistant-progress-v1", "session_id": sid, "state": {"sequence": 0, "frames": []}}) + "\n")
    open(p, "w").write(json.dumps({"kind": "header", "version": 4, "id": sid, "createdAt": ms, "cwd": os.getcwd(),
                                   "progressFormat": "assistant-progress-v1"}) + "\n")
    return p

def session_lines():
    """The session's lines after the header."""
    return memory if ephemeral else open(path).read().splitlines()[1:]

def append(v):
    """Appends one line to the session, numbered as rpi numbers them (`seq`)."""
    v["seq"] = len(session_lines()) + 1
    if ephemeral:
        memory.append(json.dumps(v))
    else:
        open(path, "a").write(json.dumps(v) + "\n")

def load():
    """The session as {"messages", "pending", "leaf"}: the messages on the path to the leaf (a
    `lane` line moves it, as `navigate` does; a compaction restarts the context), and whether the
    last operation was left unfinished (`resume_pending` then resumes it)."""
    entries, leaf, pending = {}, None, False
    for line in session_lines():
        v = json.loads(line)
        if v["kind"] == "entry":
            entries[v["id"]] = v; leaf = v["id"]
        elif v["kind"] == "lane":
            leaf = v["leafId"]
        elif v["kind"] == "record" and v["type"] in ("operation_started", "operation_finished"):
            pending = v["type"] == "operation_started"
    chain, at = [], leaf
    while at is not None:
        chain.append(entries[at]); at = entries[at]["parentId"]
    messages = []
    for e in reversed(chain):
        if e["type"] == "compaction":
            messages = [{"kind": "compactionSummary", "role": "compactionSummary", "timestamp": 0, "entryId": e["id"],
                         "content": [{"type": "text", "text": e["summary"]}]}]
        else:
            messages.append(dict(e["message"], entryId=e["id"]))
    return {"messages": messages, "pending": pending, "leaf": leaf}

def add(s, m, kind="message", **fields):
    """Commits message `m` (or, with no message, a `compaction`) as an entry after the leaf;
    returns its id."""
    eid = "e%d" % (len(session_lines()) + 1)
    entry = {"kind": "entry", "lane": "main", "type": kind, "id": eid, "parentId": s["leaf"],
             "timestamp": int(time.time() * 1000)}
    entry.update(fields if m is None else {"message": m})
    append(entry)
    s["leaf"] = eid
    if m is not None:
        s["messages"].append(dict(m, entryId=eid))
    return eid

def set_pending(s, on):
    """Marks the run resumable (an operation started, not finished), or settled."""
    if s["pending"] != on:
        append({"kind": "record", "type": "operation_started" if on else "operation_finished", "id": "op",
                "lane": "main", "timestamp": int(time.time() * 1000)})
        s["pending"] = on

path = None if ephemeral else session_file()

def out(v):
    sys.stdout.write(json.dumps(v) + "\n"); sys.stdout.flush()

def respond(cmd, data=None, error=None):
    v = {"type": "response", "command": cmd["type"], "success": error is None,
         "status": "ok" if error is None else "error", "id": cmd.get("id")}
    if error is None: v["data"] = data; v["result"] = data
    else: v["error"] = error
    out(v)

def said(s, m):
    """Records message `m` in the session and reports it, its `messageId` its entry id (as 0.18)."""
    mid = add(s, m)
    out({"type": "message_start", "messageId": mid, "message": m})
    out({"type": "message_end", "messageId": mid, "message": m})

def turn_end(msg=None):
    v = {"type": "turn_end", "pendingMessages": len(queue) + len(steering)}
    if msg is not None:
        v.update(message=msg, toolResultCount=0)
    out(v)

def settled(outcome, error=None, summary=None):
    global running
    aborted, running = outcome == "aborted", False
    out({"type": "agent_settled", "runId": "r", "outcome": outcome, "aborted": aborted, "error": error,
         "summary": summary, "pendingResume": False,
         "reclaimed": {"steer": [], "followUp": []} if aborted else None})

def abort_reply(c):
    """As rpi 0.18: an abort takes both queues back and answers them."""
    global queue, steering
    back = lambda q: [{"text": t, "images": []} for t in q]
    respond(c, {"aborted": True, "steer": back(steering), "followUp": back(queue)})
    queue, steering = [], []

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
    set_pending(s, False); said(s, msg)
    turn_end(msg)
    out({"type": "session_entry_committed", "entryId": "e", "entryType": "message", "customType": None, "lane": "main"})
    out({"type": "agent_end", "messageCount": 0, "messages": []})
    settled("failed", error)

def finish(s, text, outcome="completed"):
    msg = assistant(text)
    set_pending(s, False); said(s, msg)
    out({"type": "agent_end", "messageCount": 0, "messages": []})
    settled(outcome)

def refused(cmd):
    """As rpi (`maxConcurrentPrompts: 1`): while a run is active, another run is refused, a
    `prompt` whatever its `streamingBehavior` included."""
    if running and cmd["type"] in ("prompt", "compact", "resume_pending"):
        respond(cmd, error="a run is already active")
    elif running and cmd["type"] == "navigate":
        respond(cmd, error="cannot navigate while an operation is active")
    else:
        return False
    return True

def quick(cmd, s):
    """Answers a command that needs no run (also while a gateway call waits); False otherwise."""
    global queue, steering, model, thinking
    kind = cmd["type"]
    if refused(cmd):
        pass
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
    elif kind == "queue_message" and cmd.get("deliverAs") == "steer":
        steering.append(cmd["message"]); respond(cmd, {"accepted": True, "entryId": "q", "queue": "steer"})
    elif kind == "queue_message":
        queue.append(cmd["message"]); respond(cmd, {"accepted": True, "entryId": "q", "queue": "followUp"})
    elif kind == "clear_queue":
        respond(cmd, {"cleared": len(queue) + len(steering), "steering": steering, "followUp": queue})
        queue, steering = [], []
    elif kind == "get_stats":
        respond(cmd, {"totalTokens": 0, "costTotal": 0.0})
    elif kind == "set_model":
        want = "%s/%s" % (cmd.get("provider"), cmd.get("modelId"))
        if want in catalog:
            model = want; respond(cmd, {"modelId": cmd.get("modelId"), "provider": cmd.get("provider")})
        else:
            respond(cmd, error="unknown model: %s" % cmd.get("modelId"))
    elif kind == "get_state":
        respond(cmd, {"sessionId": opt("--session-id"), "activeOperation": None, "thinkingLevel": thinking,
                      "context": {"window": 200000}})
    elif kind == "get_available_thinking_levels":
        respond(cmd, {"levels": LEVELS})
    elif kind == "set_thinking_level" and cmd.get("level") in LEVELS + ["xhigh"]:
        thinking = cmd["level"]; respond(cmd, {"thinkingLevel": thinking})
    elif kind == "set_thinking_level":
        respond(cmd, error="invalid command payload")
    else:
        return False
    return True

def deliver_steering(s):
    """As rpi: each steered message becomes a user message of its own at the next turn, and the
    fake answers it `echo: <text>` (unless it holds `[quiet]`) before the run carries on."""
    global steering, steered_this_run
    steered_this_run = steered_this_run or bool(steering)
    for text in steering:
        out({"type": "turn_start"})
        for m in [user(text)] + ([] if "[quiet]" in text else [assistant("echo: " + text)]):
            said(s, m)
    steering = []

def run_tool(s, name="bash", args={"command": "true"}, cid="t1"):
    call = {"kind": "assistant", "role": "assistant", "stopReason": "toolUse", "timestamp": 0,
            "content": [{"type": "toolCall", "id": cid, "name": name, "arguments": args}]}
    result = {"kind": "toolResult", "role": "toolResult", "toolCallId": cid, "toolName": name,
              "content": [{"type": "text", "text": "ok"}], "isError": False, "timestamp": 0}
    said(s, call)
    out({"type": "tool_execution_start", "toolCallId": cid, "toolName": name, "args": args})
    while os.path.exists(os.path.join(home, "tool_wait")):
        # As rpi: commands that need no run are answered while the tool runs; the rest wait.
        try:
            line = lines.get(timeout=0.05)
        except queues.Empty:
            continue
        if line is None:
            lines.put(None); time.sleep(0.05); continue
        open(os.path.join(home, "log.jsonl"), "a").write(line)
        if not quick(json.loads(line), s):
            deferred.append(line)
    aborted = False
    if name == "gateway" and gateway_kind(args.get("command", "").strip()) is None:
        # As the plugin: refused in the child, with no wait (rpi still reports the start).
        result["isError"] = True
        result["content"] = [{"type": "text", "text": "tool error: %s is not available to the agent"
                              % args.get("command", "").strip()}]
    elif name == "task" and not ((args.get("action") == "start" and str(args.get("goal", "")).strip())
                                 or args.get("action") == "list"
                                 or (args.get("action") == "stop" and isinstance(args.get("id"), int))):
        result["isError"] = True
        result["content"] = [{"type": "text", "text": "tool error: bad task call"}]
    elif name == "exec" and not str(args.get("command", "")).strip():
        result["isError"] = True
        result["content"] = [{"type": "text", "text": "tool error: `command` is empty"}]
    elif os.path.exists(os.path.join(home, "tool_fail")) and name not in ("gateway", "task", "exec", "process", "vault"):
        result["isError"] = True
        result["content"] = [{"type": "text", "text": "tool error: " + open(os.path.join(home, "tool_fail")).read()}]
    elif name in ("gateway", "task", "exec", "process", "vault"):
        answer = os.path.join(os.environ.get("ZEPHYR_GATEWAY_DIR", ""), cid)
        text = "the gateway did not answer"
        # An exec answers when its command ends or yields, so it may take a while.
        deadline = time.time() + (30 if name == "exec" else 5)
        while time.time() < deadline:
            if os.path.exists(answer):
                text = open(answer).read(); os.remove(answer); break
            # As rpi: commands keep being answered while the call waits, and an abort ends it.
            try:
                line = lines.get(timeout=0.05)
            except queues.Empty:
                continue
            c = json.loads(line)
            open(os.path.join(home, "log.jsonl"), "a").write(line)
            if c["type"] == "abort":
                abort_reply(c); text = "Operation aborted"; result["isError"] = aborted = True; break
            if not quick(c, s):
                deferred.append(line)
        result["content"] = [{"type": "text", "text": text}]
    out({"type": "tool_execution_end", "toolCallId": cid, "toolName": name, "isError": result["isError"],
         "result": {"content": result["content"]}})
    said(s, result)
    if not aborted:
        deliver_steering(s)
    return None if aborted else result["content"][0]["text"]

out({"type": "ready", "protocol": "rpi", "piCompatibilityTarget": "1.2.0",
     "supportedCommands": ["get_state", "get_commands", "get_messages", "get_stats", "queue_message", "clear_queue",
                           "prompt", "resume_pending", "abort", "ping", "stop", "get_auth_status", "set_model",
                           "compact", "navigate", "get_available_thinking_levels", "set_thinking_level"],
     "supportedEvents": ["session_entry_committed", "external_tool_request", "external_tool_request_closed",
                         "file_io_request", "file_io_request_closed"],
     "semantics": {"promptResponse": "accepted", "settlementEvent": "agent_settled", "agentEndIsTerminal": False,
                   "messageIds": "sessionEntryId-v1"},
     "limits": {"frameBytes": 1048576, "maxConcurrentPrompts": 1}, "parityComplete": False})
queue = []
steering = []  # queue_message with deliverAs "steer": delivered after the running tool
steered_this_run = False
hanging = False
running = False  # a run is active: from an accepted prompt or resume to its settle
calls = 0
model = opt("--model")
LEVELS = ["off", "minimal", "low", "medium", "high"]
thinking = "medium"
catalog = set(CATALOG) | ({model} if model else set())
lines = queues.Queue()  # stdin, read on a thread; None at EOF
deferred = []  # commands a waiting gateway call left for the main loop
def read_stdin():
    for l in sys.stdin:
        lines.put(l)
    lines.put(None)
threading.Thread(target=read_stdin, daemon=True).start()
def next_line():
    return deferred.pop(0) if deferred else lines.get()
for line in iter(next_line, None):
    cmd = json.loads(line)
    open(os.path.join(home, "log.jsonl"), "a").write(line)
    kind, s = cmd["type"], load()
    if refused(cmd):
        continue
    if kind == "prompt":
        respond(cmd, {"accepted": True}); running = True
        add(s, user(cmd["message"]))
        if "WRITE_MEMORY" in cmd["message"]:
            open(os.path.join(home, "memory", "MEMORY.md"), "a").write("remembered: " + cmd["message"] + "\n")
        # As rpi: the prompt is committed (before agent_start), but no message events carry it.
        out({"type": "session_entry_committed", "entryId": "u", "entryType": "message", "customType": None, "lane": "main"})
        out({"type": "agent_start"})
        steered_this_run = False
        deliver_steering(s)  # as rpi: one steered while idle waits for the next run
        if os.path.exists(os.path.join(home, "slow")):
            time.sleep(float(open(os.path.join(home, "slow")).read() or 1))
        if os.path.exists(os.path.join(home, "wedge")):
            os.remove(os.path.join(home, "wedge"))
            set_pending(s, True)
            while True:  # wedged: no command is read or answered again, abort included
                time.sleep(3600)
        if os.path.exists(os.path.join(home, "hang")):
            os.remove(os.path.join(home, "hang"))
            set_pending(s, True)
            # Stay busy until aborted, then settle with no reply, as a cancelled run does.
            for line in iter(next_line, None):
                c = json.loads(line)
                open(os.path.join(home, "log.jsonl"), "a").write(line)
                if c["type"] != "abort":
                    # As rpi: what needs no run is answered; anything else waits for none.
                    if not quick(c, s):
                        respond(c, error="a run is already active")
                    continue
                abort_reply(c); set_pending(s, False)
                # As rpi: a cancelled run still records an aborted assistant message.
                msg = {"kind": "assistant", "role": "assistant", "content": [], "stopReason": "aborted",
                       "errorMessage": "abort error: Request aborted", "timestamp": 0}
                said(s, msg)
                turn_end(msg)
                out({"type": "session_entry_committed", "entryId": "e", "entryType": "message", "customType": None, "lane": "main"})
                out({"type": "agent_end", "messageCount": 0, "messages": []})
                settled("aborted")
                break
            continue
        if rate_limit() is not None:
            fail(s, rate_limit()); continue
        if "[progress]" in cmd["message"]:
            # A note of its own before the rest, so a run cut later has partial work.
            said(s, assistant("progress: halfway"))
        turns = re.search(r"\[turns (\d+)\]", cmd["message"])
        if turns:
            for _ in range(int(turns.group(1))):
                turn_end()
            hanging = True; continue
        if "[hang-pending]" in cmd["message"]:
            # As a run cut mid-flight: resumable after a restart (`resume_pending`).
            set_pending(s, True)
            hanging = True; continue
        if "[hang]" in cmd["message"]:
            hanging = True; continue
        if "bash: " in cmd["message"] and tools:
            out({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash",
                 "args": {"command": cmd["message"].split("bash: ", 1)[1]}})
            finish(s, "ran"); continue
        if "[tool]" in cmd["message"] and tools:
            run_tool(s)
        if "[pend-after-steer]" in cmd["message"] and steered_this_run:
            # A crash point: the steer was answered, the run never settles until resumed.
            set_pending(s, True)
            hanging = True; continue
        answers, text = [], ""
        for name, args in re.findall(r"\[call (\w+) (\{[^{}]*\})\]", cmd["message"]) if tools else []:
            # A fresh id per call, unique across children too, as a model's are: main and its tasks
            # share one gateway dir, so a task must never read main's answer to the same number.
            calls += 1
            text = run_tool(s, name, json.loads(args), "c%d-%d" % (os.getpid(), calls))
            if text is None:
                break
            if name in ("gateway", "task", "exec", "process", "vault"):
                answers.append("\n%s: %s" % (name, text))
        if text is None:
            finish(s, "", "aborted"); continue
        if cmd["message"].endswith("model?"):
            finish(s, "model: %s" % model); continue
        if cmd["message"].startswith("say: "):
            finish(s, cmd["message"][5:]); continue
        if "[stop-after-steer]" in cmd["message"] and steered_this_run:
            finish(s, ""); continue
        if "reply with exactly NO_REPLY" in cmd["message"] and "[unfinished]" not in cmd["message"]:
            finish(s, "NO_REPLY"); continue
        finish(s, "echo: " + cmd["message"] + "".join(answers))
    elif kind == "resume_pending":
        if not s["pending"]:
            respond(cmd, {"resumed": False}); continue
        respond(cmd, {"accepted": True}); running = True
        out({"type": "agent_start"})
        # While `resume_wait` exists the resumed run stays open, taking commands (a steer is held).
        while os.path.exists(os.path.join(home, "resume_wait")):
            try:
                line = lines.get(timeout=0.05)
            except queues.Empty:
                continue
            if line is None:
                break
            open(os.path.join(home, "log.jsonl"), "a").write(line)
            if not quick(json.loads(line), s):
                deferred.append(line)
        last = [m for m in s["messages"] if m["kind"] == "user"][-1]
        finish(s, "resumed: " + last["content"])
    elif kind == "navigate":
        target = cmd.get("targetId")
        if target is not None and target not in [m["entryId"] for m in s["messages"]]:
            respond(cmd, error="unknown entry: %s" % target); continue
        append({"kind": "lane", "lane": "main", "leafId": target})
        respond(cmd, {"outcome": "completed", "leafId": target})
    elif kind == "compact":
        respond(cmd, {"accepted": True})
        before = sum(len(json.dumps(m["content"])) for m in s["messages"])
        add(s, None, "compaction", summary="summary", retainedTail=[])
        out({"type": "session_entry_committed", "entryId": "c", "entryType": "compaction", "customType": None, "lane": "main"})
        settled("compacted", summary="Conversation compacted · %d tokens before, 0 messages kept." % before)
    elif quick(cmd, s):
        pass
    elif kind == "abort":
        abort_reply(cmd)
        if hanging:
            hanging = False; finish(s, "", "aborted")
    elif kind == "stop":
        respond(cmd, {"stopped": True}); break
    else:
        respond(cmd, error="unknown command type: " + kind)
