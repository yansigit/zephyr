# Network floor: the agent reaches the internet only through the daemon's proxy

Status: design drafted 2026-11-09; awaiting the owner's review.
Sub-project **A2** of the Muse-parity roadmap (the table is in
`2026-11-07-settings-registry-design.md` §1). It depends on nothing. It uses S only for one
registry row (`network.allow`). A3 (token broker) builds on the proxy this spec adds.

## 1. Why

The container runs `--network host` (`host/mod.rs` `up`, lines 334-335; DESIGN §4.4 item 1,
"Network"). Every process the agent starts shares the host's network namespace, so today the agent
can reach:

- every service on the host's loopback, including zephyr's own attach port (`attach.rs:110`) and
  the iMessage webhook (`imessage.rs:463`). Both cap connections at 32 only because the agent can
  reach them (§4.3);
- the LAN, the tailnet (100.64.0.0/10, including the BlueBubbles Mac's API), cloud metadata
  at 169.254.169.254, and the host's abstract unix sockets, X99 among them (§4.11 relies on the
  X cookie for exactly this);
- any raw protocol: SMTP, IMAP, SSH, databases, DNS to any resolver.

§4.16 "What it does not stop" leaves "reach a mail API with curl" to this sub-project. The Muse audit
(`reports/Muse agent audit.md`, "A2 and A3") and its notes
(`research_notes/Muse agent audit/memory_tokens_network.md`, "Network egress") recommend closing the
route rather than listing hostnames: no direct route out for the agent's uid, one proxy that checks
the address it is about to connect to and then connects to that address, and Deny for raw protocols.

What A2 does **not** change: §4.4 item 3's finding still holds. Leaks mostly go out in normal
replies, and A2 still allows HTTPS to any public host, including a mail API, as long as the agent
holds a token for it. Taking those tokens away is A3's job (surrogates swapped at this proxy). A2
removes everything else: loopback, private ranges, metadata, the tailnet, and raw protocols.

## 2. Requirements

- Every process running as the agent's uid (rpi children, `exec`/`process` commands, tasks, jobs,
  `rpi auth *`, the browser, Xvfb/openbox, connectors, `docker exec -u <agent>` shells) has no
  direct network access. It reaches only its own loopback, and the proxy through it.
- The proxy, run by the daemon, allows TCP to **public** addresses on ports 80 and 443. It denies
  loopback, RFC 1918, 100.64.0.0/10, link-local (169.254.0.0/16, fe80::/10), ULA, multicast,
  documentation ranges, and every IPv6 form that embeds an IPv4 address (mapped, compatible,
  NAT64, 6to4, Teredo). It connects only to an address it has checked.
- Any other port is denied unless the owner lists that exact `host:port` in `network.allow`. The
  agent may propose an entry but never set one.
- The agent gets no DNS. Names are resolved only by the proxy.
- The daemon's own traffic is unchanged: Discord, BlueBubbles over the tailnet, inbox downloads.
  The attach port still works for the owner on the host's `127.0.0.1`, and the webhook still works
  on the Tailscale IP. The agent can reach neither.
- Fail closed: if the rules cannot be loaded, the agent's processes never start. While the proxy is
  down, the agent has no network.
- Every refusal goes to the audit table. Allowed traffic is recorded once per destination per hour.
- `doctor` proves the floor by probing as the agent.
- Existing installs move to the floor at their next `zephyr up` or `upgrade`. A single host.toml
  line (`network = "host"`) brings back today's networking.

### Non-goals

- Stopping exfiltration to a public HTTPS host, domain fronting, SNI/Host matching and TLS
  interception. These belong to A3, which terminates TLS at this proxy.
- Host-side rules (`DOCKER-USER` or a host nftables forward chain). The host sees the daemon and the
  agent as one container address after NAT, so dropping 100.64/10 there would also cut BlueBubbles.
  Such rules also need host root and a unit to survive a reboot. The in-container uid rule plus the
  proxy is the floor. Host rules as defence against a compromised root in the container can come
  later.
