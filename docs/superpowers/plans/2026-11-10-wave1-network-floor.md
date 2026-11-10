# Network floor (A2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The agent's uid has no direct network: it reaches only its own loopback and a CONNECT/HTTP proxy in the daemon (127.0.0.1:7792), which allows public addresses on ports 80 and 443 plus the owner's exact `network.allow` entries, on a Docker bridge network where the daemon's listeners are published to host loopback only.

**Architecture:** `zephyr up` moves the container from `--network host` to a user-defined bridge with `NET_ADMIN`. `supervise` loads an nftables table that sends every packet with `meta skuid <agent>` to a chain accepting only loopback (minus Docker's DNS), then drops `NET_ADMIN` from the bounding set before it starts the daemon. A new `egress.rs` holds the pure address policy, the hyper proxy that checks every resolved address and connects only to those, the listener-address helper the attach port, the webhook (and W's web app) bind with, and the doctor probes. The agent's processes get `*_PROXY` through `agent::cmd`.

**Tech Stack:** Rust (tokio, hyper 1 `server`+`http1`+**`client`** feature, hyper-util, http-body-util, `url` 2), nftables (`/usr/sbin/nft`) in the image, `libc::prctl`, Python `fake-rpi.py`.

**Spec:** `docs/superpowers/specs/2026-11-09-network-floor-design.md`. Its "Decided with the owner (2026-11-10)" section overrides the body. Decision 2 replaces §4.2's `-p <imessage ip>:<port>:<port>` and §4.10's Tailscale boot-ordering case: the webhook is published on `127.0.0.1:<port>` like the attach port, and the owner runs `tailscale serve --bg --tcp <port> tcp://127.0.0.1:<port>` once. Wave index: `docs/superpowers/plans/2026-11-10-wave1.md`.

## Global Constraints

The wave index's Global Constraints apply: commits, worktree, TDD order with fmt/clippy/test, event waits, the no-secrets rule, root's full paths, ports, no new crates, docs in the same task, and side containers only. In addition, for this lane:

- Branch `claude/network-floor`, its own worktree. **No schema change** (`SCHEMA_VERSION` stays 16).
- Proxy: `127.0.0.1:7792` (`egress::PORT`); **256** connections at once, refused at once past that; **10 s** to send the head; name lookup **5 s**; each connect **10 s**; ports **80 and 443** without asking, anything else only as an exact `host:port` in `network.allow`.
- IPv4 is public unless in 0/8, 10/8, 100.64/10, 127/8, 169.254/16, 172.16/12, 192.0.0/24, 192.0.2/24, 192.88.99/24, 192.168/16, 198.18/15, 198.51.100/24, 203.0.113/24, 224/4, 240/4. IPv6 is public only inside 2000::/3 and not in 2001::/23, 2001:db8::/32, 2002::/16, 3fff::/20. `private_ok` (listed entries only): 10/8, 172.16/12, 192.168/16, 100.64/10, fc00::/7. Loopback, link-local, unspecified, multicast, broadcast and the container's own address are never allowed, listed or not. If any resolved address fails, the whole request is refused.
- Audit: session `net`, tool `connect` (CONNECT) or `http` (absolute form), args `host:port -> ip`, scrubbed of facts (`facts::scrubbed`), cut to 500 bytes by `gate::audit_row`. Outcome `allowed` once per `host:port` per hour (in memory), `refused: <reason>` for every refusal, at most **60 a minute**, then one `refused: N more` row.
- Exact copy:
  - 403 status line `zephyr: <reason>` (`hyper::ext::ReasonPhrase`), with the body `the owner can allow HOST:PORT in network.allow; propose it with gateway /set network.allow`.
  - Reasons: `port <p> needs network.allow`, `private address <ip>` (unlisted), `<ip> is never allowed` (listed, but loopback, link-local, own address or another never-class), and `loopback is never allowed` when that address is loopback.
  - Bind failure, told once: `[zephyr] the network proxy is down, so the agent has no internet: <error>`.
  - supervise: `network floor could not be loaded: <nft's error>; network = "host" in host.toml runs without it`.
- `host.toml` `network = "bridge" | "host"`; absent means `bridge`; any other value fails `up` with `network in host.toml must be "bridge" or "host"`. `host` mode passes exactly today's `docker run` arguments, and the daemon behaves exactly as today (no proxy, no rules, `127.0.0.1` / `listen` binds).
- In the container `ZEPHYR_NETWORK=bridge` is the only switch (`egress::bridge()`). Tests never set it: unit tests call the bridge-mode functions directly.

## Review Focus

1. **An IPv6 address carrying an IPv4 one** (`::ffff:127.0.0.1`, `::ffff:7f00:1`, `64:ff9b::a9fe:a9fe`, `::127.0.0.1`): refused, with no unwrapping step. Test in A2-1 (`public` table and `target` normalization) and A2-2 (a CONNECT to `[::ffff:127.0.0.1]:443` gets 403).
2. **A name that resolves to one public and one private address** (DNS rebinding): the whole request is refused and the public address is never tried. Test in A2-2: the upstream counts zero accepts.
3. **An install with `[imessage]` upgraded to bridge mode**: BlueBubbles webhooks stop arriving until `tailscale serve` forwards the port. `up` must print the exact command, and it must publish `127.0.0.1:<port>`, never the Tailscale IP. Test in A2-7.
4. **A `network.allow` entry whose name resolves to link-local, loopback or the container's own address** (`meta.lan:80` → 169.254.169.254): refused even though listed. Test in A2-1.
5. **An absolute-form request that dodges the CONNECT rules** (`GET http://upstream:22/`, `GET https://example.com/`, a CONNECT with no port): the same port rule applies, and the other forms get 400. Test in A2-2.

---

### Task A2-1: Address policy, `network.allow` parsing, the registry row

