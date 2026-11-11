# Token broker: the agent holds surrogates, the proxy swaps in the real token

Status: design drafted 2026-11-10; awaiting the owner's review.
Sub-project **A3** of the Muse-parity roadmap (the table is in
`2026-11-07-settings-registry-design.md` §1). It needs A2 (the proxy of
`2026-11-09-network-floor-design.md`, DESIGN §4.20, shipped in 0.8.0). It uses S for two registry
rows. D (payments) will put its secrets through it.

## 1. Why

A token the owner saves in the vault reaches the agent as its real value today:

- `vault::exec_env` (`vault.rs:616`) returns `(token.name, token.value)`, the router and
  `actions.rs:215` pass it on, and `procs::spawn` sets it with `.envs(env)` (`procs.rs:373`). The
  command, and anything it starts, can write it to a file or send it anywhere.
- `drafts::filled` (`drafts.rs:805`) writes each `${NAME}` of a connector from secrets.env into
  `<state>-public/mcp.json`, which the agent can read (DESIGN §7: "the agent can read a
  connector's token").

§4.15's threat model says it plainly: the vault "does not stop a deliberately hostile agent from
reading a value it has been allowed to use". Output scrubbing only covers what comes back through
`exec`. Since A2, such an agent can no longer reach raw protocols or private addresses, but HTTPS
to any public host is open, so a token it holds can leave in one `curl` (§4.20: "taking the agent's
tokens away is a later sub-project that builds on this proxy").

The Muse audit (`reports/Muse agent audit.md`, "A3 should swap surrogates at the proxy, bound to
exact hosts") and its notes (`research_notes/Muse agent audit/memory_tokens_network.md`, "Token
brokering") describe the one pattern every vendor ships: a placeholder in the sandbox, the real
secret put in at an egress proxy, each secret bound to exact hosts, and only over TLS the proxy
itself has verified. Fly's Tokenizer names why the binding is not optional: without it the agent
sends the placeholder through the proxy to a server that records what arrives.

## 2. Requirements

- A vault token reaches the agent's processes as a surrogate, `zsur_` and 40 hex digits, in
  `exec`'s environment and in `mcp.json`. The real value stays in root's files and the daemon's
  memory.
- The proxy replaces a surrogate with the real value only in a request it has decrypted itself, to
  a host the owner bound that token to, over TLS it verified against roots the agent cannot change.
- Only hosts with a token bound are decrypted. Every other host is the plain CONNECT tunnel of
  §4.20, byte for byte.
- A surrogate the proxy sees on its way to any other host is never forwarded: the request is
  refused, audited by token name and told to the owner.
- Bindings, and every other loosening, come only from the owner on a strong channel. The agent may
  propose.
- Fail closed: no CA, no vault or no key means no swap and no real value, never a fall back to
  handing the token out.
- `vault::scrub` stays, and also covers surrogates.
- `doctor` shows what is decrypted and what still reaches the agent in plain.

### Non-goals

- **Provider credentials** (`accounts/*/auth.json`, `[agent] env` keys): §4.9 says why.
- **Vault logins.** A password is typed into a page, so it is in a form body and in the page. The
  origin check of §4.15 stays their only guard.
- **Body swaps**, and scanning bodies at all (§4.3).
- **Signing secrets** (AWS SigV4, HMAC webhooks, JWT service accounts). The agent cannot compute
  with a value it does not have, and proxy-side signers (Tokenizer's processors) are a project of
  their own. Such a token is `raw` (§4.2).
- **Decrypting everything.** No SNI or Host checks on unbound hosts, no content inspection. The
  floor's own non-goal stands for them.
- **Holding a request for the owner.** As in A2, clients time out first. The gate decides before a
  command runs (§4.8).
- HTTP/2, gRPC, client certificates and plain `http://` to a bound host.

## 3. What changes in DESIGN.md

- §4.4 item 1 (line 412): `mcp.json` no longer holds a connector's secret. `auth.json` stays
  readable.
- §4.6 connectors: `${NAME}` is filled with a surrogate; `hosts` and `secrets` on `[[mcp]]`.
- §4.8 `doctor`: the checks in §4.10 below.
- §4.13: `exec`'s `secrets` set surrogates.
- §4.15: the `[[token]]` fields `hosts`, `url`, `raw`; `/vault bind`, `unbind`, `url`, `raw`; the
  agent's `vault bind`; the threat model's last sentence becomes "a token bound to hosts never
  reaches the agent; a `raw` token and a login still can be read by a hostile agent".
- §4.17: two rows (§4.11).
- §4.20 "Not done": TLS interception and Host matching now exist for bound hosts only.
- §7 (lines 1018-1019): the connector line goes; the `auth.json` line stays, with §4.9's reasons.
- New §4.21 Token broker (this spec, once built). README: `/vault bind`, `raw`, what a certificate
  error on a bound host means. CLAUDE.md: `broker.rs` and `ca.rs` in the architecture list.

## 4. Design

### 4.1 Surrogates (`crates/zephyr/src/broker.rs`, new)

`broker::surrogate(key, kind, name)` is `"zsur_"` followed by the first 20 bytes, in hex, of
HMAC-SHA256 (`ring`, already a dependency) over `kind`, a zero byte and `name`. `kind` is `token`
for a vault token and `env` for a secrets.env name a connector uses. `key` is 32 random bytes in
`<state>/broker.key` (root, 0600), made on first use.

Why derived and not stored:

- Nothing to migrate and no table: the map from surrogate to token is rebuilt from `vault.toml` and
  the config whenever either changes (§4.4).
- It is stable across a daemon re-exec, so a command `procs` adopted, and the `mcp.json` a child
  loaded (which the policy plugin compares byte for byte), keep working.
- It depends on the name, not the value, so the owner rotates a token without restarting anything.
- The alphabet is `[a-z0-9_]`, which URL encoding, JSON and shells all leave alone, so the proxy
  finds it with one substring search for `zsur_`.

A surrogate is worth nothing outside this container: only the proxy turns it into a token, only the
agent's uid can reach the proxy, and the agent already has it. Deleting `broker.key` and restarting
the container retires every surrogate at once.

`broker::secrets(cfg, &vault)` returns `Vault::secrets()` plus one `(name, surrogate)` pair per
token and connector secret. `vault::scrubbed` and `gate::Scrub::load` (`gate.rs:466`) use it, so a
surrogate in command output, a reply, a card or an audit row reads `[vault:NAME]`, as the value
does today. The agent therefore sees the same text before and after A3.

### 4.2 Binding: which hosts a token goes to

**Vault tokens.** `[[token]]` in `vault.toml` gains three fields (`vault::Entry`, `read`, `put`):

```toml
[[token]]
name = "GH_TOKEN"
value = "…"
programs = ["gh", "git"]
hosts = ["api.github.com", "github.com", "uploads.github.com"]  # exact; the only places it is swapped in
url = false      # also swap it in the path and query (§4.3)
raw = false      # hand the program the real value, as before A3
```

`broker::parse_host` checks an entry: `host` or `host:port` (443 when absent), normalized by
`egress::target` (WHATWG, lower case, IDNA), a name and never an IP literal, no `*`, no scheme, no
path. Exact means exact: `github.com` does not cover `api.github.com`. A wildcard is how a token
bound to `*.github.io` would reach a page anyone can publish.

**The owner** (`vault::command`):

- `/vault bind NAME HOST..` adds hosts; `/vault url NAME on`; `/vault raw NAME on`. Each is a
  loosening and needs `may_approve`, as `grant` does (a channel in `strong_channels`, or a passkey).
- `/vault unbind NAME [HOST]`, `/vault url NAME off`, `/vault raw NAME off` tighten, from any
  channel, as `revoke` does.
- The TUI's and the web panel's token form gains a hosts field, and `zephyr vault add token NAME
  --host HOST..` on the host; both are the owner's own strong surfaces, so `put` takes `hosts`
  from them. A changed value keeps its hosts. `Entry::public` and `Entry::line` show `hosts`,
  `url` and `raw`.
- Each change is one `settings_log` row (key `vault.<NAME>.hosts`, `.url` or `.raw`, the actor and
  the channel). Host names are not secrets.

**The agent** gets one new `vault` action, `bind {name, hosts, why}` (`VaultAction::Bind` in
zephyr-policy `tools.rs`, checked again in `vault::agent`). It stores the proposal
`/vault bind NAME HOST..`, exactly as `fill` stores a grant, and answers that it waits. The owner's
card puts the hosts first: `send GH_TOKEN to api.github.com, github.com?`. `gateway` still refuses
`/vault`.

**Using a token** (`vault::exec_env`). After the grant check, `broker::value_for` decides what goes
in the environment: the real value when `raw`; the surrogate when `hosts` is not empty; otherwise
the call is answered `not run: the token NAME is bound to no host; ask the owner with vault bind`,
and nothing runs. The `programs` grants stay as they are: they decide which command is first handed
the surrogate, not who may use it afterwards.

**Connector secrets** stay in secrets.env. `[[mcp]]` gains `hosts` (same syntax) and `secrets =
"surrogate" | "plain"`:

- A `url` connector with no `hosts` is bound to its own URL's host. That is the common case and
  needs nothing from the owner.
- A `command` connector must list `hosts`, or say `secrets = "plain"` (a database password, a
  signing key). With neither it is left out at start with a notice, as one missing a secret is
  today (`drafts::prepare`, `drafts.rs:937`).
- A secret named in the connector's `url` is swapped in the request target too; one named only in
  `headers`, `env` or `args` is swapped in headers only.
- A name used by two connectors is bound to both sets of hosts. A draft carries `hosts`, and
  `/mcp ok`'s card shows them, so the agent can propose a connector's hosts but not set them.

`drafts::filled` and `expand` do not change. `prepare` passes them the list from
`broker::connector_secrets(cfg, m, &secrets)`: the surrogate for each name, or the real value for a
`plain` connector. `missing` still reads secrets.env.

**Under host networking** (`!egress::bridge()`) there is no proxy to swap anything, so tokens and
connector secrets are handed out as before A3, and `doctor` says so.

### 4.3 Where a surrogate is swapped

| Place | Swapped | Why |
|---|---|---|
| Any request header value | always | `Authorization: Bearer`, `X-Api-Key` and custom names alike. The surrogate is unique, so the header's name needs no declaring |
| `Authorization: Basic …` | always, after decoding, then encoded again | `git` over HTTPS and `curl -u` send the token as a Basic password |
| Path and query | only with the token's `url = true` | Telegram-style paths, webhook URLs and `?key=`. A server often repeats its URL back (`Link`, `Location`, error text), so it is the owner's choice per token |
| Body | never | below |

Without `url = true`, a surrogate in the target is refused (`403 zephyr: NAME is not swapped in
URLs`) rather than sent on, so the agent learns why the call failed.

**No body swap.** A bound host that stores what it is sent (an issue, a gist, a paste, a chat
message) would store the real token where the agent can read it back: the swap itself becomes the
leak. It would also force the proxy to buffer and re-frame every body, which ends streaming
uploads, and to understand JSON, forms, multipart and compressed bodies. No vendor in the notes
swaps bodies by default. A token an API wants in a body is `raw`. Bodies are not scanned either:
a surrogate in a body to a bound host goes through as text and means nothing there.

**Replies.** In the reply's headers, each real value this request carried is turned back into its
surrogate, so a `Location` or `Link` that repeats the URL does not hand the token over. Reply
bodies stream through untouched; `vault::scrub` on what `exec` prints is the backstop for a body
that repeats it.

### 4.4 The proxy's new path (`egress.rs`, `broker.rs`)

`Conn::answer` (`egress.rs:515`) is unchanged up to the connect: `target`, `decide`, then `connect`
to the checked addresses. Then, for a `CONNECT` whose `host:port` is in `broker::Map::bound`, it
spawns `broker::serve_tls` in place of `tunnel` (`egress.rs:555`):

1. Answer `200`, then accept TLS from the client with the leaf for that host (§4.5), offering ALPN
   `http/1.1` only.
2. Start TLS to the already connected, already checked address, with rustls: SNI is the bound
   host, the certificate must verify for that name against `webpki-roots` (compiled into the
   binary, already in the tree through `reqwest`), ALPN `http/1.1`. If it does not verify, the
   client gets `502 zephyr: HOST did not prove it is HOST` and nothing else is sent.
3. Serve HTTP/1.1 on the client side (`hyper::server::conn::http1`, keep-alive on, 10 s for a
   head, an idle connection closed after 90 s) and send each request on over the one upstream
   connection (`client::conn::http1`, as `forward` does at `egress.rs:742`).
4. Per request: `Host` must be the bound host (with or without its port), or `403 zephyr: Host
   does not match HOST`. Without this, a bound host on a shared CDN would carry the token to
   another customer's origin. Then `broker::swap(&mut request, &map, host)` (pure), which applies
   §4.3 and the refusals of §4.6.

`broker::Map` holds surrogate to (name, value, hosts, url) for every bound token and connector
secret. The proxy keeps one and rebuilds it when `vault.toml`'s mtime, secrets.env's mtime or the
live config changes, checked with one `stat` per CONNECT. If the vault cannot be read, the last
map still decides which hosts are decrypted, and every request on them gets `502 zephyr: the vault
could not be read`. The values live in the daemon's memory only; `Map`'s `Debug` prints names.

`Floor` gains `roots: Arc<rustls::RootCertStore>`, so tests pass their own, as they pass `public`
and `lookup`.

**DNS rebinding** of a bound host has two locks. `decide` resolves once, refuses any private
answer, and the connection is to the checked `SocketAddr` (§4.20). And an attacker who gets a
public address of theirs into the answer still cannot present a certificate for the bound name
under public roots, so step 2 fails before any request is read.

**A bound host that is also in `network.allow`** keeps its direct route (§4.20). A program that
connects directly skips the proxy, sends the surrogate, and is refused by the server. Nothing
leaks; the program has to use the proxy.

### 4.5 The CA (`crates/zephyr/src/ca.rs`, new; `rcgen`)

- **Key.** `<state>/ca/ca.key` and `ca.pem`, in a 0700 folder of root's, the key 0600. `<state>`
  is already closed to the agent (§4.4). ECDSA P-256, `CA:TRUE`, path length 0, valid 398 days,
  subject `zephyr token broker (this container only)`. It is not one of `DAEMON_FILES`: the host
  CLI has no use for it.
- **Rotation happens only at a container start.** `supervise::main`, before the first daemon and
  beside `egress::load_floor`, calls `ca::ensure`: when there is no CA, or it has less than 180
  days left, it makes a new one and deletes the old key. Every process of the agent's starts after
  that, so none ever holds a trust file older than the CA in use; a rotation while things run
  would need an overlap period and restarts. A container up for more than 218 days without a
  restart gets a `doctor` warning from 30 days before expiry (`restart with zephyr up`).
- **Leaves.** One P-256 leaf key per daemon run, in memory. `ca::leaf(host)` signs a certificate
  with that host as its only SAN, valid from an hour ago for 30 days, and keeps the
  `Arc<rustls::ServerConfig>` in a map by host; an entry within a day of expiry is made again.
  The map cannot grow past the number of bound hosts, because nothing else is ever signed.
- **What the agent's tools trust**, all written by root, none in the agent's home:
  - the system bundle: `ca::ensure` writes `/usr/local/share/ca-certificates/zephyr.crt` and runs
    root's `/usr/sbin/update-ca-certificates`, which rebuilds `/etc/ssl/certs` from scratch (so a
    rotated CA is dropped). This covers curl, wget, git, gh and other Go programs, and OpenSSL;
  - `<state>-public/ca.pem` (root's 0755 folder, `drafts::public_dir`), and in `agent::proxy_env`
    (`agent.rs:85`, so the host's `docker exec` as the agent gets them too):
    `NODE_EXTRA_CA_CERTS=<state>-public/ca.pem`, and `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE` =
    `/etc/ssl/certs/ca-certificates.crt`, for Node, and for Python and Rust tools that carry their
    own roots (requests, pip, httpx, uv);
  - Chromium: once per daemon start, `certutil -d sql:/zephyr/.pki/nssdb -A -n zephyr -t C,, -i
    <state>-public/ca.pem`, run **as the agent** through `agent::cmd` (the store is in its home,
    which root never writes; `libnss3-tools` joins the image). A root added there is exempt from
    Chromium's built-in pins;
  - Camoufox: a `policies.json` in the image's Camoufox `distribution/` folder with
    `Certificates.Install` naming `/var/lib/zephyr-public/ca.pem`. To be confirmed by the
    real-container test; if Camoufox ignores it, a bound host shows a certificate error there and
    nothing leaks.
- **The agent adding trust of its own changes nothing that matters.** It cannot write
  `/usr/local/share/ca-certificates`, `/etc/ssl` or `<state>-public`. It can point its own
  programs at any bundle it likes, or run `curl -k`: that only lets it intercept itself. The one
  check that guards a real token is step 2 of §4.4, made by the daemon against roots compiled into
  the binary, which no file or variable of the agent's reaches.
- **The daemon's own clients.** serenity reads the system bundle and so now trusts the CA too.
  Root's traffic does not pass the proxy, and only root holds the key, so nothing follows from it.

**Pinning: what works and what must stay a tunnel.**

| Client | On a bound host |
|---|---|
| curl, wget, git, gh, Go programs | works: system bundle, no pins |
| Node (npm, a Node connector), Python (requests, pip, httpx), uv | works through the variables above |
| Chromium | works through the NSS store |
| `zephyr-cursor`, and rpi's own provider clients | **does not work.** The Cursor plugin is `reqwest` with `rustls-tls` (`crates/zephyr-cursor/Cargo.toml:35`): roots compiled in, no file read. rpi is a Rust binary and is assumed the same until the real-rpi test says otherwise |
| A program with compiled-in roots or real pins | does not work: a certificate error, at once |

So a model provider's host must stay a tunnel, and it does: no provider credential is brokered
(§4.9), so nothing is bound there. If the owner binds a token of their own to a provider's host,
rpi loses that provider; `/vault bind` answers `HOST is now decrypted by zephyr for every program
the agent runs`, and `doctor` lists the decrypted hosts. A pinned client's token is `raw`, or the
host is left unbound. Every failure here is a refused handshake, never a leaked token.

### 4.6 A surrogate on its way anywhere else

The proxy can see a surrogate in three places: a request it decrypted for a bound host, a plain
`http://` request it forwards (`forward`), and the host name of any request. There it looks for
`zsur_`, and for the surrogate's base64 forms (`vault::base64_forms`), and:

- a token not bound to this host: `403 zephyr: the vault token NAME is not sent to HOST`. The
  request is not forwarded in any form. An audit row (§4.8). One notice to the owner per token and
  host per hour, at most 10 an hour in all: `[zephyr] the agent tried to send the vault token
  GH_TOKEN to evil.example, where it is not bound. Nothing was sent.` The notice names no command:
  the answer to an attack should not be one tap from allowing it;
- a `zsur_` that matches no token (a removed one, or made up): `403 zephyr: unknown vault
  surrogate`, an audit row, no notice.

**What it cannot see**: a CONNECT tunnel to an unbound HTTPS host. A surrogate sent there arrives.
That is accepted, and it is why surrogates exist: what arrives is the surrogate, which no one
outside the container can turn into anything. Seeing it would mean decrypting every host, which
breaks every pinned client and reads all of the agent's traffic to stop the theft of a worthless
string.

### 4.7 HTTP/2, websockets, streams, browser history

- **HTTP/2.** Both legs offer `http/1.1` only, so clients fall back, as every HTTPS client and
  server in common use does. One parser, one swap function. gRPC, which needs HTTP/2, fails on a
  bound host; such a token is `raw`.
- **Websockets.** An `Upgrade` request is swapped like any other (the token is in a header or the
  URL). After `101` both sides are upgraded (`hyper::upgrade::on`) and the bytes relayed untouched
  by `egress::relay`, with its 30 minute idle limit. Frames are not read.
- **Streaming bodies.** Request and reply bodies pass through as hyper frames, never collected:
  uploads, downloads and event streams keep their timing. `Floor::reply_wait` and `idle` apply as
  on the forwarded path.
- **Surrogates in URLs and browser history.** The swap is made on the wire, after the browser.
  The address bar, the history, `agent-browser get url` and a `Referer` sent to another site all
  hold the surrogate. A redirect that repeats the URL is turned back in the reply's headers
  (§4.3). A page body that prints the real token is the remaining case, and the reason `url` is
  off unless the owner turns it on.
- **A surrogate in logs or replies** is scrubbed to `[vault:NAME]` (§4.1). The proxy's own
  refusals name the token and never quote the surrogate.

### 4.8 The audit and the gate

- **Audit.** Rows go through the proxy's existing writer (`write_audit`, `egress.rs:630`), session
  `net`, tool `swap`, args `NAME -> host:port`. Outcome `allowed`, once per token and host per
  hour (the map `Audit` already keeps for destinations), or `refused: <reason>` under the same cap
  of 60 a minute. A row never holds a value or a surrogate: the writer scrubs with
  `broker::secrets`, and the row is built from the name. `/activity` (A4) reads these rows as it
  reads `connect` rows, so "GH_TOKEN was used on api.github.com" is one line there. The `connect`
  row for a decrypted host is written as before.
- **The gate.** Nothing new waits on the owner. `policy::gated` and `gate::decide` still judge a
  command before it runs, and an approved held action (`actions.rs:215`) gets its tokens from the
  same `exec_env`, so surrogates. What A3 adds under the gate: a token can now be used only on its
  bound hosts and only through the proxy, so "reach a mail API with curl" (§4.16 "What it does not
  stop") needs a token the owner bound to that API, and each use leaves an audit row by name.
- **Not done:** a class on a token (`messages`, `money`) that would send each swapped request to
  `gate::decide`. The proxy is now the right place for it, but it is an in-flight ask, which A2
  ruled out; D can revisit it for payments.

### 4.9 Provider credentials are out of scope

What the agent can read today, and after A3:

- `<home>/accounts/<name>/auth.json` (`RPI_CODING_AGENT_DIR`, `pool.rs:84`): the folder is 0700
  and the **agent's**, because rpi runs as the agent and writes the file. It holds each OAuth
  provider's `{access, refresh, expires_at}` and any API key saved at login. The policy plugin
  refuses a tool call that names `auth.json`, and DESIGN §7 already says that check can be
  bypassed.
- the Claude config beside it, and every secrets.env variable `[agent] env` lets through, in any
  agent process's environment.

Why A3 leaves them:

1. rpi rewrites them. `rpi auth refresh` (`auth_keeper.rs`) posts the refresh token in a **body**
   and reads the new tokens from a reply **body**. Brokering that means body swaps both ways and a
   token store in the proxy, that is, the daemon doing the OAuth itself.
2. Some providers read their own token (an account id or an expiry inside a JWT). A surrogate
   breaks them.
3. It would decrypt all model traffic, the largest and longest-lived streams there are, through
   clients that do not trust the CA (§4.5).
4. The loss is different in kind: a provider token spends the owner's model quota. It does not
   read their mail or push to their repositories, which is what vault tokens do.

So §7's line stays. `doctor` lists the `[agent] env` names as reaching the agent in plain. An API
key that only a script needs belongs in the vault as a bound token, not in `[agent] env`.

### 4.10 Failure handling and doctor

- **No CA** (`ca::ensure` fails): `supervise` logs it and starts the daemon anyway; the floor does
  not depend on it. `broker::serve_tls` then answers `502 zephyr: the token broker is down` on
  bound hosts, the owner is told once (`[zephyr] the token broker is down, so vault tokens do not
  work: <error>`), and surrogates are still what is handed out. Unbound hosts are unaffected.
- **`broker.key` unreadable**: `exec_env` answers `not run`, and connectors that need a surrogate
  are left out. No real value is used in its place.
- **Vault unreadable**: §4.4. **Proxy down**: as §4.20; no swap happens anywhere else.
- **Upstream certificate fails**: `502`, one audit row, nothing sent.
- **`doctor`** (in the container) adds: the CA's files, owners and modes, and its days left; the
  CA present in `/etc/ssl/certs/ca-certificates.crt`; `ca.key` and `broker.key` not readable as
  the agent (a `zephyr __netcheck` line); no secrets.env value of a surrogate-mode connector
  inside the published `mcp.json`; the decrypted hosts; and one warning each for tokens with
  `raw`, tokens with `url`, connectors with `secrets = "plain"`, tokens bound to nothing, and the
  `[agent] env` names. Under host networking: `token broker off (host networking): vault tokens
  reach the agent in plain`.

### 4.11 Schema and registry

**No schema version.** Bindings live with the secrets they bind (`vault.toml`, `zephyr.toml`),
surrogates are derived, leaves and the notice limiter are in memory, and the audit and
`settings_log` tables are used as they are. `SCHEMA_VERSION` stays 18 and v19 stays free.

**Two registry rows** (`settings.rs`, beside `mcp.<name>.enabled`):

- `row("mcp.<name>.hosts", "Connectors", "Connector hosts", "Exact HOST[:PORT] this connector's
  secrets are sent to, comma separated. No wildcards.", Kind::Text { max: 1000 }, Access::Propose,
  true, Stricter::NotOrdered, Apply::Toml)`, checked by `broker::parse_host`.
- `row("mcp.<name>.secrets", "Connectors", "Connector secrets", "surrogate: the connector holds
  stand-ins and zephyr sends the real secret only to its hosts. plain: it holds the real ones.",
  Kind::Choice(&["surrogate", "plain"]), Access::Propose, true, Stricter::Earlier, Apply::Toml)`.

Both are strong, so the agent proposes and the owner approves on a strong channel. Vault tokens
have no rows: their bindings sit beside their grants in `vault.toml`, changed only by `/vault`.

### 4.12 Upgrade

Nothing is rewritten and no value moves.

- **Vault tokens.** An existing `[[token]]` has no `hosts`, so its next use is answered `not run`
  (§4.2) until the owner binds it or marks it `raw`. Grants are kept. The first daemon start that
  finds such tokens tells the owner once, naming each: `[zephyr] vault tokens now go to the agent
  as stand-ins, swapped for the real token only on hosts you name. Bind each: /vault bind GH_TOKEN
  api.github.com github.com. To hand one out as before: /vault raw NAME on.` (Open question 1.)
- **Connectors.** A `url` connector is bound to its URL's host with no edit; its `mcp.json` entry
  simply changes from the value to the surrogate at the next child start. A `command` connector
  with a `${NAME}` and no `hosts` is left out, with the same notice naming `/set mcp.NAME.hosts`
  and `/set mcp.NAME.secrets plain`.
- **Running commands** started before the upgrade hold real values until they exit; the upgrade
  restarts the container, so there are none.
- **Host networking**: nothing changes (§4.2).
- `zephyr init` and `zephyr vault add token` ask for hosts.

## Open questions for the owner

1. **Existing tokens and `command` connectors with no hosts, at upgrade.**
   (a) Refused until bound or marked raw/plain, with one notice listing the commands.
   (b) Treated as raw/plain until the owner binds them, with a `doctor` warning.
   Recommend (a): it matches A2's "safe by default, one line goes back", and under (b) A3 protects
   nothing until the owner happens to act.
2. **Body swaps.** (a) Never; such tokens are `raw`. (b) A per-token `body = true`, bodies up to
   64 KB, JSON and form bodies only, buffered. Recommend (a): the bound host storing what it is
   sent is the one way a swap leaks the token, and (b) is the only part of this design that needs
   buffering. Add it when a concrete API needs it.
3. **Provider credentials stay readable** (`auth.json`, `[agent] env`). (a) Out of scope, as §4.9.
   (b) A later part brokers API-key providers only, once rpi is shown to trust the CA.
   Recommend (a) now, and (b) only if the real-rpi test in "Tests" passes.
4. **CA life.** (a) 398 days, renewed at a container start with under 180 days left. (b) One CA
   for 10 years, never rotated. Recommend (a): no rotation ever happens under running processes,
   and a copied key is useful for at most a year.
5. **Tokens for hosts with a private CA or plain HTTP** (a LAN Home Assistant). (a) `raw` only.
   (b) An owner-supplied `<state>/ca/extra-roots.pem` the proxy also trusts upstream.
   Recommend (a): (b) weakens the one check that guards every swap, for one kind of host.

## Tests

Each piece starts with a failing test (CLAUDE.md, DESIGN §6).

- **`broker` unit tests** (pure): `surrogate` is stable for a name, differs by name, kind and key,
  and matches `^zsur_[0-9a-f]{40}$`; `parse_host` takes `api.github.com` and `x.io:8443` and
  refuses `*.x.io`, `https://x.io`, `x.io/path`, `1.2.3.4` and `x.io:0`; `swap` over a table:
  Bearer, a custom header, Basic (decoded and encoded again), two tokens in one request, the
  target with and without `url`, a body left alone, a reply's `Location` turned back; `Map`'s
  `Debug` and every refusal text hold no value and no surrogate; `secrets` makes `vault::scrub`
  turn a surrogate, and its base64 form, into `[vault:NAME]`.
- **`ca` unit tests**: `ensure` makes the folder 0700 and the key 0600, keeps a CA with 200 days
  left and replaces one with 100; a leaf verifies under the CA for its host and no other; the
  leaf map returns the same config twice and a new one a day before expiry.
- **`vault`**: `exec_env` gives a surrogate for a bound token, the value for a `raw` one, and
  `not run` for one bound to nothing; `put` keeps `hosts` across a new value; `command` refuses
  `bind`, `raw on` and `url on` without `may_approve` and takes `unbind` and `off` without; the
  agent's `bind` stores a proposal and `/ok N` applies it; each change is a `settings_log` row.
- **`drafts`**: `mcp.json` for a `url` connector holds the surrogate and never the secrets.env
  value; a `command` connector without `hosts` is left out; with `secrets = "plain"` it holds the
  value; the file is byte-identical across two `prepare` calls.
- **Proxy tests** (tokio, in `egress.rs`, with `Floor::roots` a test CA and `lookup` passed in):
  - a request to a bound host reaches the upstream with the real token in `Authorization`, and
    the upstream never sees `zsur_`; an unbound host on the same proxy is tunnelled and its
    certificate is the upstream's own;
  - keep-alive (two requests, one connection), a chunked upload and an event stream arrive in
    order and unbuffered; a websocket handshake is swapped and frames pass both ways; a client
    offering `h2` is given `http/1.1`;
  - audit: one `swap` row `allowed` per token and host per hour, by name.
- **Attacks:**
  - *A surrogate sent to an attacker host.* (1) Plain `http://attacker/` with the surrogate in a
    header, then in the query, then base64 in `Authorization: Basic`: `403`, the attacker's
    listener accepts no connection, one `refused` audit row naming the token, one notice, and a
    second attempt within the hour adds no notice. (2) A host bound for token A, sent token B's
    surrogate: `403`, the upstream reads no request. (3) A bound host with `Host: attacker` on
    the decrypted connection: `403`. (4) A CONNECT tunnel to an unbound TLS host: the bytes the
    attacker reads contain the surrogate and never the value; this test documents §4.6's limit.
  - *An agent-planted CA.* The upstream for a bound host presents a certificate from a CA that is
    not in `Floor::roots` but is in the test's `SSL_CERT_FILE` and system-style bundle: the
    handshake fails, the client gets `502`, and the upstream reads no request. In the real
    container, as the agent: `ca.key` and `broker.key` cannot be read, and writes to
    `/usr/local/share/ca-certificates`, `/etc/ssl/certs` and `<state>-public/ca.pem` fail.
  - *DNS rebinding of a bound host.* A `lookup` that answers a public address first and
    `127.0.0.1` second: refused by `decide`, as in A2. A `lookup` that answers the attacker's
    address, where a server holds a valid certificate for another name: step 2 of §4.4 fails and
    no request is read. A counter in `lookup` shows one resolution per connection.
- **`tests/` with `fake-rpi.py`**: an `exec` with `secrets` whose command prints its environment
  answers `[vault:NAME]`, and the `.procs` log on disk holds the surrogate, not the value; a
  token bound to nothing gives `not run`; the upgrade notice is sent once.
- **`agent::cmd`**: in bridge mode the three CA variables are in `get_envs()`; in host mode none.
- **supervise**: `ca::ensure` failing does not stop the daemon from starting.
- **Ignored real-container test** (`tests/token_broker.rs -- --ignored`, beside a live install
  through `ZEPHYR_CONTAINER`/`ZEPHYR_IMAGE`, against a local TLS echo server listed in
  `network.allow` whose root is given to the proxy for the test): as the agent, `env` under an
  `exec` with `secrets` shows `zsur_…` before scrubbing; `curl`, `git ls-remote`, `gh api`,
  `python -c 'import requests…'`, `node -e 'fetch…'`, `uv`, agent-browser and Camoufox each reach
  the bound host and the server sees the real token; the same commands against an unbound host
  see its own certificate; `doctor` passes and lists the host.
- **Ignored real-rpi test** (`tests/real_rpi.rs`): with a token bound to the test provider's
  host, does rpi's own client still connect? The result goes into §4.5's table and decides open
  question 3(b).
- **Manual, with the pinned rpi**: `gh pr list` and a `git push` over HTTPS with a bound
  `GH_TOKEN`; one remote connector (`url` and a `${NAME}` header) answering a call after the
  upgrade with no edit to its `[[mcp]]`.