- The host's own public address on a VPS. Services listening there on `0.0.0.0` are already exposed
  to the internet, and the host's INPUT firewall applies to them. Loopback-only services become
  unreachable, because `route_localnet` is never set. See the open questions.
- An in-flight Ask, where a CONNECT waits for the owner's `/ok`. Clients time out first. Ask is
  done instead as a proposal to change `network.allow`.
- Attributing a connection to a session or run. Rows say `net`, not `main` or `task-7`.
- Byte counts, per-host rate limits, SOCKS, PAC, HTTP/2 to the proxy, IPv6 egress for the
  container (the network is created without `--ipv6`), and NAT64 (always denied).
- A host-side relay for OAuth callbacks. Pasting the URL (§4.7) covers both the host and channels.
- Configuring SSH for the agent. Once the owner lists `github.com:22`, the agent can write its own
  `ProxyCommand`.

## 3. What changes in DESIGN.md

- §4.3 TUI bullet (line 122): under the floor, the agent cannot reach the attach port or the
  webhook. The caps stay for other users on the host. The daemon binds both on the container's own
  address, and in-container clients connect there.
- §4.4 item 1 "Network" (line 302): replace with the bridge network, the published ports, and the
  `NET_ADMIN` capability that `supervise` uses once and then drops. Point to the new §4.19.
- §4.4 item 3 (line 325): "General egress is not blocked either" becomes: the agent's egress goes
  only through the proxy, to public ports 80 and 443 plus `network.allow` (§4.19). The ClawSafety
  reasoning and the after-turn outbound list stay.
- §4.7.1 `oauth` (line 382): the callback no longer reaches the container. The owner pastes the
  address the browser could not open, and rpi reads it.
- §4.8 `doctor` (line 432): the network-floor checks in §4.9 below.
- §4.11 Desktop (line 472): the container has its own network namespace, so the abstract socket
  `@/tmp/.X11-unix/X99` is no longer the host's. The cookie stays. The x11vnc recipe listens on
  the container address.
- §4.16 "What it does not stop" (line 695): SMTP, IMAP and other non-HTTP protocols are closed by
  §4.19. HTTPS to a mail API with a token the agent holds is still open (A3).
- §7: add the host's own public address (non-goal above).
- New §4.19 Network floor (this spec, once built). README: `network` in host.toml, `network.allow`,
  the OAuth paste, x11vnc, and Tailscale-before-Docker boot ordering. CLAUDE.md: an `egress.rs`
  line in the architecture list.

## 4. Design

### 4.1 Choice: bridge network + uid rule + proxy in the daemon