**Files:**
- Create: `crates/zephyr/src/egress.rs` (`pub mod egress;` in `crates/zephyr/src/lib.rs`)
- Modify: `crates/zephyr/src/config.rs` (`Config` at :9, `validate` at :345), `crates/zephyr/src/settings.rs` (a row after `facts`, :358-370)

**Interfaces:**
- Produces (`egress.rs`):
  ```rust
  pub const PORT: u16 = 7792;
  pub fn bridge() -> bool;                              // std::env::var("ZEPHYR_NETWORK") == "bridge"
  /// The `zephyr` line Docker writes to /etc/hosts (`("zephyr", 0).to_socket_addrs()`): IPv4, not loopback, else an error.
  pub fn own_ip() -> anyhow::Result<Ipv4Addr>;
  pub fn public(ip: IpAddr) -> bool;                    // the two tables in Global Constraints, each line citing IANA
  pub fn private_ok(ip: IpAddr) -> bool;
  #[derive(Debug, Clone, PartialEq)] pub struct Entry { pub host: url::Host<String>, pub port: u16 }
  pub fn parse_allow(raw: &str) -> Result<Vec<Entry>, String>;   // comma separated; "" → []
  #[derive(Debug, PartialEq)] pub enum Refusal { Bad(String) /*400*/, Denied(String) /*403*/, Unreachable(String) /*502*/ }
  pub fn target(authority: &str) -> Result<(url::Host<String>, u16), Refusal>;
  pub type Lookup = fn(String, u16) -> futures_util::future::BoxFuture<'static, std::io::Result<Vec<SocketAddr>>>;
  pub fn system_lookup(host: String, port: u16) -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>>; // tokio::net::lookup_host
  pub async fn decide(host: &url::Host<String>, port: u16, allow: &[Entry], own: IpAddr,
      public: fn(IpAddr) -> bool, lookup: Lookup) -> Result<Vec<SocketAddr>, Refusal>;
  ```
- `config.rs`: `#[derive(Default, …)] pub struct Network { #[serde(default)] pub allow: String }` and `#[serde(default)] pub network: Network` on `Config` (`deny_unknown_fields` as its neighbours). `validate` calls `egress::parse_allow(&self.network.allow)` and fails with `network.allow: <its error>`. `reload::diff` needs no change: `[network]` is `Plan::Live`, because the proxy reads `cfgs.borrow()` on each request.
- `settings.rs` row, verbatim from spec §4.4: `row("network.allow", "Security", "Network exceptions", "Exact HOST:PORT the agent may reach beyond public ports 80 and 443, comma separated. No wildcards.", Kind::Text { max: 2000 }, Access::Propose, true, Stricter::NotOrdered, Apply::Toml)`.

- [x] **Step 1: Write the failing tests** in `egress.rs` `mod tests`:
  - `public_refuses_every_special_range_and_embedded_ipv4`: every denied literal in spec §5's list, plus `::127.0.0.1`, gives `!public(ip)`. `1.1.1.1`, `8.8.8.8` and `2606:4700::1111` give `public(ip)`.
  - `targets_normalize_numeric_hosts_through_url`: `target("2130706433:443")`, `"0x7f.1:443"`, `"0177.0.0.1:443"` and `"127.1:443"` all give `Host::Ipv4(127.0.0.1)`. `"[::ffff:127.0.0.1]:443"` gives `Host::Ipv6(::ffff:7f00:1)`. `"example.com:80"` gives port 80. Take the port from the authority, never `Url::port()`, which drops a default port. `"example.com"` (no port), `"user@example.com:443"` and `"example.com:0"` give `Refusal::Bad`.
  - `parse_allow_takes_exact_host_ports_only`: `"imap.gmail.com:993, github.com:22,homeassistant.lan:8123"` gives 3 entries, and `""` gives `[]`. Errors for `*.x:443`, `x`, `x:0`, `x:65536`, `http://x:1`, `127.0.0.1:22`, `[::1]:22` and `169.254.169.254:80`.
  - `decide_applies_the_port_rule_and_the_allow_list`, with a test `Lookup` fn that matches on the name: `example.com:22` unlisted gives `Denied("port 22 needs network.allow")`, and listed gives Ok. `1.1.1.1:443` gives `Ok([1.1.1.1:443])`. `10.0.0.5:443` gives `Denied("private address 10.0.0.5")`. `homeassistant.lan:8123` listed (→ 192.168.1.10) gives Ok, and the same name on `:443` unlisted gives Denied. A mixed answer `[1.1.1.1, 10.0.0.5]` gives Denied. An empty answer gives `Unreachable`. Matching is case-insensitive through `url` (`IMAP.GMAIL.COM:993` matches `imap.gmail.com:993`).
  - `a_listed_name_never_reaches_loopback_link_local_or_this_container` (Review Focus 4): listed names resolving to 169.254.169.254, to 127.0.0.1 and to `own` are each Denied. The 127.0.0.1 one gives `loopback is never allowed`.
  - In `config.rs` tests, `a_wildcard_network_allow_is_refused`: `[network]\nallow = "*.gmail.com:993"` fails `validate`. In `settings.rs` tests, `network_allow_is_a_strong_proposal_only_row`: `find("network.allow")` is `Access::Propose` and `strong`, and `decide` for `Actor::Agent` gives `Verdict::Propose`.
