#!/usr/bin/env python3
"""A stand-in for Telnyx's Call Control API and media streaming (calls/telnyx.rs), and for the
owner's funnel in front of zephyr's calls listener. Standard library only.

    fake-telnyx.py PORT DIR

Listens on 0.0.0.0:PORT (0 picks one; the port is written to DIR/port once it listens) and
keeps its state in DIR:

  public_key   written at start: the ed25519 key it signs with, base64 (the vault's `public_key`)
  connect      read at each dial: `host:port` of zephyr's calls listener, which this connects to
               whatever host the `stream_url` and `webhook_url` name (it plays the funnel)
  script       read at each dial: what the other party does, one step a line (below)
  api.jsonl    every API request: {"method", "path", "auth", "body"}
  events.jsonl what came of the call: {"stream": <HTTP status of the media handshake>},
               {"webhook": <event type>, "status": <HTTP status>}, {"mark": <name>},
               {"clear": true}, {"dtmf": <digits>}, {"hangup": "api"}, {"closed": true}
  heard.ulaw   every byte of audio zephyr sent, in order

API: `POST /v2/calls` answers {"data": {"call_control_id": "fake-<n>", ...}} and starts the
script; `POST /v2/calls/<id>/actions/hangup` ends the call (the socket closes, a `call.hangup`
webhook follows); `.../actions/send_dtmf` is logged. A request whose `Authorization` is not a
Bearer token gets 401. With a DIR/api_fail file every dial gets 422 with the file's text as
the error's detail.

Script steps (default script: `stream`, `answer`):

  stream [unsigned|wrongkey|stale|badpath|replay]
                open the media socket to the dial's `stream_url`, signed over
                `<timestamp>|<stream_url>`; the variants send a connection zephyr must refuse
                (`replay` sends a good one's exact headers a second time on a new connection)
  answer        post the signed `call.answered` webhook
  webhook TYPE [unsigned]   post any other event
  say SECS      SECS of loud audio (a square wave), sent at once
  quiet SECS    SECS of silence, sent at once
  play FILE     FILE's bytes (μ-law, 8 kHz) in real time, then nothing
  wait SECS     sleep
  expect TEXT   wait (up to 60 s) until what zephyr said on the line holds TEXT
  hangup        the other party hangs up: `stop` on the socket, it closes, `call.hangup` is posted
  (end of script) the socket stays open, silent, until zephyr hangs up through the API

A mark zephyr sends is echoed at once (as if its audio had played), and logged.
"""
import base64, hashlib, http.server, json, os, socket, struct, sys, threading, time

PORT, DIR = int(sys.argv[1]), sys.argv[2]
os.makedirs(DIR, exist_ok=True)