| | (a) bridge + nftables `meta skuid` + CONNECT proxy | (b) separate netns for agent processes, proxy over a unix socket |
|---|---|---|
| Capability | `NET_ADMIN` once, in `supervise`, then dropped from the bounding set before the daemon runs | `CAP_SYS_ADMIN` kept by the daemon for every spawn (`unshare`/`setns`; Docker's seccomp profile blocks both without it), which undoes §4.4's hardening (the reason systemd was rejected) |
| Spawn sites | none change: `agent::cmd` adds env only | every `agent::cmd` caller enters the netns in `pre_exec`, along with procs' pidfd adoption, process groups and the login pty |
| Proxy reach | tools speak TCP proxies, so 127.0.0.1 works directly | needs a TCP-to-unix relay process inside the agent's netns |
| OAuth callback | the daemon (root) shares the netns. A pasted URL works (§4.7) | the login listener sits in the agent's netns, out of the daemon's reach |
| X11 | fixed for free: abstract sockets belong to the container's netns | Xvfb has to move into the agent's netns |
| Daemon listeners | moved to the container's address and published | unchanged (host netns) |

**(a) is chosen.** It needs the smaller capability, held for one step only, and changes no spawn
site. The proxy and its policy are the same in both options. Anthropic's `sandbox-runtime` uses (b)
because it has no root daemon sharing the namespace. Here the daemon must keep a direct route
(Discord, the tailnet), and a rule on the agent's uid gives exactly that split. The (a)
pattern is the AWS `--uid-owner` recipe the notes cite.

Under (a), the agent's uid is not root, runs with `no-new-privileges`, and has no capabilities.
Docker's default seccomp profile refuses it user namespaces, so it cannot load rules or enter
another netns. Sockets it creates carry its uid, including ICMP ping sockets and io_uring sockets.

### 4.2 The container (`host/mod.rs` `up`)

`host.toml` gains `network = "bridge" | "host"`. The default is `bridge`, read with
`self.data.host_get("network")` beside `memory`. In `host` mode, `up` passes exactly today's
arguments. In `bridge` mode it replaces `"--network", "host"` (lines 334-335) with:

```
--network <container>            # a user-defined bridge named after the container (ZEPHYR_CONTAINER-safe)
--hostname zephyr                # Docker writes "<container ip> zephyr" to /etc/hosts
--cap-add NET_ADMIN              # supervise loads the rules, then drops it (§4.3)
-e ZEPHYR_NETWORK=bridge
-p 127.0.0.1:<attach.port>:<attach.port>              # when [attach] is set
-p <imessage.listen ip>:<port>:<port>                 # when [imessage] is set, e.g. 100.64.0.1:7791:7791
```

Before `docker run`, `up` checks for the network with `docker network inspect <container>` and, if
it is missing, creates it with `docker network create --driver bridge <container>`. There is no
`--ipv6` and no fixed subnet. `uninstall` runs `docker network rm <container>`. The ports come from
`conf::get(&doc, "attach.port")` (as `attach` does at line 581) and `conf::get(&doc,
"imessage.listen")`. `check_listen` (`imessage.rs:94`) still refuses an unspecified address. If the
Tailscale IP is missing on the host, `docker run` fails, and `up`'s existing "the old one comes
back" path restores the old container with Docker's error.

Changing `attach.port` or `imessage.listen` now means recreating the container. `zephyr config set`
on those two keys runs `up` instead of `apply`. A daemon that sees either key change on its tick
tells the owner once: `[zephyr] attach.port changed; run zephyr up on the host to publish it`.

### 4.3 The rule set (`egress::ruleset`, loaded by `supervise`)

`supervise::main` (`supervise.rs:64`) runs this before it spawns the first daemon, when
`ZEPHYR_NETWORK=bridge`. It pipes `egress::ruleset(agent_uid)` to `/usr/sbin/nft -f -` (the
`nftables` package is added to the runtime `apt-get install` in `deploy/Dockerfile:159`). Then it
calls `prctl(PR_CAPBSET_DROP, CAP_NET_ADMIN)`. A root `execve` gets the bounding set as its
permitted set, so the daemon and everything it starts lack `NET_ADMIN`. `docker exec` processes,
which are the owner's, still have it. The text, with the uid filled in:

```
table inet zephyr
delete table inet zephyr
table inet zephyr {
  chain output {
    type filter hook output priority filter; policy accept;
    meta skuid 1000 jump agent
  }
  chain agent {
    ct state established accept           # replies on connections made to the agent (x11vnc, a dev server the owner opens)
    ip daddr 127.0.0.11 reject            # Docker's embedded DNS: the agent gets no resolver
    ip daddr 127.0.0.0/8 accept           # its own servers, and the proxy at 127.0.0.1:7792
    ip6 daddr ::1 accept
    meta l4proto tcp reject with tcp reset
    reject with icmpx admin-prohibited
  }
}
```

Why the rules look like this:

- The rule jumps on the agent's uid and does not accept on "not the agent". A packet with no socket
  (a kernel RST, a TIME_WAIT reply) has no `skuid`, so it falls through to `policy accept` instead
  of being rejected.
- TCP is rejected with a reset, so `connect` fails at once with `ECONNREFUSED`. A dropped TCP
  packet in OUTPUT only makes the SYN retransmit until it times out.
- No port list. The daemon's TCP listeners are on the container's address (§4.6), not on loopback,
  so the only root listener on 127.0.0.0/8 is the proxy. `doctor` enforces this (§4.9).
- `route_localnet` is never touched. Nothing is DNATed to loopback.
- The table is replaced atomically. Every container start gets a fresh netns, so the rules load
  once per start. A daemon crash, re-exec or `supervise` restart of the daemon leaves them in place.

If the rules fail to load, `supervise` prints `network floor could not be loaded: <nft's error>;
network = "host" in host.toml runs without it` and exits 1, without starting the daemon. Docker's
restart policy retries, the HEALTHCHECK fails, and `zephyr alert` fires. Before it starts any agent
process, the daemon also checks the rules in practice (`egress::verify`, §4.9 probe a): a
direct-connect probe run as the agent must fail with `ECONNREFUSED` within 2 s, or `run` exits
non-zero.

### 4.4 The proxy (`crates/zephyr/src/egress.rs`, new)

`egress::serve(cfgs)` runs in the router's `select!` beside `gate::serve` and `attach::serve`
(`router.rs:296-297`), only when `ZEPHYR_NETWORK=bridge`. It binds `127.0.0.1:7792`
(`egress::PORT`). It uses hyper 1 with `http1::Builder::with_upgrades`, which is already a
dependency for the webhook. The forwarding path adds hyper's `client` feature, which is a feature
flag and not a new crate. The shape is hyper's own `http_proxy` example. It allows 256 connections
at once (a semaphore, as `attach.rs` does) and 10 s to send the head (`header_read_timeout`, as
`imessage.rs:313` does). A full proxy refuses new connections at once. Only the agent's own egress
can be exhausted this way. The daemon's channels do not use the proxy.

**Requests.** `CONNECT host:port` tunnels the connection after `200` (`hyper::upgrade::on`, then
`tokio::io::copy_bidirectional`). An absolute-form `http://host[:port]/path` request is sent on in
origin form over a fresh `client::conn::http1` connection. `Proxy-Authorization` and
`Proxy-Connection` are removed, and `Host` is set from the URI if it is missing. Any other form, any
other scheme (`https://` in absolute form, `ftp://`), a CONNECT without a port, or userinfo gets
`400`. Redirects are not followed. A client that follows one sends a new request, and that request
is checked the same way.

**Destination check** (`egress::decide`, pure, no I/O but the lookup):

1. Normalize the host with `url::Url::parse("http://" + authority)`. The `url` crate is already a
   dependency. It applies WHATWG host parsing, so `2130706433`, `0x7f.1`, `0177.0.0.1` and `127.1`
   all become `Host::Ipv4(127.0.0.1)`, and IDNA is applied.
2. Port: 80 or 443, or exactly `host:port` in `network.allow`. Otherwise refuse with
   `port <p> needs network.allow`.
3. Addresses: an IP literal is used as is. A name goes through `tokio::net::lookup_host((host,
   port))` with a 5 s timeout, using root's resolver (Docker's 127.0.0.11, then the host's). An
   empty answer gives `502`.
4. Every address must pass `egress::public`. A name listed in `network.allow` may instead pass
   `egress::private_ok` (10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7). No entry can ever
   permit loopback, link-local, unspecified, multicast, broadcast or the container's own address
   (the `zephyr` entry in `/etc/hosts`). **If any address fails, the whole request is refused**,
   so a DNS answer mixing public and private addresses cannot slip through.
5. Connect to those checked `SocketAddr`s in order, 10 s each. `connect` takes `&[SocketAddr]` and
   never a name, so nothing is resolved twice. That closes DNS rebinding between the check and the
   connect.

`egress::public(ip)` is an allowlist for IPv6 and a denylist for IPv4. Both are tables in the
source, each line citing the IANA special-purpose registry:

- IPv4 is public unless it is in 0/8, 10/8, 100.64/10, 127/8, 169.254/16, 172.16/12, 192.0.0/24,
  192.0.2/24, 192.88.99/24, 192.168/16, 198.18/15, 198.51.100/24, 203.0.113/24, 224/4 or 240/4
  (which includes 255.255.255.255).
- IPv6 is public only inside 2000::/3, and not in 2001::/23 (which includes Teredo 2001::/32),
  2001:db8::/32, 2002::/16 (6to4) or 3fff::/20. Every form that embeds an IPv4 address falls
  outside this allowlist with no unwrapping step that could be got wrong: `::ffff:7f00:1`,
  `::7f00:1`, `64:ff9b::a9fe:a9fe`, `64:ff9b:1::/48`. So do `fea0::1` (fe80::/10),
  `fd00:ec2::254` and `ff02::1`.

Refusals answer `403` with the reason in the status line (`403 zephyr: private address
10.0.0.5`). The agent reads that in curl's `-v` output or a library's error. The body adds: `the
owner can allow HOST:PORT in network.allow; propose it with gateway /set network.allow`.

**`network.allow`** is one registry row, shaped like the `facts` row (`settings.rs:359`):
`row("network.allow", "Security", "Network exceptions", "Exact HOST:PORT the agent may reach
beyond public ports 80 and 443, comma separated. No wildcards.", Kind::Text { max: 2000 },
Access::Propose, true, Stricter::NotOrdered, Apply::Toml)`. The value is checked by
`egress::parse_allow`: each entry is `host:port` with a port from 1 to 65535, no `*`, no scheme,
and no loopback or link-local literal. The proxy reads the live config (`cfgs.borrow()`) on each
request, so a change applies to the next connection. Because the row is `Propose` and strong, the
agent can only propose a change, the owner approves it with `/ok N` from a strong channel, and
`settings_log` records it. Muse's "Deny, and only Ask, never Allow" rule thus works through S's
existing proposal path. Typical entries are `imap.gmail.com:993`, `smtp.gmail.com:465`,
`github.com:22` and a LAN Home Assistant at `homeassistant.lan:8123`.

### 4.5 What the agent's processes get

`agent::cmd` (`agent.rs:59-70`) adds the following when `ZEPHYR_NETWORK=bridge`:
`HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy` = `http://127.0.0.1:7792`; `NO_PROXY`,
`no_proxy` = `localhost,127.0.0.1,::1`; and `NODE_USE_ENV_PROXY=1`, because Node 24's `fetch`
ignores the variables without it. `ALL_PROXY` is not set: rpi 0.18 refuses any proxy URL that is
not HTTP. It also appends `,--proxy-server=http://127.0.0.1:7792` to `AGENT_BROWSER_ARGS`
(`Dockerfile:207`), so Chromium does not depend on its desktop-environment detection. This cannot
go in the image's `ENV`, which also serves `host` mode, where no proxy runs. Camoufox (Firefox)
reads the variables through its default "system" proxy type, and the real-container test checks
this. The constants live as one `agent::PROXY_ENV` slice. `Host::exec_agent` (`host/mod.rs:164`)
passes the same slice as `-e` pairs, as it does `AGENT_PATH`, when host.toml says `bridge`.

Every agent spawn already goes through `agent::cmd`: rpi children (`rpc.rs:288`), `procs::for_agent`
(`procs.rs:357`, which covers `exec`, approved actions and the vault's fill browser at
`vault.rs:887`), the desktop (`desktop.rs:124`), logins (`auth.rs:839`), and memory's git. The
daemon's own clients (serenity, BlueBubbles' and the inbox's `reqwest`) run as root with no proxy
variables and connect directly, as they do today.

What stops working for the agent, and fails at once instead of leaking: tools that ignore
`*_PROXY`, raw sockets, `ssh`/`scp`/`rsync` over ssh, SMTP/IMAP clients (`himalaya`) until
listed, `dig`, `ping`, and any name lookup by the agent. `git`, `curl`, `wget`, `pip`, `uv`, `npm`,
`cargo`, `gh`, rpi itself, and the zephyr-policy and Cursor plugins (`reqwest` honours the
variables) all work through the proxy. Model servers on the host's loopback stop working. The owner
binds them on the bridge gateway and lists that `host:port`.

### 4.6 Listeners and in-container clients

In bridge mode, the daemon's TCP listeners bind the container's address, `egress::own_ip()`. That
is the `zephyr` entry of `/etc/hosts`, through `ToSocketAddrs`, and it must be IPv4 and not
loopback, or the daemon fails at start.

- `attach::serve` (`attach.rs:110`): `TcpListener::bind((own_ip, a.port))` instead of
  `("127.0.0.1", a.port)`. Docker's publish on the host's `127.0.0.1:<port>` reaches it. The agent
  is refused by the rule set's last line, and nobody listens on the container's loopback.
- `IMessage::run` (`imessage.rs:463`): binds `(own_ip, self.listen.port())`. `listen` keeps its
  meaning, the host-side Tailscale address where Docker publishes. BlueBubbles' webhook URL does
  not change.
- `zephyr tui` (`tui/mod.rs:82`) connects to `zephyr:<port>`, and stock `zephyr attach`
  (`host/mod.rs:586`) passes `<ip>:<port>`. On the host, the IP comes from `docker inspect -f
  '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' <container>`. Both run as root inside
  the container, and root's traffic is not filtered.
- Unix sockets (the chat socket, `gate.sock`) are unaffected.
- In `host` mode, all of these keep `127.0.0.1` and `listen` exactly as today.

### 4.7 Sign-in callbacks and the desktop

**OAuth.** The provider's `http://127.0.0.1:<port>/…` redirect now fails in the owner's browser
("can't connect"). rpi 0.18's login already prompts `Paste OAuth code or response`. The owner
pastes that address, with the code in it, and rpi completes the sign-in. The daemon's logins get
one change: `auth.rs:523` sets `Phase::NeedInput` for `oauth` too, so `zephyr.login_input` and a
channel's `/login` reply reach the child's stdin. The pasted text is handled like a key: it is
kept only to redact the pty's echo, and it is never logged or stored (`auth.rs:359-381`). The host
`zephyr login` is already interactive (`host/setup.rs:430`, `-it`), so the owner pastes into rpi's
prompt directly. `login` prints one hint before it starts: `if the browser then cannot open
127.0.0.1, paste its address here`. The token exchange that follows goes through the proxy, since
the login runs as the agent.

**X11.** The container now has its own netns, and abstract unix sockets are scoped to it, so the
host no longer sees `@/tmp/.X11-unix/X99`. The cookie (§4.11) stays for the root processes in the
container. The README's x11vnc recipe changes from `-localhost` to `-listen <container ip>`, and
the viewer connects to that IP from the host. x11vnc runs as the agent, and its replies go out
through the rule set's `ct state established` rule.

### 4.8 Audit

Rows go to the `audit` table through `gate::audit_row` (`gate.rs:609`, made `pub(crate)`):
session `net`, tool `connect` or `http`, args `host:port -> ip` with fact and vault values
scrubbed (`facts::scrub`; a hostname can carry one) and cut to 500 bytes. Outcome is one of the
following:

- `refused: <reason>`: every refusal. At most 60 such rows a minute. Past that, one
  `refused: N more` row closes the minute, so a looping agent cannot flood state.db.
- `allowed`: the first connection to each `host:port` in each hour. This is an in-memory map in
  the proxy, cleared when the daemon starts. A browser's hundreds of connections a page become a
  list of destinations, which is what A4 `/activity` will read.

Retention keeps its 90-day prune of state.db.

### 4.9 doctor

The in-container `doctor` (`main.rs:391`) adds the following, run in bridge mode:

- **Probe as the agent**: `zephyr __netcheck`, a hidden subcommand dispatched in `main.rs` beside
  `__file` (line 61) and run through `agent::cmd`. It prints one line per check:
  a. a direct TCP connect to `1.1.1.1:443` must fail with `ECONNREFUSED` within 2 s. A connection,
     a timeout or `ENETUNREACH` all mean the rules are missing. `egress::verify` at daemon start
     runs this same check;
  b. a direct TCP connect to `own_ip:<attach.port>` must be refused;
  c. resolving `example.com` must fail;
  d. through the proxy, `CONNECT 169.254.169.254:80` and `CONNECT [::ffff:7f00:1]:443` must get
     `403`. `CONNECT example.com:443` should get `200`. If it does not, that is a warning (the
     host may be offline), not a failure.
- **Root listeners**: parse `/proc/net/{tcp,tcp6,udp,udp6}` (local address, state, uid). Any uid-0
  TCP socket in LISTEN, or bound UDP socket, on loopback or a wildcard address fails, except
  `127.0.0.1:7792` (the proxy) and `127.0.0.11` (Docker's DNS). It fails with `root listens on
  ADDR, where the agent can reach it`. This keeps the rule set free of port lists.
- In `host` mode it prints one warning instead: `network floor off (host networking): the agent can
  reach the host's loopback and private ranges`.

The host `doctor` (`host/mod.rs:622`) reads `docker inspect -f '{{.HostConfig.NetworkMode}}'` and
warns when it says `host` but host.toml does not. That means the container predates this version,
and `zephyr up` moves it.

### 4.10 Failure handling

- **Rules not loaded**: `supervise` exits without starting the daemon (§4.3), and `egress::verify`
  stops a daemon whose probe connects. The agent never runs without the floor in bridge mode.
- **Proxy down** (failed bind, daemon crash, re-exec): the agent's new connections are refused,
  because nothing else listens on 7792 for it. Open tunnels close with the daemon. Background
  `exec` downloads fail and are reported as any failed command is. If the bind fails, the owner is
  told once, as the gate's failure is (`gate.rs:1068`): `[zephyr] the network proxy is down, so
  the agent has no internet: <error>`.
- **The agent binds 127.0.0.1:7792 itself** while the daemon is down: its own processes then talk
  to its own fake proxy, which has no route out either. Nothing is gained.
- **Lookup fails or times out**: `502`, plus an audit row.
- **Tailscale IP absent when Docker starts** (at boot, if tailscaled starts after dockerd): the
  publish fails, so the container does not start, and `zephyr alert` reports it. The README gives
  the fix (`systemctl edit docker`: `After=tailscaled.service`). See the open questions.
- **Allow entry resolving to loopback or link-local**: refused with `loopback is never allowed`,
  even though the entry is listed.

### 4.11 Migration

- `host.toml` with no `network` key means `bridge`, so `zephyr upgrade` or `up` moves an existing
  install. Before it removes the old container, `up` reads its `HostConfig.NetworkMode`. If that
  was `host`, `up` prints once: `the agent now reaches the internet only through zephyr's proxy:
  ports 80 and 443 to public addresses (DESIGN §4.19). Other destinations: /set network.allow
  HOST:PORT. Sign-ins: paste the address the browser cannot open. To go back: network = "host" in
  host.toml, then zephyr up.`
- Nothing in the agent's home or in state.db changes, and no schema migration is needed.
  `network.allow` is absent, which means empty.
- `zephyr init` writes `network = "bridge"` with a comment for new installs.

## 5. Testing

Each piece starts with a failing test (CLAUDE.md, DESIGN §6).

- **`egress` unit tests** (pure): `public` over an adversarial table. Denied: `127.0.0.1`,
  `127.1`, `2130706433`, `0x7f000001`, `0177.0.0.1`, `0.0.0.0`, `10.1.2.3`, `100.100.100.100`,
  `169.254.169.254`, `172.17.0.1`, `192.168.1.1`, `192.0.0.170`, `198.18.0.1`, `224.0.0.1`,
  `255.255.255.255`, `::`, `::1`, `::ffff:7f00:1`, `::ffff:127.0.0.1`, `::7f00:1`, `fea0::1`,
  `fe80::1`, `fd00:ec2::254`, `64:ff9b::a9fe:a9fe`, `64:ff9b:1::1`, `2002:7f00:1::`,
  `2001:0:4136:e378::`, `2001:db8::1`, `ff02::1`. Public: `1.1.1.1`, `8.8.8.8`,
  `2606:4700::1111`. Also: host normalization through `url` (the numeric forms in CONNECT and in
  absolute form); `decide` (the port rule, exact `network.allow` match, `private_ok` only for listed
  names, loopback and own IP refused even when listed); `parse_allow` rejecting `*.x:443`, `x`,
  `x:0` and `http://x:1`; `ruleset(1000)` against a golden text; the `/proc/net/tcp` parser on
  fixture lines.
- **Proxy tests** (tokio, in `egress.rs`'s test module, with the address predicate passed in so
  127.0.0.1 counts as public there): a CONNECT tunnel carries bytes both ways; an absolute-form GET
  arrives in origin form without `Proxy-*` headers; `localhost` (both `127.0.0.1` and `::1`) with a
  predicate that passes only the first is refused, because mixed answers are refused; port 22 is
  refused without an allow entry and passed with one; the 257th connection is refused at once; a
  head that never finishes is closed at 10 s; audit rows (one `allowed` per host and hour, every
  refusal, the 61st refusal in a minute folded into `refused: N more`, a fact value in a hostname
  scrubbed).
- **`agent::cmd`**: with `ZEPHYR_NETWORK=bridge`, the proxy variables and the extended
  `AGENT_BROWSER_ARGS` are in `get_envs()`. Without it, none are.
- **supervise**: a failing floor loader, passed in through `Policy` as `heartbeat` is, exits 1 and
  never spawns the daemon.
- **`tests/login.rs`**: a line typed into a waiting OAuth login reaches fake rpi's stdin and is
  redacted from `lines`.
- **`tests/host.rs`**: `init_non_interactive_builds_the_layout_and_starts_the_container` asserts
  `--network <container>`, `--hostname zephyr`, `--cap-add NET_ADMIN`, `ZEPHYR_NETWORK=bridge` and
  `-p 127.0.0.1:7790:7790`, and that `host` is not passed to `--network`. `docker network create` is
  called when `network inspect` fails. With `network = "host"`, the arguments are today's. An
  `[imessage]` config publishes `100.64.0.1:7791:7791`. `config set attach.port` recreates the
  container. Upgrading from a `host`-mode container prints the notice.
- **Ignored real-container test** (`tests/net_floor.rs -- --ignored`; it needs Docker and runs
  beside a live install through `ZEPHYR_CONTAINER`/`ZEPHYR_IMAGE`): `doctor` passes. As the agent,
  `curl https://example.com` works, `curl --noproxy '*' https://1.1.1.1` is refused at once,
  `curl http://169.254.169.254/` gets 403, `getent hosts example.com` fails, and `nc -z <bridge
  gateway> 22` is refused. agent-browser and Camoufox each open a page. `xdpyinfo -display :99`
  works in the container but not from the host. The host's `127.0.0.1:7790` attaches. The daemon's
  Discord and BlueBubbles checks in `doctor` still pass.
- **Manual, with the pinned rpi**: an `openai-codex` and an `antigravity` login completed by
  pasting the failed callback address, from the host `zephyr login` and from a channel's `/login`.

## Decided with the owner (2026-11-10)

1. Upgrades default to `bridge`; `network = "host"` in host.toml goes back.
2. The iMessage webhook is published on host loopback only, and the owner fronts it with
   `tailscale serve --tcp 7791 tcp://127.0.0.1:7791` (no boot-ordering risk; the web app, W,
   already needs `tailscale serve`). §4 is amended to match when this is built.
3. Only ports 80 and 443 without asking; anything else is a `network.allow` entry.
4. The host's own public address on a VPS stays a non-goal.