- [x] **Step 2:** `cargo test -p zephyr --lib -- egress config settings` gives FAIL (module missing).
- [x] **Step 3: Implement.** `target` parses with `url::Url::parse(&format!("http://{authority}"))`. `decide` runs the port rule before any lookup. An IP literal skips the lookup. A name goes through `lookup` under `tokio::time::timeout(5 s)`. The check runs over every address before any is returned.
- [x] **Step 4:** Run the tests again: PASS. Then run `cargo test -p zephyr` (the registry's own table tests cover the new row).
- [x] **Step 5:** Commit `feat(egress): decide which addresses the agent may reach, and let the owner list exact exceptions in network.allow`.
- Deviation: `settings::parse` also runs `egress::parse_allow` for `network.allow` and `settings::value` shows it, so `/set network.allow` refuses a bad entry before the file is written (spec §4.4: "the value is checked by `egress::parse_allow`"). `target` also refuses an authority with a path, a query or whitespace.

### Task A2-2: The proxy and its audit

**Files:**
- Modify: `crates/zephyr/src/egress.rs`, `crates/zephyr/Cargo.toml` (hyper features `["server", "http1", "client"]`), `crates/zephyr/src/router.rs` (`select!` at :291-300), `crates/zephyr/src/gate.rs` (`audit_row` :610 becomes `pub(crate)`), `DESIGN.md` (§4.4 item 3 at :325, §4.16 "What it does not stop" at :695), `README.md` (`network.allow` in "Changing settings")

**Interfaces:**
- Consumes: A2-1 `decide`, `target`, `parse_allow`, `Refusal`, `Lookup`, `system_lookup`, `PORT`, `bridge`.
- Produces:
  ```rust
  pub struct Floor { pub public: fn(IpAddr) -> bool, pub own: IpAddr, pub lookup: Lookup,
      pub conns: usize, pub head_wait: Duration, pub connect_wait: Duration }
  impl Floor { pub fn live() -> anyhow::Result<Floor> }  // public, own_ip() (A2-1), system_lookup, 256, 10 s, 10 s
  pub async fn serve(cfgs: watch::Receiver<Arc<Config>>, wake: &crate::scheduler::Wake) -> anyhow::Result<()>;
  pub async fn serve_on(listener: TcpListener, cfgs: watch::Receiver<Arc<Config>>, floor: Floor) -> anyhow::Result<()>;
  pub enum Outcome { Allowed, Refused(String) }
  #[derive(Default)] pub struct Audit { /* hour map, minute, count */ }
  impl Audit { pub fn note(&mut self, now: i64, tool: &str, dest: &str, o: Outcome) -> Vec<(String, String, String)> } // (tool, args, outcome) rows to write
  ```
- `serve` is `std::future::pending()` unless `bridge()`. It binds `127.0.0.1:PORT`. On a bind error (or a failing `Floor::live`) it logs, tells the owner once with the bind-failure copy (`notify_last_owner`, then `wake.outbox.notify_one()`, as `gate::serve` does at :1066-1075), and stays pending. Router: `r = crate::egress::serve(egress_cfgs, &jobs) => r.map(|()| Exit::Shutdown),` beside the gate's arm.
- `serve_on` follows hyper's `http_proxy` example: `http1::Builder` with `header_read_timeout(head_wait)` and `.with_upgrades()`, and a semaphore of `conns` taken with `try_acquire_owned` (drop on failure, as `attach.rs:116-121`).
  - CONNECT: decide, then 200, then `hyper::upgrade::on` and `copy_bidirectional`.
  - Absolute `http://` form: decide, then a fresh `client::conn::http1::handshake` over a `TcpStream` connected to the checked addresses in order (`connect_wait` each). The request is sent in origin form with `Proxy-Authorization` and `Proxy-Connection` removed and `Host` set from the URI when missing.
  - Anything else is 400. The store is `Store::open(&cfg.db_path())` held behind a mutex, as the gate's. Rows are written with `gate::audit_row(store, "net", tool, &facts::scrubbed(&cfg, args), outcome)`.
- `Audit::note`: `allowed` once per `dest`'s `host:port` per `now / 3600`. Refusals count per `now / 60`, rows up to 60. The first note in a later minute first emits `("connect", "", "refused: N more")` when N > 0.

- [ ] **Step 1: Write the failing tests** in `egress.rs` (tokio). Each test serves `serve_on` on `127.0.0.1:0` with a temp config and state dir. The `Floor` has `public = |ip| ip == 127.0.0.1`, `own = 10.255.255.1`, and a test `Lookup` mapping `upstream.test` to `127.0.0.1:<port>` and `rebind.test` to `[127.0.0.1:<port>, 127.0.0.2:<port>]`. The upstream is a local listener counting accepts. Allow entries carry the upstream's dynamic port (`upstream.test:<port>`).
  - `a_connect_tunnel_carries_bytes_both_ways`: listed, `CONNECT upstream.test:<port>` gives `200`, then `ping` goes up and `pong` comes back.
  - `an_absolute_get_arrives_in_origin_form_without_proxy_headers`: the upstream reads `GET /p?q HTTP/1.1`, a `Host` header, and no `Proxy-Authorization` or `Proxy-Connection`.
  - `a_name_with_one_refused_address_is_refused_whole` (Review Focus 2): `CONNECT rebind.test:<port>` (listed) gives `403`, and the upstream's accept count stays 0.
  - `an_embedded_ipv4_literal_is_refused` (Review Focus 1): `CONNECT [::ffff:127.0.0.1]:443` gives `HTTP/1.1 403 zephyr: private address ::ffff:7f00:1` (`public` is the real one here, with no lookup).
  - `an_unlisted_port_is_refused_until_the_owner_lists_it`: the reply carries the status line `403 zephyr: port <port> needs network.allow` and a body containing `propose it with gateway /set network.allow`. Then send the config with the entry through the watch sender: the next CONNECT gets 200.
  - `absolute_form_follows_the_same_rules` (Review Focus 5): `GET http://upstream.test:<port>/` unlisted gives 403. `GET https://example.com/` gives 400. `CONNECT example.com HTTP/1.1` (no port) gives 400.
  - `a_full_proxy_refuses_at_once`: `conns: 2`, two idle connections open, and a third reads EOF within 1 s.
  - `an_unfinished_head_is_closed`: `head_wait: 200 ms`, send `CONNECT x` with no blank line, and read EOF within 2 s.
  - `audit_lists_destinations_hourly_and_folds_refusal_floods` (pure `Audit`): two `Allowed` notes in one hour give 1 row, and the next hour gives 1 more. 61 `Refused` notes in minute 0 give 60 rows. The first note in minute 1 gives `refused: 1 more` and then its own row.
  - `audit_rows_carry_no_fact_value`: a fact saved through the crate's facts API (as `gate.rs`'s tests do), then `CONNECT <value>.example:443`. `store.audit_rows()` shows a `net` row whose args lack the value.
