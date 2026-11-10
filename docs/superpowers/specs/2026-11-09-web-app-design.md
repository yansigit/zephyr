# Web app: chat, approvals and settings from the phone, over Tailscale

Status: design drafted 2026-11-09; awaiting the owner's review.
Sub-project **W** of the Muse-parity roadmap (the table is in
`2026-11-07-settings-registry-design.md` §1). It depends on **S** (registry, strong channels) and
**A1** (gate, cards, `zephyr.decide`). It shows **A4**'s `zephyr.activity`
(`2026-11-09-activity-design.md`) and **B**'s goals (`2026-11-09-goals-memory-design.md`, the
`/goal` commands) without designing them. When the bridge network is on, it follows **A2**'s
listener rules (`2026-11-09-network-floor-design.md` §4.6).

## 1. Why

Away from the terminal, the owner reaches zephyr through Discord or iMessage. Both are weak
channels (DESIGN §4.17): a stolen session there can tighten a budget but not raise it, and cannot
approve a payment once `approvals.strong_only` names `money`. §4.16 keeps `money` off by default
"so a purchase can be approved from a phone until a stronger approval path (a web app with a
passkey) exists". W is that path. It is also a better phone client than a DM: the live run, the
whole card, settings, and a push when a card or reply arrives.

The Muse audit (`reports/Muse agent audit.md`, W section) sets these rules:

- **Copy Muse's live status line and drawer** (Activity, Approvals, Upcoming), using data the
  daemon already has.
- **Passkeys guard against a stolen phone session and record who approved what.** They do not
  catch more attacks: a trusted card on the host matched a phone passkey in 137 of 140 cells
  (arXiv 2610.21063). So the passkey gates only what a weak channel cannot do, and the signature
  commits to the stored payload's digest and the proposal id.
- **Enrolment is closed.** A one-time code delivered out of band is required (AB7).
- **The page is root's code** with anti-framing headers, and it never rebuilds a card that is on
  screen.
- **A lock-screen button approves only what any owner channel may already approve.** Money and
  loosening open the app and the passkey. Swipe-away counts as deny.
- **"The agent says" is collapsed.** For an approver who trusts the agent's words, removing that
  box cut attack success from 0.54 to 0.05.

## 2. Requirements

- **Serving.** The daemon serves HTTP with hyper on `127.0.0.1:<[web] port>` (default 7793). The
  owner fronts it with `tailscale serve` on the host, which gives HTTPS on the tailnet name, the
  secure context that passkeys and service workers need. `zephyr web setup` guides this.
- **Protocol.** A WebSocket at `/api/ws` carries the attach protocol's JSON-RPC (§4.3.1), one
  message per frame, served by `attach.rs`'s own `Conn`. There is no second protocol.