# ---- ed25519 (RFC 8032), the reference construction ----
P = 2 ** 255 - 19
L = 2 ** 252 + 27742317777372353535851937790883648493
D = -121665 * pow(121666, P - 2, P) % P
I = pow(2, (P - 1) // 4, P)

def _xrecover(y):
    xx = (y * y - 1) * pow(D * y * y + 1, P - 2, P)
    x = pow(xx, (P + 3) // 8, P)
    if (x * x - xx) % P:
        x = x * I % P
    return P - x if x % 2 else x

_BY = 4 * pow(5, P - 2, P) % P
B = (_xrecover(_BY), _BY, 1, _xrecover(_BY) * _BY % P)

def _add(a, b):
    x1, y1, z1, t1 = a; x2, y2, z2, t2 = b
    A = (y1 - x1) * (y2 - x2) % P; Bq = (y1 + x1) * (y2 + x2) % P
    C = t1 * 2 * D * t2 % P; Dq = z1 * 2 * z2 % P
    E, F, G, H = Bq - A, Dq - C, Dq + C, Bq + A
    return (E * F % P, G * H % P, F * G % P, E * H % P)

def _mul(s, p):
    q = (0, 1, 1, 0)
    while s:
        if s & 1:
            q = _add(q, p)
        p = _add(p, p); s >>= 1
    return q

def _enc(p):
    x, y, z, _ = p
    zi = pow(z, P - 2, P); x, y = x * zi % P, y * zi % P
    return (y | ((x & 1) << 255)).to_bytes(32, "little")

def keypair(seed):
    h = hashlib.sha512(seed).digest()
    a = int.from_bytes(h[:32], "little"); a &= (1 << 254) - 8; a |= 1 << 254
    return a, h[32:], _enc(_mul(a, B))

def sign(key, msg):
    a, prefix, pub = key
    r = int.from_bytes(hashlib.sha512(prefix + msg).digest(), "little") % L
    R = _enc(_mul(r, B))
    h = int.from_bytes(hashlib.sha512(R + pub + msg).digest(), "little") % L
    return R + ((r + h * a) % L).to_bytes(32, "little")

KEY = keypair(hashlib.sha256(b"fake-telnyx").digest())
WRONG = keypair(hashlib.sha256(b"someone else").digest())
open(os.path.join(DIR, "public_key"), "w").write(base64.b64encode(KEY[2]).decode())

lock = threading.Lock()
def log(name, obj):
    with lock:
        open(os.path.join(DIR, name), "a").write(json.dumps(obj) + "\n")

def read(name, default=None):
    try:
        return open(os.path.join(DIR, name)).read().strip()
    except OSError:
        return default

def headers(signed, how=""):
    stamp = str(int(time.time()) - (900 if how == "stale" else 0))
    if how == "unsigned":
        return []
    sig = sign(WRONG if how == "wrongkey" else KEY, (stamp + "|").encode() + signed)
    return [("telnyx-timestamp", stamp), ("telnyx-signature-ed25519", base64.b64encode(sig).decode())]

def zephyr():
    host, _, port = read("connect", "127.0.0.1:1").rpartition(":")
    return socket.create_connection((host, int(port)), timeout=10)

def path_of(url):
    rest = url.split("://", 1)[-1]
    return "/" + rest.split("/", 1)[1] if "/" in rest else "/"

def host_of(url):
    return url.split("://", 1)[-1].split("/", 1)[0]

def webhook(call, kind, how=""):
    body = json.dumps({"data": {"record_type": "event", "event_type": kind, "id": "ev-%f" % time.time(),
                                "payload": {"call_control_id": call["id"], "to": call["to"]}}}).encode()
    status = 0
    try:
        s = zephyr()
        head = ["POST %s HTTP/1.1" % path_of(call["webhook_url"]), "Host: " + host_of(call["webhook_url"]),
                "Content-Type: application/json", "Content-Length: %d" % len(body), "Connection: close"]
        head += ["%s: %s" % h for h in headers(body, how)]
        s.sendall(("\r\n".join(head) + "\r\n\r\n").encode() + body)
        status = int(s.makefile("rb").readline().split()[1])
        s.close()
    except (OSError, IndexError, ValueError):
        pass
    log("events.jsonl", {"webhook": kind, "status": status})

class Stream:
    """The media WebSocket, as Telnyx opens it."""
    def __init__(self, call, how="", reuse=None):
        url = call["stream_url"]
        self.call, self.sock, self.open, self.send_lock = call, None, False, threading.Lock()
        self.headers = reuse or headers(url.encode(), how)
        path = "/calls/nowhere" if how == "badpath" else path_of(url)
        status = 0
        try:
            s = zephyr()
            head = ["GET %s HTTP/1.1" % path, "Host: " + host_of(url), "Upgrade: websocket", "Connection: Upgrade",
                    "Sec-WebSocket-Key: " + base64.b64encode(os.urandom(16)).decode(), "Sec-WebSocket-Version: 13"]
            head += ["%s: %s" % h for h in self.headers]
            s.sendall(("\r\n".join(head) + "\r\n\r\n").encode())
            f = s.makefile("rb")
            status = int(f.readline().split()[1])
            while f.readline() not in (b"\r\n", b""):
                pass
            if status == 101:
                s.settimeout(None)
                self.sock, self.file, self.open = s, f, True
            else:
                s.close()
        except (OSError, IndexError, ValueError):
            pass
        log("events.jsonl", {"stream": status})
        if self.open:
            self.seq = 0
            self.event({"event": "connected", "version": "1.0.0"})
            self.event({"event": "start", "sequence_number": "1", "stream_id": "s-" + call["id"],
                        "start": {"call_control_id": call["id"], "from": call["from"], "to": call["to"],
                                  "media_format": {"encoding": "PCMU", "sample_rate": 8000, "channels": 1}}})
            threading.Thread(target=self.reader, daemon=True).start()

    def event(self, obj):
        data = json.dumps(obj).encode()
        n = len(data)
        head = bytes([0x81]) + (bytes([0x80 | n]) if n < 126 else bytes([0x80 | 126]) + struct.pack(">H", n)
                                 if n < 65536 else bytes([0x80 | 127]) + struct.pack(">Q", n))
        mask = os.urandom(4)
        body = bytes(b ^ mask[i % 4] for i, b in enumerate(data))
        try:
            with self.send_lock:
                self.sock.sendall(head + mask + body)
        except OSError:
            self.open = False

    def audio(self, data):
        for i in range(0, len(data), 160):
            self.seq += 1
            self.event({"event": "media", "sequence_number": str(self.seq), "stream_id": "s-" + self.call["id"],
                        "media": {"track": "inbound", "chunk": str(self.seq), "timestamp": str(self.seq * 20),
                                  "payload": base64.b64encode(data[i:i + 160]).decode()}})

    def reader(self):
        try:
            while True:
                b = self.file.read(2)
                if len(b) < 2:
                    break
                op, n = b[0] & 0x0f, b[1] & 0x7f
                if n == 126:
                    n = struct.unpack(">H", self.file.read(2))[0]
                elif n == 127:
                    n = struct.unpack(">Q", self.file.read(8))[0]
                mask = self.file.read(4) if b[1] & 0x80 else None
                data = self.file.read(n)
                if mask:
                    data = bytes(c ^ mask[i % 4] for i, c in enumerate(data))
                if op == 8:
                    break
                if op != 1:
                    continue
                msg = json.loads(data)
                if msg.get("event") == "media":
                    with lock:
                        open(os.path.join(DIR, "heard.ulaw"), "ab").write(base64.b64decode(msg["media"]["payload"]))
                elif msg.get("event") == "mark":
                    log("events.jsonl", {"mark": msg["mark"]["name"]})
                    self.event({"event": "mark", "stream_id": "s-" + self.call["id"], "mark": msg["mark"]})
                elif msg.get("event") == "clear":
                    log("events.jsonl", {"clear": True})
        except (OSError, ValueError, struct.error):
            pass
        self.open = False
        log("events.jsonl", {"closed": True})

    def close(self, stop=False):
        if self.sock is None:
            return
        if stop and self.open:
            self.event({"event": "stop", "stream_id": "s-" + self.call["id"], "stop": {"call_control_id": self.call["id"]}})
        try:
            self.sock.shutdown(socket.SHUT_RDWR); self.sock.close()
        except OSError:
            pass
        self.open = False

calls = {}

def run(call):
    stream, good = None, None
    for line in (read("script") or "stream\nanswer").splitlines():
        step = line.split()
        if not step or call["over"].is_set():
            continue
        if step[0] == "stream":
            how = step[1] if len(step) > 1 else ""
            s = Stream(call, how, reuse=good.headers if how == "replay" and good else None)
            if how == "":
                stream = good = s
            elif s.open and stream is None:
                stream = s  # zephyr took what it should have refused: the test sees a 101
        elif step[0] == "answer":
            webhook(call, "call.answered")
        elif step[0] == "webhook":
            webhook(call, step[1], step[2] if len(step) > 2 else "")
        elif step[0] in ("say", "quiet") and stream:
            n = int(float(step[1]) * 8000)
            stream.audio(bytes((0x10 if i // 20 % 2 == 0 else 0x90) for i in range(n)) if step[0] == "say" else b"\xff" * n)
        elif step[0] == "play" and stream:
            data = open(step[1], "rb").read()
            for i in range(0, len(data), 160):
                if call["over"].is_set():
                    break
                stream.audio(data[i:i + 160]); time.sleep(0.02)
        elif step[0] == "wait":
            call["over"].wait(float(step[1]))
        elif step[0] == "expect":
            want, until = line.split(None, 1)[1].encode(), time.time() + 60
            while time.time() < until and not call["over"].is_set():
                try:
                    if want in open(os.path.join(DIR, "heard.ulaw"), "rb").read():
                        break
                except OSError:
                    pass
                time.sleep(0.05)
        elif step[0] == "hangup":
            if stream:
                stream.close(stop=True)
            call["over"].set()
            webhook(call, "call.hangup")
            return
    call["over"].wait(120)
    if stream:
        stream.close(stop=True)
    webhook(call, "call.hangup")

class Api(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def answer(self, status, obj):
        data = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_POST(self):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        try:
            body = json.loads(raw or b"{}")
        except ValueError:
            body = {}
        auth = self.headers.get("Authorization", "")
        log("api.jsonl", {"method": "POST", "path": self.path, "auth": auth, "body": body})
        if not auth.startswith("Bearer ") or len(auth) < 8:
            return self.answer(401, {"errors": [{"code": "10009", "title": "Authentication failed", "detail": "No key"}]})
        parts = self.path.strip("/").split("/")
        if parts == ["v2", "calls"]:
            fail = read("api_fail")
            if fail is not None:
                return self.answer(422, {"errors": [{"code": "90015", "title": "Invalid", "detail": fail}]})
            with lock:
                cid = "fake-%d" % (len(calls) + 1)
                call = dict(body, id=cid, over=threading.Event())
                calls[cid] = call
            threading.Thread(target=run, args=(call,), daemon=True).start()
            return self.answer(200, {"data": {"record_type": "call", "call_control_id": cid, "call_leg_id": "leg-" + cid,
                                              "call_session_id": "ses-" + cid, "is_alive": False}})
        if len(parts) == 5 and parts[:2] == ["v2", "calls"] and parts[3] == "actions":
            call = calls.get(parts[2])
            if call is None:
                return self.answer(422, {"errors": [{"code": "90015", "title": "Invalid Call Control ID",
                                                     "detail": "The call_control_id provided was not valid."}]})
            if parts[4] == "hangup":
                log("events.jsonl", {"hangup": "api"})
                call["over"].set()
            elif parts[4] == "send_dtmf":
                log("events.jsonl", {"dtmf": body.get("digits")})
            return self.answer(200, {"data": {"result": "ok"}})
        self.answer(404, {"errors": [{"title": "Not found"}]})

server = http.server.ThreadingHTTPServer(("0.0.0.0", PORT), Api)
open(os.path.join(DIR, "port"), "w").write(str(server.server_address[1]))
server.serve_forever()