- [ ] **Step 2:** `cargo test -p zephyr --lib egress` gives FAIL.
- [ ] **Step 3: Implement** as in Interfaces. Add `"client"` to hyper's features. `cargo deny check advisories bans`.
- [ ] **Step 4:** `cargo test -p zephyr --lib egress` passes, then `cargo test -p zephyr --test router` (the extra `select!` arm is pending outside bridge mode).
- [ ] **Step 5: Docs.** DESIGN §4.4 item 3: "General egress is not blocked either" becomes "the agent's egress goes only through the proxy, to public ports 80 and 443 plus `network.allow` (§4.19)". The ClawSafety reasoning stays. §4.16: SMTP, IMAP and other non-HTTP protocols are closed by §4.19, and HTTPS to a mail API with a token the agent holds stays open (A3). README: the `network.allow` row and its typical entries (`imap.gmail.com:993`, `github.com:22`, `homeassistant.lan:8123`).
- [ ] **Step 6:** Commit `feat(egress): serve the agent's only way out, a proxy that connects only to addresses it checked`.

### Task A2-3: The agent's processes use the proxy

**Files:**
- Modify: `crates/zephyr/src/agent.rs` (`cmd` :59-70), `crates/zephyr/src/host/mod.rs` (`exec_agent` :164-176)
- Test: `crates/zephyr/src/agent.rs` tests, `crates/zephyr/tests/host.rs` (`shell_opens_a_login_shell_as_the_agent` :1605)

**Interfaces:**
- Consumes: A2-1 `egress::bridge`, `egress::PORT`.
- Produces:
  ```rust
  pub const PROXY_ENV: &[(&str, &str)] = &[("HTTP_PROXY", "http://127.0.0.1:7792"), ("HTTPS_PROXY", …), ("http_proxy", …),
      ("https_proxy", …), ("NO_PROXY", "localhost,127.0.0.1,::1"), ("no_proxy", …), ("NODE_USE_ENV_PROXY", "1")];
  pub fn proxy(cmd: &mut Command) -> &mut Command; // PROXY_ENV, and AGENT_BROWSER_ARGS = "<daemon's value>,--proxy-server=http://127.0.0.1:7792" (no leading comma when unset)
  ```
  `cmd` calls `proxy` when `egress::bridge()`. There is no `ALL_PROXY` (rpi 0.18 refuses a non-HTTP proxy URL). `Host::bridge(&self) -> anyhow::Result<bool>` reads `host_get("network")`, validated as in Global Constraints. `exec_agent` adds each `PROXY_ENV` pair as `-e K=V` when `self.bridge().unwrap_or(true)`. That covers `zephyr shell`, `login` and `--list-models`.

- [ ] **Step 1: Write the failing tests:**
  - `bridge_mode_gives_the_agent_the_proxy_and_the_browser_flag`: `proxy(&mut Command::new("true")).get_envs()` holds every `PROXY_ENV` pair, and `AGENT_BROWSER_ARGS` ends with `--proxy-server=http://127.0.0.1:7792`. `PROXY_ENV[0].1 == format!("http://127.0.0.1:{}", egress::PORT)`.
  - `outside_bridge_mode_the_agent_gets_no_proxy`: `cmd(&mut Command::new("true"))` sets none of the keys (the test process has no `ZEPHYR_NETWORK`).
  - `tests/host.rs`: the shell's `docker exec` has `-e HTTPS_PROXY=http://127.0.0.1:7792` with the default host.toml, and has no `HTTPS_PROXY` with `network = "host"`.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** `cargo test -p zephyr --lib agent` and `cargo test -p zephyr --test host` → PASS.
- [ ] **Step 5:** Commit `feat(agent): send the agent's processes and its browser through the network proxy in bridge mode`.

### Task A2-4: Listeners on the container's address

**Files:**
- Modify: `crates/zephyr/src/egress.rs`, `crates/zephyr/src/attach.rs` (:110), `crates/zephyr/src/imessage.rs` (`run` :463-465), `crates/zephyr/src/tui/mod.rs` (:82), `crates/zephyr/src/router.rs` (`reloaded` :3328), `DESIGN.md` (§4.3 bullet at :122)

**Interfaces (the W lane follows these for its web listener on 7793):**
- Produces:
  ```rust
  // egress::own_ip() -> anyhow::Result<Ipv4Addr> comes from A2-1.
  /// Where a daemon listener binds, and where in-container clients connect: in bridge mode
  /// `(own_ip()?, addr.port())`, otherwise `addr` unchanged.
  pub fn listen_addr(addr: SocketAddr) -> anyhow::Result<SocketAddr>;
  /// Every daemon TCP port `zephyr up` publishes, keyed by the zephyr.toml key that sets it.
  pub fn published(cfg: &Config) -> Vec<(&'static str, u16)>; // [("attach.port", a.port), ("imessage.listen", listen.port())]
  /// Keys whose published port differs between two configs.
  pub fn moved(old: &Config, new: &Config) -> Vec<&'static str>;
  ```