- **The app.** Static files are embedded in the binary: plain JS, no build step, phone-first. It
  covers chat, approval cards (ok, once, no, from the daemon's card text), settings and Waiting,
  facts (names only), the vault (list only), activity, goals and devices.
- **Passkeys.** WebAuthn ES256 with user verification, each passkey enrolled with a one-time code
  from a strong channel. Every approval made in the app, and every change it makes to a `strong`
  setting, is signed over a challenge that commits to the effect.
- **Push.** Web Push (VAPID, RFC 8291) for new cards, and for replies while no web client is
  connected. Notification buttons approve only messages and posts that need no strong channel.
- **Storage.** Schema **v19**: passkey credentials, push subscriptions, web sessions.
- **Checks.** Origin and Host checks, `__Host-` cookies, session lifetimes, rate limits. The
  Tailscale identity headers never authenticate.

### Non-goals

- Desktop or browser takeover ("Open browser", "Take control"): part C. The app's Stop is `/stop`.
- Typing secrets on the phone (vault entries, fact values, key or setup-token sign-ins). These stay
  on the TUI and the host CLI.
- Sending files from the app (Discord and iMessage do), and an Identity tab (B's).
- Receipts a third party can verify, and A4's hash chain. The challenge is shaped so that a
  receipts table can be added later (§4.5).
- Offline use, a QR code for enrolment, rich markdown, and more than one owner.
- Tailscale Funnel or any public exposure. W refuses them (§4.2).

## 3. What changes in DESIGN.md

- **§4.3** gains a **Web** bullet.
- **§4.3.1** gains `zephyr.web_challenge` and `zephyr.web_devices`, the `challengeId` and
  `assertion` params of `zephyr.decide` and `zephyr.settings_set`, `channel` on `zephyr_reply`, and
  the methods closed to `web`.
- **New §4.3.3, "The web app"**, holds §4.1–4.8 below once built.
- **§4.16, "Strong channels":** a payment's `/ok` is strong when signed with a passkey (channel
  `passkey`), and `zephyr web setup` offers to add `money` to `approvals.strong_only`.
- **§4.17:** `settings.strong_channels` may name `web` and `passkey`, and its default becomes
  `["tui", "chat", "passkey"]`. "Not done: the web app" is removed, and `web` joins the
  read-only row.
- **Elsewhere:** in §4.1 item 6, the sweep deletes expired web sessions. §4.8 gains
  `zephyr web setup` and `zephyr web enrol`, and §5 gains `src/web/` and `assets/web/`.
- **README** gains a "Web app" section, and **CLAUDE.md** one architecture bullet.

## 4. Design

### 4.1 One listener

`web::serve(cfg, tx, attach_tx, snap_rx, cfgs)` (`crates/zephyr/src/web/mod.rs`) joins the
router's `select!` beside `attach::serve` (router.rs:296), with the same handles, when `[web]` is
set. A change to `[web]` re-execs the daemon, as `[attach]` does (reload.rs:33).

```toml
[web]
host = "box.tail1234.ts.net"     # the name tailscale serve answers on; Host and Origin must match
port = 7793
# tailnet_users = ["owner@github"] # optional: Tailscale-User-Login must be one of these (§4.2)
```

The accept loop copies the iMessage webhook's (imessage.rs:278–320): a 32-connection semaphore, a
10 s `header_read_timeout`, `keep_alive(false)` and 64 KB `Limited` bodies (imessage.rs:253). It
adds `.with_upgrades()`.

| Method | Path | Needs | Does |
|---|---|---|---|
| GET | `/`, `/app.js`, `/app.css`, `/sw.js`, `/manifest.webmanifest`, `/icon-192.png`, `/icon-512.png` | — | embedded files (`include_bytes!`, `assets/web/`) |
| POST | `/api/enrol/start`, `/api/enrol/finish` | the code | creation options; store the passkey, set the cookie |
| POST | `/api/login/start`, `/api/login/finish` | an assertion | request options; set the cookie |
| POST | `/api/logout` | cookie | end this session |
| GET | `/api/ws` | cookie | WebSocket into an attach `Conn` on channel `web` |
| GET | `/api/push/key` | cookie | the VAPID public key |
| POST, DELETE | `/api/push` | cookie | save or drop this device's subscription |
| POST | `/api/push/decide` | cookie | a notification button, `{id, verb}` (§4.7) |

Anything else is 404. Every response carries these headers:

- `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src
  'self'; connect-src 'self' wss://HOST; manifest-src 'self'; worker-src 'self'; frame-ancestors
  'none'; base-uri 'none'; form-action 'none'`;
- `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff` and `Referrer-Policy: no-referrer`;
- `Cross-Origin-Opener-Policy` and `-Resource-Policy: same-origin`;
- `Cache-Control: no-store` on `/api/*`, and `no-cache` on the files, whose ETag is the crate
  version.

Nothing is inline, so the CSP needs no nonces.


### 4.2 Reaching it: `tailscale serve`, Host, Origin, identity headers

- **Listen address.** Under today's `--network host` (host/mod.rs:335) the daemon listens on
  `127.0.0.1:<port>`, the host's loopback, where `tailscale serve` reaches it. Under A2's bridge
  (`ZEPHYR_NETWORK=bridge`) it listens on the container's own address (`egress::own_ip()`, A2 §4.6),
  as attach does, and A2's `up` gains `-p 127.0.0.1:<web.port>:<web.port>`. Any other address is
  refused at start, as imessage's `check_listen` refuses `0.0.0.0`.
- **Host** must equal `[web] host`, else 421. That stops DNS rebinding from a page on the owner's
  laptop.
- **Origin.** Every POST and DELETE, and the WebSocket upgrade, need `Origin: https://HOST`
  exactly, and `Sec-Fetch-Site: same-origin` when that header is sent, else 403. JSON bodies need
  `Content-Type: application/json` (else 415), which a cross-site form cannot send without a
  preflight. With `SameSite=Strict`, that is the CSRF defence, and it covers WebSocket hijacking.
- **Tailscale identity headers.** `tailscale serve` adds `Tailscale-User-Login` for tailnet users;
  Funnel traffic and tagged nodes get none.
  - *What they may do:* a request without the header is refused (403, "open the app through
    tailscale serve"). With `tailnet_users` set, a login not on the list is refused too (compared
    ignoring case). That turns away an accidental `tailscale funnel`, a browser on the box hitting
    the port, and other people on a shared tailnet.
  - *What they may never do:* authenticate or authorise anyone, excuse a passkey, or stand for the
    owner in a log, an audit row or `settings_log`. Anything that can open the port can send any
    header: the agent under host networking, another host user, another process in the container.
- **`zephyr web setup`** (host, `host/mod.rs`) prints each step and asks before it changes the
  host:
  1. It reads `.Self.DNSName` from `tailscale status --json`, or says how to install or log in.
  2. It checks that tailnet HTTPS certificates are on.
  3. It writes `[web] host` and `port` (`conf::set`).
  4. It runs `tailscale serve --bg http://127.0.0.1:7793`, printing the `sudo` form, or
     `sudo tailscale set --operator=$USER`, when operator rights are missing.
  5. It warns never to point `tailscale funnel` at the port.
  6. It runs `zephyr up`.
  7. It offers to add `passkey` to an explicit `settings.strong_channels`, with `zephyr config set`
     over the chat socket.
  8. It runs `zephyr web enrol` (§4.5).
  9. Once a passkey exists, it offers to add `money` to `approvals.strong_only`.

  The host's `zephyr status` gains a line saying whether `tailscale serve status` forwards to the
  port.

### 4.3 Channels `web` and `passkey`

A session cookie is a bearer token on a phone. It is worth what a Discord session is worth. So the
app speaks as two channels:

- **`web`** is everything the cookie alone does: chat rows (inbox channel `web`, sender `web`),
  unsigned `no` and `cancel`, changes to settings that are not `strong`, and the notification
  buttons. It is weak unless the owner lists it in `strong_channels`, as they could list `discord`.
- **`passkey`** is the actor channel of a `zephyr.decide` or `zephyr.settings_set` that carried an
  assertion verified against a challenge bound to that exact request (§4.5). It never has inbox or
  outbox rows. It is in the default `strong_channels`, so a signed approval can do what the TUI
  can: a payment under `strong_only = ["money"]`, a strong loosening, a strong proposal.
  `settings_log.channel`, the audit rows and the `/ok` replies read `passkey`.

Code changes:

- `settings::CHANNELS` (settings.rs:184) gains `web` and `passkey`, and `strong_default`
  (config.rs:212) gains `passkey`. `tui` stays mandatory (config.rs:416–428).
- `attach::attached(channel)`, true for `tui` and `web`, replaces `== attach::CHANNEL` wherever the
  check means "a client on the port": `Config::is_owner` (config.rs:462), `owner_on`
  (router.rs:2766), the notify short-cut (router.rs:3102), the reply `show` (router.rs:1712) and
  the outbox flush (router.rs:3629, 3645). So `web` rows reach the tap as `tui` rows do. While no
  web socket is open they wait in the outbox, and one push goes out (§4.7).
- `AttachRequest::SettingsSet`, `Decide` and `Dequeue` (attach.rs:47–74) carry the channel. The
  router's `settings_set`, `decide` and `dequeue` (router.rs:1371, 1462, 1525) use it in place of
  `attach::CHANNEL`. `not_now` and `actions::approve` (actions.rs:114) already take a channel, so
  the strong checks do not change.
- `zephyr_reply` gains `channel` (null for a notice), and `Router::show` (router.rs:3703) takes it.
  Each client draws the command replies for its own channel, and every notice.
- `Snapshot.channels` lists `web` (router.rs:1194).
- `gateway_kind` (zephyr-policy tools.rs:140) does not know `/web`, so the agent's
  `gateway "/web …"` is refused.

### 4.4 The WebSocket bridge

**Decision: hyper's upgrade (`hyper::upgrade::on`, `hyper_util::rt::TokioIo`), framed by
`tokio-tungstenite`.** It becomes a direct dependency at `=0.21.0`, with `default-features =
false, features = ["handshake"]`.

- It is already in Cargo.lock through serenity, so no crate is added. `cargo tree -d` must show no
  new duplicate.
- The accept key comes from `tungstenite::handshake::derive_accept_key`.
- The stream is `WebSocketStream::from_raw_socket(io, Role::Server, cfg)`, with
  `max_message_size = MAX_LINE` (1 MiB, attach.rs:40).
- Hand-rolled framing (masking, fragments, control frames, close codes) would be about 200 lines
  that buy nothing.

**`attach::Conn` is reused whole.**

- `Conn::run` (attach.rs:169) takes any `AsyncRead + AsyncWrite + Send + 'static`, split with
  `tokio::io::split`, and `read_lines` (attach.rs:747) becomes generic.
- A new `pub async fn serve_stream(stream, Origin)`, where `Origin` is `Tui` or
  `Web { session, signer }`, gives `Conn` a `channel: &'static str` and a
  `signer: Option<Arc<web::Passkeys>>`. A web `Conn` starts `authed`: the cookie was checked at
  the upgrade.
- `web::ws` joins the socket to a `tokio::io::duplex`:
  - each text frame goes in with a `\n`, and each line `Conn` writes goes out as one frame;
  - a binary frame, or a text frame holding a newline (`JSON.stringify` writes none), closes it;
  - it pings every 30 s;
  - it re-checks the session every 60 s, and at once when a `/web` command bumps a `watch`, and
    closes the socket once the session is revoked or expired.
- At most 4 web sockets are open at once; a 5th upgrade gets 503. `web::connected()`, an
  `AtomicUsize`, tells the router whether any is open.
- **Open on `web`:** `zephyr.hello`, `zephyr.subscribe`, `send` (`prompt` with `followUp`,
  `abort`, `get_state`), `zephyr.models`, `zephyr.dequeue`, `zephyr.turns`, `zephyr.settings`,
  `zephyr.settings_set`, `zephyr.proposals`, `zephyr.decide`, `zephyr.facts_list`,
  `zephyr.vault_list`, `zephyr.activity` (A4), `ping`, and the two methods below.
- **Closed with `-32601`** ("not available in the web app"): `authenticate`, the stock
  `start_session`, `subscribe`, `stop_session` and `list_sessions`, `zephyr.login_*`,
  `zephyr.vault_put`/`_rm`/`_revoke`, `zephyr.facts_put`/`_rm`, and `zephyr.sessions`.
- The TUI's `Conn` is unchanged: its `signer` is `None`, and an `assertion` param is ignored.

```jsonc
// zephyr.web_challenge: what the owner is about to sign
{"op": "decide", "id": 7, "verb": "ok", "card_sha256": "<hex of the card text on screen>"}
{"op": "set", "key": "budget.daily_cost_usd", "value": "8", "until": "2h"}
// → {"challengeId": "c3", "publicKey": {"challenge": "<b64url>", "rpId": "HOST", "timeout": 120000,
//    "userVerification": "required", "allowCredentials": [{"type": "public-key", "id": "<b64url>"}]}}
//   or -32602 "the card changed; look at it again" | "no proposal #7 is waiting" | …
// then, with navigator.credentials.get's result (fields base64url):
{"method": "zephyr.decide", "params": {"id": 7, "verb": "ok", "challengeId": "c3",
  "assertion": {"id", "clientDataJSON", "authenticatorData", "signature", "userHandle"}}}
// zephyr.web_devices {} → {"passkeys": [{"name", "created_at", "last_used_at"}],
//   "sessions": [{"passkey", "created_at", "seen_at", "this"}], "push": [{"passkey", "created_at"}]}
```

On `web`, `decide ok` or `once` without an assertion is refused (`-32602` "approve with your
passkey"): every approval made in the app is signed. A `settings_set` without an assertion acts on
`web`. A tightening then applies, and a strong loosening gets the registry's own refusal
(`loosening KEY needs tui, chat or passkey`). The app signs whenever the setting is `strong`.

### 4.5 Passkeys

`web/passkey.rs`. **Decision: verification is hand-rolled on `ring`**, which is already a direct
dependency for TOTP. webauthn-rs is ruled out for two reasons. Its challenge is always random
(`WebauthnCore::generate_challenge` in webauthn-rs-core), so a signature could not commit to the
digest. It also brings OpenSSL-backed crypto and a dozen crates. The subset zephyr needs is small:
ES256 only (alg −7, which every platform passkey offers), `attestation: "none"`, discoverable
credentials, and user verification required.

**Enrolment code.** `/web enrol [NAME]` is taken only from a channel in `strong_channels` other
than `passkey`, so from tui or chat by default. In the container, `zephyr web enrol` asks for it
over the chat socket, as `zephyr set` does.

- **Storage.** One code is kept in `meta` (`web_enrol`: the code's SHA-256, the name, and a 10 min
  expiry). A new code replaces it.
- **The reply.** `[zephyr] open https://HOST/#enrol=ABCDE-FGHJK on the phone within 10 min, or
  type the code there; it works once`.
- **The code** is 10 base32 characters (50 bits, `data-encoding`), so it can be typed on a phone.
  The lockout (§4.6) leaves guessing hopeless.
- **The fragment** never reaches a log or a `Referer`.
- **The agent never sees it.** The code is a reply on tui or chat, `gateway` refuses `/web`, and
  the tap needs the attach token or a cookie.

**Enrolment.** `enrol/start {code}` checks the code in constant time (`subtle`) and answers:

- `rp: {id: HOST, name: "zephyr"}`. The full host is used, since `ts.net` is a public suffix.
- `user: {id: <the install's random 16-byte handle, in meta>, name: "zephyr owner"}`.
- `pubKeyCredParams: [{alg: -7}]`, `residentKey: "required"`, `userVerification: "required"`, and
  the stored passkeys as `excludeCredentials`.
- A random challenge, kept 2 min.

`enrol/finish` takes `{id, clientDataJSON, authenticatorData, publicKey, alg}` from the browser's
`getAuthenticatorData()`, `getPublicKey()` and `getPublicKeyAlgorithm()`, so no CBOR is parsed.
It checks:

- the type `webauthn.create`, the challenge and origin, and no `crossOrigin`;
- the `rpIdHash`, and the UP, UV and AT flags;
- that the attested credential id equals `id`, and that `alg == -7`;
- that `publicKey` is the fixed 26-byte P-256 SPKI prefix followed by the 65-byte point. Only the
  point is stored.

It then consumes the code and stores the passkey (at most 10). It starts a session and tells every
attached client and the last owner: `[zephyr] passkey NAME was added to the web app; /web rm NAME
removes it`. That notice is how the owner finds out if someone else used the code.

**Sign-in.** `login/start` answers options with no `allowCredentials`, `userVerification:
"required"` and a 2 min challenge. `login/finish` finds the credential by id and runs `verify`
(type `webauthn.get`). It updates `sign_count` and `last_used_at` and sets the cookie.

**Signing an approval.** `zephyr.web_challenge` builds the preimage as a JSON array, so its order
is fixed:

- `["zephyr-web/1", "decide", 7, "ok", <digest>, <card_sha256>, <nonce>]`, or
- `["zephyr-web/1", "set", key, value, until, <current value>, <nonce>]`.

The fields:

- `digest` is `Store::proposal_digest(id)`: SHA-256 over the row's `kind, class, command, payload,
  secrets, key, seen`, which is everything `/ok` reads.
- `card_sha256` must equal the hash of the card the router would show now. A new
  `AttachRequest::Card {id}` returns that card (`proposals_json`'s `card`, router.rs:1390). On a
  mismatch the answer is "the card changed", so the page and the daemon agree on what is signed.
- The WebAuthn challenge is SHA-256(preimage). It is kept in memory with its request and session,
  lives 2 min, is used once, and is removed on the first finish whether that succeeds or not. At
  most 16 are pending, and the oldest goes first.

A `decide` or `settings_set` with the `challengeId` then recomputes both hashes and refuses any
change. It runs `verify` and passes channel `passkey`. Keeping the preimage and the clientData
would later make a receipt anyone can check.

**`verify`** is pure. It checks:

- `clientDataJSON`: its `type`, its `challenge` (base64url), `origin == "https://HOST"`, and no
  `crossOrigin`.
- `authenticatorData`: at least 37 bytes, `rpIdHash == SHA-256(HOST)`, and the UP and UV flags.
- The signature: `ECDSA_P256_SHA256_ASN1` over `authenticatorData ‖ SHA-256(clientDataJSON)`.
- The sign count: a non-zero count at or below the stored non-zero count is refused as a clone.
  Synced passkeys send 0.

**Removal.** `/web rm NAME` works from any owner channel, since it only takes away. It deletes the
passkey, its sessions and its pushes, and bumps the `watch`. After the last passkey is lost, a new
`/web enrol` starts over.

### 4.6 Sessions, cookies, rate limits

**The cookie** is `__Host-zephyr=<32 random bytes>; Path=/; Secure; HttpOnly; SameSite=Strict;
Max-Age=2592000`.

- Only its SHA-256 is stored.
- A session lasts 30 days from sign-in, or until it is 7 days unused. `seen_at` is written at most
  once a minute.
- The hourly sweep deletes expired sessions.
- There are at most 20 sessions, and a new one drops the oldest.
- `/web signout` ends all sessions; `/api/logout` ends this one.

**Rate limits** are global and in memory, since every request comes from 127.0.0.1.

- The unauthenticated routes take 20 requests a minute, then answer 429.
- 5 failed finishes in 10 min close enrolment and sign-in for 15 min. A wrong code, an unknown
  credential, a bad signature and a stale challenge all count. The last owner is told once:
  `[zephyr] 5 failed passkey attempts on the web app; sign-in is closed until 14:05`. Open
  sessions keep working.
- Each session gets 60 socket requests a minute (past that, the socket is closed), 10
  `web_challenge` and 10 `/api/push/decide` a minute.

### 4.7 Web Push

`web/push.rs`. **Decision: hand-rolled on `ring` and `reqwest`**, both already direct
dependencies; the `web-push` crate brings `ece`, with its OpenSSL backend, and its own HTTP
client. About 150 lines cover three parts:

- **VAPID (RFC 8292):** an ES256 JWT (`ECDSA_P256_SHA256_FIXED_SIGNING`),
  `{"aud": <endpoint origin>, "exp": now+12h, "sub": "https://HOST"}`, so no personal address.
- **aes128gcm (RFC 8291):** ephemeral ECDH P-256, two HKDF-SHA256 rounds, AES-128-GCM, one record
  with the `0x02` delimiter, and the header `salt ‖ 4096 ‖ 65 ‖ as_public`.
- **The POST:** `TTL`, `Urgency`, `Content-Encoding: aes128gcm` and
  `Authorization: vapid t=…, k=…`.

`encrypt(ecdh_secret, as_public, ua_public, auth, salt, plaintext)` is pure, so RFC 8291
Appendix A tests it.

**The key** is `<state>/web/vapid.pk8` (root, 0600, in a 0700 folder), made at the first start.
Each time the app opens, it compares `/api/push/key` with its subscription's
`applicationServerKey` and subscribes again if they differ, so a remade key heals itself.

**Subscribing** is a button on Devices, because iOS asks for permission only on a tap, and only in
a Home Screen app.

- `{endpoint, keys}` is saved against the session's passkey, at most 10 of them.
- The endpoint must be `https` on `fcm.googleapis.com`, `*.push.apple.com`,
  `*.push.services.mozilla.com` or `*.notify.windows.com`, else 400, so the daemon relays no
  other POST.
- Tests widen the list with a hidden `Config::push_hosts`, as with `tick`.

**Sending.** Each push is spawned with a 10 s timeout and opens the store itself, as
`zephyr.sessions` does (attach.rs:420), so the router never waits. A 404 or 410 deletes the
subscription. Any other failure is logged once
(`shortcut: no retry; add one if pushes are seen lost`).

**Cards.** `Router::push_cards` runs from `publish` (router.rs:1183) and from the 5 s tick, so a
card stored off the loop (by the gate socket) goes out within a tick.

- It pushes every pending proposal whose id is above `pushed_up_to`. That starts at the newest id
  when the daemon starts, so no card is pushed twice.
- The payload is `{"kind": "card", "id", "title": "zephyr: approve #7", "body", "quick"}`, with
  `Urgency: high` and `TTL: 86400`.
- `body` is the class and the summary, facts-scrubbed as in `proposals_json`. For a setting it is
  the card's first line.
- `quick` is true only for a held `messages` or `posts` action that is not `strong`.
- The agent's reason is never pushed.

**Replies.** The outbox flush pushes a `web` row once while `web::connected()` is 0. The payload is
`{"kind": "reply", "body": <first 120 chars>}` with `TTL: 3600`: no more than a Discord DM shows.
`shortcut: the pushed set is in memory, so a restart may push a reply twice`.

**`sw.js`.**

- It shows each push as a notification tagged `card-7` or `reply`.
- A card gets the buttons `[Approve, Deny]` when `quick`, else `[Deny]`. A tap opens `/#held/7`.
  iOS shows no buttons, so a tap opens the app.
- A button posts `/api/push/decide {id, verb}`.
- `notificationclose` on a card sends `no`. That is the audit's swipe-away-as-deny, and it can only
  refuse.

**`/api/push/decide`** re-reads the proposal at click time.

- It takes `ok` only for a pending held `messages` or `posts` action whose `strong` is false. It
  takes `no` for any proposal.
- Anything else gets 409, "open the app to approve this".
- It calls `Decide` on channel `web`. If the settings changed in between, `not_now` and `approve`
  still refuse anything strong, so a lock-screen button never approves money or a loosening.

### 4.8 The app

`crates/zephyr/assets/web/` holds `index.html`, `app.js` (one file of plain ES2020, about 1,200
lines, no framework), `app.css`, `sw.js` and `manifest.webmanifest` (standalone, with two PNG
icons). The app is phone-first; from 900 px, the drawer becomes a side panel. Daemon text goes in
through `textContent`, never `innerHTML`. Control and direction marks are dropped, as in the
TUI's `view::clean`.

1. **Sign-in.** "Sign in with passkey". With `#enrol=` or a typed code, it offers "Add this
   device" and a name field.
2. **Chat** (home).
   - The status line, from `Snapshot.run`, is worded like Discord's progress note
     (`working · 0:24 · bash · 3 tools`). It sits next to the `paused` and `N waiting` badges, and
     a tap opens the drawer.
   - The transcript comes from `zephyr.subscribe`: user bands with their channel and time, text
     streamed from `message_update`, tools folded into `▸ 3 tools  bash, read ×2 ✓ 4.1s`,
     `zephyr_reply` as notices, and code in `<pre>` with a copy button.
   - The queue strip, from `Snapshot.queue`, offers "take back" (`zephyr.dequeue`).
   - The composer has a follow-up toggle, and Stop (`abort`) while a run goes.
3. **Drawer.** It opens on Approvals when something waits, as Muse does on Android.
   - **Approvals** (`zephyr.proposals`): newest first, with class, summary, age and expiry.
   - **Activity** (`zephyr.activity`): A4's entries, with A4's revoke commands sent as prompts.
     Hidden until A4 exists.
   - **Upcoming**: `Snapshot.jobs` and `scheduled` proposals.
4. **Card sheet.**
   - The daemon's `card` is shown verbatim, in monospace.
   - Its `the agent says: …` line goes in a closed `<details>`, "the agent's reason (not
     checked)". That line is safe to pick out: every line but the first and the last starts with a
     daemon label, and agent text is only ever quoted inside one (§4.16).
   - The buttons: **Approve** (signed), **Approve once** (only when `once`), **Deny**, and
     **Cancel** (for `scheduled`).
   - An open sheet is never rebuilt. If a reload changes or drops its proposal, it greys out
     ("this card changed or was handled; close and look again") and its buttons stop.
5. **Settings** (`zephyr.settings`). Each row is `label · value · source` and is edited by its
   kind (a select, the `zephyr.models` list, or a field), with an optional "for 2h". Strong rows
   are signed. A refusal shows the daemon's reason.
6. **Facts** (`zephyr.facts_list`): name, kind and allowed targets, never a value. **Vault**
   (`zephyr.vault_list`): names, sites and grants. Both point at the TUI or the CLI for changes.
7. **Goals.** B adds no attach method, so this screen sends `/goals`, lists the reply's `N. TEXT`
   lines, and sends `/goal add`, `/goal done N` and `/goal rm N`.
8. **Devices** (`zephyr.web_devices`): push on and off, sign out, and remove a passkey (which sends
   `/web rm NAME`).

The socket reconnects with backoff (1 s doubling to 30 s) and subscribes again. A 401 goes back to
sign-in.

### 4.9 Storage: schema v19

`SCHEMA_VERSION` becomes 19. B takes v18, and A4 needs none (its open question 1).

```sql
CREATE TABLE web_passkeys(id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE,
    cred_id BLOB NOT NULL UNIQUE, public_key BLOB NOT NULL,  -- the 65-byte P-256 point
    sign_count INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()), last_used_at INTEGER);
CREATE TABLE web_sessions(token_hash BLOB PRIMARY KEY,
    passkey INTEGER NOT NULL REFERENCES web_passkeys(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL, seen_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
CREATE TABLE web_push(id INTEGER PRIMARY KEY, endpoint TEXT NOT NULL UNIQUE,
    p256dh BLOB NOT NULL, auth BLOB NOT NULL,
    passkey INTEGER NOT NULL REFERENCES web_passkeys(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()));
PRAGMA user_version = 19;
```

The enrolment code and the user handle live in `meta`. Challenges and rate counters live in
memory, so a restart drops them and the app asks again. `Debug` on session and push rows prints
lengths only.

### 4.10 Commands and files

- `/web` answers `[zephyr] web app https://HOST · passkeys: phone (used 2h ago), ipad · 3 sessions
  · push on 2 devices`, or "not set up; zephyr web setup on the host".
- `/web enrol [NAME]` is taken from strong channels other than `passkey`.
- `/web rm NAME` and `/web signout` are taken from any owner channel.
- On the host: `zephyr web setup` and `zephyr web enrol [NAME]`.

New files are `src/web/mod.rs` (about 450 lines), `src/web/passkey.rs` (about 250),
`src/web/push.rs` (about 200) and `assets/web/*`.

Changed files:

- `attach.rs`: the generic `run`, `serve_stream`, the channel and signer, the method gate, and two
  methods.
- `router.rs`: the channel on three requests, `attached()`, `push_cards`, the flush hook, `/web`,
  `AttachRequest::Card`, and the `select!` arm.
- `config.rs`, `settings.rs` and `store.rs`: v19 and `proposal_digest`.
- `retention.rs`, `reload.rs`, and `main.rs` (the in-container `zephyr web enrol`).
- `host/mod.rs`: `web setup|enrol`, and the publish line under A2.
- `doctor`: the listener and the VAPID key.
- Cargo.toml: `tokio-tungstenite`.

`zephyr-policy` and `fake-rpi.py` are untouched.

### 4.11 Failure handling

- **No `[web]`.** Nothing listens.
- **The port is taken.** The daemon logs it and runs without the app. `doctor` fails, and
  `zephyr status` reads `web: not listening`.
- **The daemon restarts.** Sockets close and the app reconnects. Sessions survive, but challenges
  do not: a signature that spans a restart is refused, and the owner taps again. No card is
  pushed twice.
- **`tailscale serve` or the tailnet is down.** The app is unreachable and nothing on the daemon
  breaks. The host's status line points at `tailscale serve status`.
- **A card changes between view and signature** (a fact was saved, a mode changed). The signature
  is refused and the sheet greys out, so the owner reads the new card. A proposal handled
  elsewhere gets `/ok`'s own answer. Of two approvals at once, `take_action` lets one through
  (§4.16).
- **A lost phone.** `/web rm NAME` from any channel ends its sessions and pushes at once. A new
  passkey needs `/web enrol` from a strong channel.
- **Push trouble** never touches the router. An unreadable VAPID key turns push off with a `doctor`
  line. A clock hours off breaks the JWT `exp`, and the refusal is logged.
- **The agent at the port.** Under host networking it can reach the port. Under A2, the rule set
  refuses its uid. At the port it can:
  - fetch the static files, which hold nothing private;
  - fail at enrolment and sign-in, which costs at most a 15 min sign-in lockout while open
    sessions keep working;
  - forge Tailscale headers, which decide nothing.

  It cannot use a session or sign a challenge.

## 5. Testing

Each piece starts with a failing test (CLAUDE.md, DESIGN §6).

- **`web/passkey.rs`.** A software authenticator (ring `EcdsaKeyPair`,
  `ECDSA_P256_SHA256_ASN1_SIGNING`) signs the clientData and authData.
  - `verify` accepts the good assertion.
  - It refuses each of these: a changed type, challenge or origin; `crossOrigin: true`; a wrong
    rpIdHash; UV or UP clear; a sign-count regression; a bad SPKI prefix; alg ≠ −7; an attested id
    that differs.
  - Preimages that differ in any field give different challenges.
- **`web/push.rs`.**
  - `encrypt` matches RFC 8291 Appendix A byte for byte.
  - The VAPID JWT's claims verify with ring.
  - The allowlist refuses `https://fcm.googleapis.com.evil.test`, `http://` and userinfo.
- **`web/mod.rs`.**
  - The Host, Origin, `Sec-Fetch-Site`, Content-Type and Tailscale-header checks, and
    `tailnet_users`.
  - The cookie attributes, and idle and absolute expiry.
  - The rate windows and the lockout.
  - `app.js` holds no `innerHTML`, `insertAdjacentHTML`, `outerHTML`, `eval(` or `new Function`,
    and `index.html` holds no inline `<script>` or `style=`.
- **Store.**
  - The v19 migration on a v18 database.
  - A passkey's delete cascades to its sessions and pushes.
  - `proposal_digest` changes when any column `/ok` reads changes.
- **Integration, `tests/web.rs`.** Fake-rpi with `[web] host = "zephyr.test"`, reqwest setting
  Host, Origin and `Tailscale-User-Login`, and the software authenticator.
  - **Headers.** The files carry the CSP and frame headers. `/api/ws` gets 401 without a cookie,
    403 for a wrong Origin, 421 for a wrong Host, and 403 without the Tailscale header.
  - **Enrolment.**
    - `zephyr web enrol` over the chat socket gives a code.
    - A wrong code fails. Five failures lock enrolment (a hidden short lockout) and send the
      notice.
    - The right code enrols, sets the cookie and sends "passkey added", then fails a second time.
    - `/web enrol` from a weak owner channel is refused.
  - **Chat.**
    - `zephyr.hello` answers.
    - A `send` is answered by fake-rpi: the row is on `web`, and the reply arrives as events.
    - `/status` comes back as a `zephyr_reply` with `channel: "web"`.
  - **A held `himalaya` send.**
    - An unsigned `ok` is refused, and so is a wrong `card_sha256`.
    - A signed `decide` runs the stored command (`log.jsonl`), and the audit row reads `passkey`.
    - A challenge used twice is refused.
  - **With `strong_only = ["money"]`.**
    - A money action is refused by `/api/push/decide` and unsigned, and approved signed.
    - A strong `settings_set` loosening is refused unsigned and applied signed.
  - **Push.** A fake push service (a hyper server, through `push_hosts`) receives the card.
    - The test decrypts it with its own ring ECDH key.
    - It checks the VAPID header and `quick`, and that the reason is absent.
    - A reply is pushed only while no socket is open.
    - A 410 deletes the subscription.
  - **Limits.**
    - `/web rm` closes that passkey's socket.
    - A 5th socket gets 503.
    - A 2 MiB frame closes the socket, and so does one with a newline.
- **Manual check (README).** Before a release that touches `assets/web/`, on iOS Safari as a Home
  Screen app and on Android Chrome: enrol, sign in, chat, approve a card signed, deny from a
  notification, and swipe one away (Android).
- **Not automated:** a real `tailscale serve`, and real push services.