- Binding rule for every daemon TCP listener: `TcpListener::bind(egress::listen_addr(<host-mode address>)?)`. Attach passes `127.0.0.1:<attach.port>`. iMessage passes `self.listen`, whose IP keeps meaning the Tailscale address BlueBubbles posts to, and `check_listen` is unchanged. **W** passes `127.0.0.1:<web.port>` and adds `("web.port", w.port)` to `published`. That one row makes `up` publish it (A2-7), makes `config set web.port` recreate the container, and makes the reload notice cover it.
- Publish rule (A2-7): for each `(key, port)` in `published(&cfg)`, `zephyr up` passes `-p 127.0.0.1:<port>:<port>`. The owner fronts the webhook and the web app with `tailscale serve`.
- `zephyr tui` connects to `listen_addr(127.0.0.1:<a.port>)`. In `reloaded`, when `bridge()`, the router calls `notify_last_owner` once for each key in `moved(old, new)`: `[zephyr] <key> changed; run zephyr up on the host to publish it`.

- [ ] **Step 1: Write the failing tests** in `egress.rs`:
  - `published_lists_the_configured_ports`: a config with `[attach] port = 7790` and `[imessage] listen = "100.64.0.1:7791"` gives `[("attach.port", 7790), ("imessage.listen", 7791)]`, and neither section gives `[]`.
  - `moved_names_a_changed_port_only`: attach 7790 → 7799 gives `["attach.port"]`, and a changed `imessage.server_url` gives `[]`.
  - `listen_addr_keeps_the_address_outside_bridge_mode`.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement and switch the three binds/connects. **Step 4:** `cargo test -p zephyr` → PASS (attach, imessage and tui tests unchanged).
- [ ] **Step 5: Docs.** DESIGN §4.3, line 122: under the floor the agent cannot reach the attach port or the webhook. The caps stay for other users on the host. Both bind the container's own address, and in-container clients connect there.
- [ ] **Step 6:** Commit `feat(egress): bind the daemon's listeners on the container's own address in bridge mode`.

### Task A2-5: The rule set, loaded once by `supervise`, checked by the daemon

**Files:**
- Modify: `crates/zephyr/src/egress.rs`, `crates/zephyr/src/supervise.rs` (`Policy` :17-32, `main` :64-95, `run` :99), `crates/zephyr/src/main.rs` (dispatch beside `__file` :61, `run` :187), `deploy/Dockerfile` (`nftables` into the runtime `apt-get install` :159-165), `DESIGN.md` (§4.4 item 1 :302, §4.11 Desktop :472)

**Interfaces:**
- Consumes: A2-1 `own_ip`; `agent::cmd`; `agent::ids`.
- Produces:
  ```rust
  pub fn ruleset(agent_uid: u32) -> String;              // spec §4.3's text exactly, uid filled in
  pub fn load_floor() -> anyhow::Result<()>;             // ZEPHYR_AGENT_UID; ruleset piped to /usr/sbin/nft -f -; then
                                                          // prctl(PR_CAPBSET_DROP, 12 /* CAP_NET_ADMIN */)
  pub fn direct_refused(addr: SocketAddr, wait: Duration) -> Result<(), String>; // Ok only on ECONNREFUSED within `wait`
  pub fn netcheck(args: &[String]) -> std::process::ExitCode; // `zephyr __netcheck [OWN_IP:ATTACH_PORT]`, one `ok|fail|warn <label>` line per check
  pub fn verify() -> anyhow::Result<()>;                 // daemon start, bridge mode only
  ```
- `supervise::Policy` gains `pub floor: Option<fn() -> anyhow::Result<()>>` (default `None`). `main` sets it to `Some(egress::load_floor)` when `bridge()`. `run` calls it before the first spawn. On an error it prints the supervise copy from Global Constraints and returns 1, never spawning.
- `netcheck` with no argument runs probe (a) only: `direct_refused(1.1.1.1:443, 2 s)`. A connection, a timeout or `ENETUNREACH` is `fail`. A2-6 adds probes (b) to (d) when an address is given.
- `verify` runs `own_ip()?`. It checks that `/proc/self/status` `CapEff` has bit 12 clear (`the daemon still holds NET_ADMIN`). PR_CAPBSET_DROP does not clear an inherited capability, so this is the check that proves the drop. Then it runs `agent::cmd(Command::new("/usr/local/bin/zephyr")).arg("__netcheck")`, which must exit 0 within 5 s. `main.rs` `run` calls it right after the root/uid check when `bridge()`, so a failure exits non-zero before any agent process starts. `__netcheck` is dispatched before the config is read: the agent cannot read the config.

- [ ] **Step 1: Write the failing tests:**
  - `ruleset_matches_the_golden_text`: `ruleset(1000)` equals the spec §4.3 block verbatim, with its comments.
  - `direct_refused_accepts_only_a_refusal`: a just-closed local port gives Ok, and a listening `127.0.0.1:0` gives `Err` containing `connected`.
  - In `supervise.rs`, `a_floor_that_fails_to_load_never_starts_the_daemon`: `Policy { floor: Some(|| anyhow::bail!("nft: no")), ..quick() }` makes `run` return 1, and `counted`'s start count is 0.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. Add `nftables` to the Dockerfile. **Step 4:** `cargo test -p zephyr --lib -- egress supervise` → PASS, then `cargo test -p zephyr`.
- [ ] **Step 5: Docs.** DESIGN §4.4 item 1 "Network": the bridge network named after the container, `--hostname zephyr`, and `NET_ADMIN`, used once by `supervise` and dropped from the bounding set (the daemon checks `CapEff`). Only the attach port and the webhook are published, on host loopback. Point to §4.19. §4.11: the container has its own netns, so `@/tmp/.X11-unix/X99` is no longer the host's. The cookie stays.
- [ ] **Step 6:** Commit `feat(supervise): load the agent's network floor before the daemon starts, and refuse to run without it`.

### Task A2-6: `doctor` proves the floor

**Files:**
- Modify: `crates/zephyr/src/egress.rs` (`netcheck` probes b to d, `exposed`), `crates/zephyr/src/main.rs` (`doctor` :391), `crates/zephyr/src/host/mod.rs` (`doctor` :622), `DESIGN.md` (§4.8 `doctor` bullet)
- Test: `crates/zephyr/tests/host.rs` (the shim's `inspect` case)

**Interfaces:**
- Consumes: A2-5 `netcheck`, `direct_refused`; A2-1 `own_ip`.
- Produces:
  ```rust
  /// Root sockets the agent could reach: uid-0 TCP in LISTEN (state 0A) or bound UDP, on loopback or a
  /// wildcard address, except 127.0.0.1:7792 and 127.0.0.11:*. `files` are ("tcp"|"tcp6"|"udp"|"udp6", text).
  pub fn exposed(files: &[(&str, &str)]) -> Vec<String>;          // "ADDR:PORT" each
  fn proxy_status(target: &str) -> Result<u16, String>;           // CONNECT through 127.0.0.1:PORT, the status code
  ```
- `netcheck OWN_IP:PORT` adds the following checks:
  - (b) `direct_refused(own:port)`;
  - (c) resolving `example.com` fails within 5 s;
  - (d) `proxy_status("169.254.169.254:80")` and `proxy_status("[::ffff:7f00:1]:443")` are 403, and `proxy_status("example.com:443")` should be 200. Anything else is `warn` (the host may be offline).
- In bridge mode, in-container `doctor` runs `__netcheck <own_ip>:<attach.port|0>` as the agent and prints its lines (a `fail` fails doctor). It prints `exposed` over `/proc/net/{tcp,tcp6,udp,udp6}` as `root listens on ADDR, where the agent can reach it`. In host mode it prints one warning: `network floor off (host networking): the agent can reach the host's loopback and private ranges`.
- Host `doctor` reads `docker inspect -f '{{.HostConfig.NetworkMode}}' <container>` and warns `the container still runs host networking; zephyr up moves it to the network floor` when it says `host` while host.toml does not.

- [ ] **Step 1: Write the failing tests:**
  - `exposed_names_root_listeners_the_agent_could_reach`, on fixture `/proc/net` lines:
    - listed: uid 0 LISTEN `0100007F:1F90` (127.0.0.1:8080), `00000000:1E6E` (0.0.0.0:7790), and tcp6 `00000000000000000000000001000000:1F90` (::1:8080);
    - not listed: `0100007F:1E70` (the proxy), uid 1000 on 127.0.0.1:3000, uid 0 on `020012AC:1E6E` (172.18.0.2), and uid 0 udp `0B00007F:…` (127.0.0.11).
  - `proxy_status_reads_the_status_code`: a local listener answering `HTTP/1.1 403 x\r\n\r\n` gives `Ok(403)`.
  - `tests/host.rs` `doctor_warns_about_a_container_on_host_networking`: the shim's `inspect` answers `${SHIM_NETMODE}` for a `NetworkMode` format, and with `SHIM_NETMODE=host` the output contains the warning.
- [ ] **Step 2:** Run → FAIL. **Step 3:** Implement. **Step 4:** `cargo test -p zephyr --lib egress` and `cargo test -p zephyr --test host` → PASS.
- [ ] **Step 5: Docs.** DESIGN §4.8 `doctor`: the probes run as the agent, the root-listener rule, and the host-mode warning.
- [ ] **Step 6:** Commit `feat(doctor): prove the network floor by probing as the agent and refusing root listeners it could reach`.

### Task A2-7: `zephyr up` in bridge mode, publishing, migration

**Files:**
- Modify: `crates/zephyr/src/host/mod.rs` (`up` :313-432, `attach` stock :580-588, `config set` :1253-1278, `uninstall` :1297), `crates/zephyr/src/host/setup.rs` (`init` after `set_projects` :62, `channel` :619-623, `channel_remove` :664), `crates/zephyr/src/host/conf.rs` (beside `host_get` :218), `README.md`
- Test: `crates/zephyr/tests/host.rs` (shim :14-56)

**Interfaces:**
- Consumes: A2-3 `Host::bridge`; A2-4 `egress::published`.
- Produces (host side):
  ```rust
  fn network_mode(&self) -> Option<String>;                    // docker inspect -f '{{.HostConfig.NetworkMode}}'
  fn container_ip(&self) -> anyhow::Result<String>;            // docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}'
  /// After a zephyr.toml edit: `up_checked` in bridge mode when `published` changed from `before`, else `apply`.
  pub(crate) fn apply_published(&self, before: &[(&'static str, u16)]) -> anyhow::Result<()>;
  pub fn set_network_default(&self) -> anyhow::Result<()>;     // Data: writes network = "bridge" with the comment
                                                                // `# "host" runs without the network floor (DESIGN §4.19)`, only when absent
  ```
- Bridge-mode `up`:
  1. Before `docker run`: `docker network inspect <container>`, and on failure `docker network create --driver bridge <container>` (no `--ipv6`, no subnet).
  2. Then it replaces `"--network", "host"` with `--network <container> --hostname zephyr --cap-add NET_ADMIN -e ZEPHYR_NETWORK=bridge`, plus `-p 127.0.0.1:<p>:<p>` for each `published` entry (the config parsed with `toml::from_str::<Config>`).
  3. Before the old container steps aside, `up` reads its `network_mode()`. If it was `host`, `up` prints spec §4.11's notice verbatim after the new one runs.
  4. With `[imessage]` in bridge mode it prints `the iMessage webhook is on this host's 127.0.0.1:<p> now; forward the Tailscale address to it once: sudo tailscale serve --bg --tcp <p> tcp://127.0.0.1:<p>`.
- `uninstall` runs `docker network rm <container>` after `rm -f`. `attach --stock` passes `<container_ip>:<port>` in bridge mode. `config set` (file path) and `channel add|remove` call `apply_published` with the `published` list read before the edit. `init` calls `set_network_default`.

- [ ] **Step 1: Write the failing tests** in `tests/host.rs`. The shim's `network` case exits `${SHIM_NO_NETWORK:-0}` for `inspect`. Its `inspect` answers `${SHIM_NETMODE:-$CONTAINER}` for `NetworkMode` and `${SHIM_IP:-172.18.0.2}` for `IPAddress` formats, and keeps `SHIM_STATE` otherwise.
  - Extend `init_non_interactive_builds_the_layout_and_starts_the_container`. The run has the pairs `["--network", CONTAINER]`, `["--hostname", "zephyr"]` and `["--cap-add", "NET_ADMIN"]`, and contains `ZEPHYR_NETWORK=bridge` and `["-p", "127.0.0.1:7790:7790"]`. There is no `["--network", "host"]`. host.toml contains `network = "bridge"`.
  - `up_creates_the_network_only_when_missing`: with `SHIM_NO_NETWORK=1` the calls hold `docker network create --driver bridge CONTAINER`, and without it they do not.
  - `host_networking_in_host_toml_keeps_todays_arguments`: `network = "host"` gives `["--network", "host"]`, with no `NET_ADMIN`, no `-p` and no `ZEPHYR_NETWORK`. `network = "none"` fails with the Global Constraints text.
  - `imessage_is_published_on_loopback_with_a_tailscale_serve_hint` (Review Focus 3): `[imessage] listen = "100.64.0.1:7791"` gives `-p 127.0.0.1:7791:7791`. No argument contains `100.64.0.1`. stdout contains `tailscale serve --bg --tcp 7791 tcp://127.0.0.1:7791`.
  - `config_set_of_a_published_port_recreates_the_container`: with the daemon down, `config set attach.port 7799` makes a new `docker run -d` with `-p 127.0.0.1:7799:7799`, and no `zephyr reload` exec.
  - `up_from_a_host_networking_container_explains_the_floor`: with `SHIM_NETMODE=host`, stdout contains `the agent now reaches the internet only through zephyr's proxy`.
  - `uninstall_removes_the_network` and `attach_stock_connects_to_the_container_address` (argv ends with `172.18.0.2:7790`).
- [ ] **Step 2:** `cargo test -p zephyr --test host` → FAIL. **Step 3:** Implement. **Step 4:** Run → PASS. The existing `up`/`upgrade` tests stay green.
- [ ] **Step 5: Docs.** README covers:
  - `network` in host.toml and how to go back;
  - the migration notice;
  - the iMessage `tailscale serve --bg --tcp` step in the channel setup;
  - the x11vnc recipe with `-listen <container ip>` in place of `-localhost`, where `docker inspect` prints the IP and the viewer connects to it from the host;
  - model servers on host loopback, which bind the bridge gateway and are listed in `network.allow`.
- [ ] **Step 6:** Commit `feat(host): run the container on its own bridge with only zephyr's ports published on loopback`.

### Task A2-8: Sign-ins by pasting the callback address

**Files:**
- Modify: `crates/zephyr/src/auth.rs` (:521-525), `crates/zephyr/src/router.rs` (`login` :2844, `login_notices` :3057-3092, `Login` :379), `crates/zephyr/src/host/setup.rs` (OAuth `-it` login :429-434), `crates/zephyr/tests/fake-rpi.py` (login :194-213), `DESIGN.md` (§4.7.1 `oauth` :382), `README.md`
- Test: `crates/zephyr/tests/login.rs` (:394, `login_from_a_channel` :660)

**Interfaces:**
- Consumes: `auth::input(id, text) -> bool` (:359), `auth::poll`.
- Produces:
  - `auth::start` sets `Phase::NeedInput` for every kind, so `zephyr.login_input` (attach.rs:323) reaches an OAuth child's stdin. The pasted text joins `inner.secrets` and is redacted from `lines`.
  - Router: `Router.posted: Vec<Login>` keeps a login after its URL notice, pruned on the tick when `auth::poll` is `None` or terminal. `/login paste TEXT` feeds the newest posted login whose `from` is this `(channel, sender)` through `auth::input`.
- Router copy:
  - the taken reply: `[zephyr] sent to the <provider> sign-in`;
  - no login of the sender's waiting: `[zephyr] no sign-in of yours is waiting for a pasted address`;
  - a text that neither starts with `http://127.0.0.1` or `http://localhost` nor contains `code=`: `[zephyr] /login paste takes the address the browser could not open (http://127.0.0.1:…?code=…). Never paste a token here.`;
  - the URL notice gains a second line `If the page then cannot open 127.0.0.1, send /login paste <its address>.`, and `/login` with no argument lists `/login paste ADDRESS`.
- Host `zephyr login`, before the interactive OAuth `run`, prints `if the browser then cannot open 127.0.0.1, paste its address here`.
- fake-rpi: the `login_url` branch prints `Paste OAuth code or response:` after the URL, then finishes on `login_go` **or** on a non-empty stdin line. A non-empty line logs `{"type": "auth-paste"}` (never the text) to `log.jsonl`.

- [ ] **Step 1: Write the failing tests** in `tests/login.rs`:
  - `a_pasted_callback_address_finishes_an_oauth_login_and_is_redacted`: start `antigravity` with `login_url`, wait for the URL, `auth::input(id, "http://127.0.0.1:1455/cb?code=SECRETCODE")` is true, and the login gets to `done`. `log.jsonl` has `auth-paste`, and no poll's `lines` contains `SECRETCODE`.
  - In `an_oauth_login_reports_its_url_then_finishes_when_the_browser_returns`, the waiting phase is now `need_input`.
  - `a_channel_pastes_the_callback_into_its_own_oauth_login`: owner A on discord runs `/login antigravity`, and the notice contains `/login paste`. Owner B's `/login paste http://127.0.0.1:1455/cb?code=X` gets the no-sign-in reply. `/login paste sk-ant-abc` gets the refusal. A's paste gets `sent to the antigravity sign-in` and later `done`.
  - `tests/host.rs`: an interactive OAuth `zephyr login` prints the hint before the `docker exec -it`.
- [ ] **Step 2:** `cargo test -p zephyr --test login` → FAIL. **Step 3:** Implement Rust and fake-rpi together. **Step 4:** Run `cargo test -p zephyr --test login --test host --test router` and `cargo test -p zephyr --lib tui` → PASS.
- [ ] **Step 5: Docs.** DESIGN §4.7.1: the callback no longer reaches the container, so the owner pastes the address. The text is kept only to redact the echo, and a channel paste stays in that inbox row as any message does (a used, single-use code). README covers sign-ins.
- [ ] **Step 6:** Commit `feat(auth): finish an OAuth sign-in from the address the browser could not open`.

### Task A2-9: §4.19, and the side-container check

**Files:**
- Modify: `DESIGN.md` (new §4.19 after §4.18 :751; §7 :788), `CLAUDE.md` (an `egress.rs` line in the architecture list)
- Create: `crates/zephyr/tests/net_floor.rs` (`#[ignore]`)

**Interfaces:**
- Consumes: everything above.
- `net_floor.rs` panics at its start unless `ZEPHYR_CONTAINER` is set and is not `zephyr` (never the live install). Without Docker it skips, unless `ZEPHYR_REAL_RPI=1`. Agent commands are `docker exec -u <uid>:<gid> -e PATH=<AGENT_PATH>` plus each `agent::PROXY_ENV` pair, and `AGENT_BROWSER_ARGS` with the proxy flag (as `agent::proxy` builds it).

- [ ] **Step 1: Docs.** DESIGN §4.19 Network floor: the rule set and why it looks as it does (spec §4.3), the proxy and its checks (§4.4), what the agent's processes get and what stops working (§4.5), listeners (§4.6, with decision 2's loopback publish and `tailscale serve`), sign-ins and X11 (§4.7), audit (§4.8), failure handling (§4.10, without the Tailscale boot-order case), and the owner's four decisions. §7: the host's own public address on a VPS stays open. CLAUDE.md: `egress.rs` (DESIGN §4.19): the proxy on 127.0.0.1:7792, `public`/`decide`, `listen_addr`/`published` for every daemon listener, `ruleset`/`load_floor` run by `supervise`, `verify`, and `__netcheck`.
- [ ] **Step 2: Write `net_floor.rs`'s `the_agent_reaches_only_the_proxy`**, asserting each check in Step 4. It must compile and skip: `cargo test -p zephyr --test net_floor` → 0 run, 1 ignored.
- [ ] **Step 3: Bring up the side container** (own HOME, own image, and attach on 17790 because the live install holds 7790):
  ```sh
  cargo build -p zephyr && Z=$PWD/target/debug/zephyr
  export ZEPHYR_CONTAINER=zephyr-test ZEPHYR_IMAGE=zephyr-dev:$(git rev-parse --short HEAD)
  export ZEPHYR_SRC=$PWD HOME=$(mktemp -d) && mkdir "$HOME/p"
  $Z init --non-interactive --no-start --projects "$HOME/p" --provider anthropic --model anthropic/claude-sonnet-5
  $Z config set attach.port 17790 && $Z up --build
  ```
  Expected: `docker inspect -f '{{.HostConfig.NetworkMode}}' zephyr-test` prints `zephyr-test`.
- [ ] **Step 4: Run the checks:** `cargo test -p zephyr --test net_floor -- --ignored` (same env). It passes when all of these hold:
  - As the agent, each of these fails at once and gets no answer:
    - `curl -m 5 http://127.0.0.1:17790/` and `curl -m 5 http://127.0.0.1:7790/`;
    - `curl -m 5 --noproxy '*'` to `http://192.168.1.1/`, `http://100.100.100.100/`, `http://169.254.169.254/` and `https://1.1.1.1`.
  - Through the proxy, `curl -s -o /dev/null -w '%{http_code}'` to `http://192.168.1.1/`, `http://100.100.100.100/` and `http://169.254.169.254/` prints `403`.
  - `getent hosts example.com` fails.
  - `curl -sS -m 20 https://example.com` returns `Example Domain`.
  - `agent-browser open https://example.com` then `agent-browser get title` gives `Example Domain`. A Camoufox `goto("https://example.com")` (`camoufox-python`, headless) gives the same title.
  - `docker exec zephyr-test sqlite3 /var/lib/zephyr/state.db "select args, outcome from audit where session = 'net'"` lists `example.com:443 -> …|allowed`, which proves the browsers went through the proxy, plus the 403 refusals.
  - `docker exec zephyr-test zephyr doctor` exits 0 with the floor lines `ok`.
  - The daemon's `CapEff` (`/proc/<pid of zephyr run>/status`) has bit 12 clear.
  - From the host, `curl -s -m 3 http://127.0.0.1:17790/` connects (the publish works).
- [ ] **Step 5: Manual checks**, recorded in the PR body:
  - `docker exec -u $(id -u) -e DISPLAY=:99 -e XAUTHORITY=/zephyr/.zephyr-xauthority zephyr-test xdpyinfo` works, and `xdpyinfo -display :99` on the host does not reach it.
  - OAuth paste on the host: `$Z login openai-codex`. Open the URL in the host's browser, sign in, let the `127.0.0.1:1455` page fail, and paste its address at rpi's prompt. Then `$Z accounts` lists the account.
  - The same through the TUI: `$Z attach`, `/login antigravity`, and paste the failed address into the panel's input row. It reaches `done`.
- [ ] **Step 6: Tear down:** `docker rm -f zephyr-test && docker network rm zephyr-test && rm -rf "$HOME"`.
- [ ] **Step 7:** Commit `docs(egress): describe the network floor, and check it against a real container`.
