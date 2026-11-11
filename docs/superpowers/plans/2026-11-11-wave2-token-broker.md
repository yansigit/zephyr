# Token broker (A3) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** A vault token or connector secret reaches the agent only as a surrogate (`zsur_` + 40 hex); the network floor's proxy decrypts the hosts the owner bound it to, checks the upstream against roots compiled into the binary, and puts the real value in there and nowhere else.

**Architecture:** `broker.rs` (new, pure) derives surrogates with HMAC-SHA256 from `<state>/broker.key`, parses bindings, builds the surrogate map from `vault.toml`, secrets.env and the config, and swaps or refuses on a request's head. `ca.rs` (new) keeps a container-only CA in `<state>/ca/`, rotated only by `supervise` at a container start, and signs one leaf per bound host. `egress.rs` sends a `CONNECT` to a bound `host:port` through `serve_tls` (TLS both legs, HTTP/1.1, one upstream connection) in place of the plain tunnel, and refuses a surrogate it sees anywhere else. `vault.rs` and `drafts.rs` hand out surrogates; `agent.rs`, `supervise.rs` and the image make the agent's tools trust the CA.

**Tech Stack:** Rust; `rcgen` (new), `rustls` 0.23 and `tokio-rustls` 0.26 (in Cargo.lock through reqwest, now direct, `ring` provider), `webpki-roots` 1 (in Cargo.lock, now direct), hyper 1 server and client, `ring` HMAC, `libnss3-tools` in the image.

**Spec:** `docs/superpowers/specs/2026-11-11-token-broker-design.md`. Its "Decided with the owner (2026-11-11)" section wins: (1) unbound tokens and `command` connectors are refused until bound or marked `raw`/`plain`, with one notice; (2) no body swaps; (3) provider credentials stay out of scope; (4) the CA lives 398 days and is renewed at a container start with under 180 left; (5) private-CA and plain-HTTP hosts are `raw` only. Wave index: `docs/superpowers/plans/2026-11-11-wave2.md`.

## Global Constraints

The wave index's Global Constraints (and wave 1's, which it keeps) apply: commit style with no trailers, failing test first, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, the task's tests, then the full `cargo test` before each commit, event waits, root's binaries by full path, published files in `<state>-public`, docs in the same task, side containers only. In addition, for this lane:

- Branch `claude/token-broker`, cut from `origin/docs/wave2-specs`. **No schema change** (`SCHEMA_VERSION` stays 18). No push, no PR.
- New crates: `rcgen` only. `rustls`, `tokio-rustls` and `webpki-roots` become direct dependencies at the versions already locked. `cargo deny check advisories bans` after the change.
- **A surrogate, a real token, `broker.key` and a CA or leaf private key never appear in a log, `Debug`, an audit row, a notice or an error.** Refusals name the token; every type that holds one has a hand-written `Debug`.
- Surrogate: `zsur_` + hex of the first 20 bytes of HMAC-SHA256(key, `kind` ‖ 0x00 ‖ `name`); `kind` is `token` or `env`. Key: 32 random bytes in `<state>/broker.key`, root, 0600, made on first use.
- Binding entries: `host` or `host:port` (443 when absent), a name through `egress::target`, never an IP literal, no `*`, scheme or path. Exact match only.
- Swapped: any request header value; `Authorization: Basic` after decoding; path and query only with `url = true`. Never a body. Reply headers get each real value this request carried turned back.
- Exact copy:
  - `403 zephyr: the vault token NAME is not sent to HOST`; `403 zephyr: unknown vault surrogate`; `403 zephyr: NAME is not swapped in URLs`; `403 zephyr: Host does not match HOST`; `502 zephyr: HOST did not prove it is HOST`; `502 zephyr: the vault could not be read`; `502 zephyr: the token broker is down`.
  - `not run: the token NAME is bound to no host; ask the owner with vault bind`.
  - `/vault bind` answers `HOST is now decrypted by zephyr for every program the agent runs` per host.
  - Notice, once per token and host per hour, 10 an hour in all: `[zephyr] the agent tried to send the vault token NAME to HOST, where it is not bound. Nothing was sent.`
  - Broker down, once: `[zephyr] the token broker is down, so vault tokens do not work: <error>`.
  - Upgrade, once (`meta` key `broker.upgrade_told`): `[zephyr] vault tokens now go to the agent as stand-ins, swapped for the real token only on hosts you name. Bind each: /vault bind NAME HOST... To hand one out as before: /vault raw NAME on.` followed by the tokens, and the left-out connectors with `/set mcp.NAME.hosts` and `/set mcp.NAME.secrets plain`.
  - Doctor under host networking: `token broker off (host networking): vault tokens reach the agent in plain`.
- Audit: session `net`, tool `swap`, args `NAME -> host:port`; `allowed` once per token and host per hour; refusals under the proxy's cap of 60 a minute.
- CA: `<state>/ca/` 0700, `ca.key` 0600, ECDSA P-256, `CA:TRUE` path length 0, 398 days, subject `zephyr token broker (this container only)`; replaced at a container start when absent or under 180 days left. Leaf: one P-256 key per daemon run, one SAN, from a day ago for 30 days, made again within a day of expiry.
- Brokering is on only under the floor (`egress::bridge()`), or when a test sets the hidden `Config::brokered`. Under host networking tokens and connector secrets are handed out as before.
- Side container: `ZEPHYR_CONTAINER=zephyr-test-a3`, image `zephyr-dev:a3-<sha>`, attach port 18790, web port 18793, a scratch HOME, the bridge network. Never `zephyr`, `zephyr-test`, `~/.zephyr*` or the host's tailscale. Remove the container and its network afterwards; keep the image.

## Review Focus

1. **A surrogate sent to an unbound host or a bound host's look-alike** is never swapped, and refused and audited where the proxy can see it. Tests in A3-5: plain `http://attacker/` with the surrogate in a header, in the query, as base64 in `Basic` and in the host name (403, zero accepts, a `refused` row by name, one notice); token B's surrogate to token A's host; `Host: attacker` on a decrypted connection; a client `SNI` for another name on a CONNECT to the bound name; a rebinding `lookup` that answers the attacker's address, where a certificate for another name is valid; a CONNECT tunnel to an unbound host carries the surrogate and never the value. Pure cases in A3-1 (`swap` table).
2. **A CA the agent planted** gains no trust in the proxy's upstream check. Test in A3-5: the upstream's certificate is from a CA that is in the test process's `SSL_CERT_FILE` and not in `Floor::roots`: `502`, the upstream reads no request. Test in A3-2: `Floor::live`'s roots are `webpki-roots` alone, whatever `SSL_CERT_FILE` says. In the side container (A3-8): as the agent, `ca.key` and `broker.key` cannot be read and the three trust locations cannot be written.
3. **A token whose value cannot go in a header** (a newline, a control byte): refused with the token's name, never sent cut or raw. Test in A3-1.
4. **A keep-alive connection that outlives an `/vault unbind`**: the next request on it is refused, because the map is read per request. Test in A3-5.
5. **`vault.toml` unreadable while the daemon runs**: bound hosts stay decrypted and answer `502`, never a plain tunnel that would carry a later real value. Test in A3-5.

---

### Task A3-1: Surrogates, bindings and the swap (`broker.rs`, pure)

**Files:**
- Create: `crates/zephyr/src/broker.rs` (`pub mod broker;` in `lib.rs`)
- Modify: `crates/zephyr/src/vault.rs` (`Entry` gains `hosts: Vec<String>`, `url: bool`, `raw: bool`, read by `read`, shown in `Debug`), `crates/zephyr/src/config.rs` (`Mcp` gains `hosts: Vec<String>`, `secrets: Option<String>`; hidden `brokered: bool` beside `tick`), `DESIGN.md` (new §4.21, its first paragraphs)

**Interfaces (produces):**
```rust
pub const PREFIX: &str = "zsur_";
pub fn key(cfg: &Config) -> anyhow::Result<[u8; 32]>;           // <state>/broker.key, made 0600 on first use
pub fn surrogate(key: &[u8; 32], kind: &str, name: &str) -> String;
pub fn parse_host(entry: &str) -> Result<String, String>;        // normalized "host:port"
pub fn on(cfg: &Config) -> bool;                                 // egress::bridge() || cfg.brokered
pub struct Secret { pub name: String, value: String, pub hosts: Vec<String>, pub url: bool }  // Debug: no value
pub struct Map { /* surrogate -> Secret, base64 forms, unreadable: bool */ }                 // Debug: names only
impl Map {
    pub fn build(cfg: &Config, vault: &Vault, env: &[(String, String)], key: &[u8; 32]) -> Map;
    pub fn load(cfg: &Config) -> anyhow::Result<Map>;            // reads vault.toml, secrets.env, broker.key
    pub fn bound(&self, host_port: &str) -> bool;
    pub fn hosts(&self) -> Vec<String>;                          // every decrypted host:port, sorted
}
pub enum Refused { NotBound(String), Unknown, NotInUrl(String), Unsendable(String) }
impl Refused { pub fn reason(&self, host: &str) -> String; pub fn token(&self) -> Option<&str> }
/// Swaps in place for a request the proxy decrypted for `host`; the names used, in order, each once.
pub fn swap(uri: &mut hyper::Uri, headers: &mut hyper::HeaderMap, map: &Map, host: &str) -> Result<Vec<String>, Refused>;
/// Turns each of `used`'s real values in reply headers back into its surrogate.
pub fn unswap(headers: &mut hyper::HeaderMap, map: &Map, used: &[String]);
/// A request that is never swapped (plain http, a host name): any surrogate refuses it.
pub fn scan(text: &[&[u8]], map: &Map) -> Result<(), Refused>;
pub fn secrets(cfg: &Config, vault: &Vault) -> Vec<(String, String)>; // Vault::secrets() + (name, surrogate)
pub fn connector_names(m: &Mcp) -> (Vec<String>, Vec<String>);        // ${NAME}s anywhere, and those in `url`
```
`Map::build`: every token that is not `raw` (one with no hosts is known and bound nowhere); for every `[[mcp]]` not `secrets = "plain"`, each `${NAME}` it uses that secrets.env sets, bound to the connector's `hosts`, or its `https` URL's `host:port` when it lists none, the union over connectors, `url` when the name is in a connector's `url`.

- [x] **Step 1: failing tests** in `broker.rs`:
  - `surrogates_are_stable_and_differ_by_name_kind_and_key`: matches `^zsur_[0-9a-f]{40}$`.
  - `parse_host_takes_exact_names_only`: `api.github.com` → `api.github.com:443`, `X.io:8443` → `x.io:8443`; errors for `*.x.io`, `https://x.io`, `x.io/path`, `1.2.3.4`, `[::1]`, `x.io:0`, ``.
  - `swap_table`: Bearer; a custom header; `Basic` (decoded, swapped, encoded again); two tokens in one request; the target with `url` and without (`NotInUrl`); a body is not an argument at all; token B on token A's host (`NotBound("B")`); a made-up `zsur_` + 40 hex (`Unknown`); a base64 form of an unbound token's surrogate in a header (`NotBound`); `unswap` turns a `Location` back.
  - `a_value_that_cannot_be_a_header_is_refused_by_name` (Review Focus 3): value `a\nb` gives `Unsendable`, headers unchanged.
  - `scan_refuses_any_surrogate`: header, query, host name, base64.
  - `nothing_printed_holds_a_value_or_a_surrogate`: `format!("{map:?}")` and every `Refused::reason` hold neither.
  - `secrets_scrub_a_surrogate_and_its_base64_form`: `vault::scrub(text, &secrets(..))` gives `[vault:NAME]`.
  - `the_map_binds_connector_secrets_to_their_hosts`: a `url` connector to its URL's host, `hosts` when listed, the union for a shared name, none for `plain`, `url` only when named in the URL.
  - `the_key_is_made_once_and_private`: 0600, the same bytes on a second call.
- [x] **Step 2:** `cargo test -p zephyr --lib broker` fails (no module).
- [x] **Step 3: implement.** One left-to-right pass finds `zsur_` followed by 40 lowercase hex; base64 forms come from `vault::base64_forms`.
- [x] **Step 4:** the tests pass; full `cargo test`.
- [x] **Step 5:** commit `feat(broker): derive a stand-in for each vault token and swap it only on the hosts it is bound to`.

### Task A3-2: The CA (`ca.rs`)

**Files:**
- Create: `crates/zephyr/src/ca.rs`
- Modify: `crates/zephyr/Cargo.toml` (`rcgen`, `rustls`, `tokio-rustls`, `webpki-roots`), `Cargo.lock`, `DESIGN.md` §4.21

**Interfaces (produces):**
```rust
pub const DAYS: i64 = 398; pub const RENEW_UNDER: i64 = 180;
pub fn dir(state: &Path) -> PathBuf;                             // <state>/ca
/// Makes or renews the CA; `true` when a new one was made.
pub fn ensure(state: &Path, now: i64) -> anyhow::Result<bool>;
pub fn days_left(state: &Path, now: i64) -> anyhow::Result<i64>;
pub fn pem(state: &Path) -> anyhow::Result<String>;
pub struct Authority { /* issuer, the run's leaf key, leaves by host */ }  // Debug: nothing
impl Authority {
    pub fn load(state: &Path) -> anyhow::Result<Authority>;
    pub fn leaf(&self, host: &str, now: i64) -> anyhow::Result<Arc<rustls::ServerConfig>>;  // ALPN http/1.1 only
}
pub fn public_roots() -> Arc<rustls::RootCertStore>;            // webpki-roots, nothing from a file or a variable
pub fn client(roots: Arc<rustls::RootCertStore>) -> Arc<rustls::ClientConfig>;              // ALPN http/1.1 only
```
The expiry is kept as unix seconds in `<state>/ca/expires`, written with the certificate; a missing or unreadable one reads as expired.

- [x] **Step 1: failing tests:** `ensure_makes_a_private_folder_and_key` (0700, 0600); `ensure_keeps_a_ca_with_200_days_left_and_replaces_one_with_100` (the key file's bytes change only in the second case); `a_leaf_verifies_for_its_host_and_no_other` (a TLS handshake over `tokio::io::duplex` with the CA as the client's only root: `api.x.test` succeeds, `other.test` fails); `the_same_leaf_is_served_until_a_day_before_it_expires` (`Arc::ptr_eq`, then a new one at 29 days); `leaves_offer_http1_only` (a client offering `h2, http/1.1` negotiates `http/1.1`); `the_proxys_roots_ignore_ssl_cert_file` (Review Focus 2: with `SSL_CERT_FILE` naming the test CA, `public_roots()` does not verify its leaf).
- [x] **Step 2:** fails. **Step 3:** implement; `cargo deny check advisories bans`. **Step 4:** tests, full `cargo test`.
- [x] **Step 5:** commit `feat(ca): keep a container-only CA and sign one leaf per bound host`.

### Task A3-3: The vault hands out surrogates and takes bindings

**Files:**
- Modify: `crates/zephyr/src/vault.rs` (`put`, `public`, `line`, `bind`/`unbind`/`set_flag`, `command`, `exec_env`, `agent`, `scrubbed`), `crates/zephyr/src/gate.rs` (`Scrub::load`), `crates/zephyr/src/router.rs` (`/vault` logs to `settings_log`), `crates/zephyr-policy/src/tools.rs` (`VaultAction::Bind`, the tool's schema text), `DESIGN.md` §4.13, §4.15, `README.md`
- Test: `vault.rs`, `tools.rs`, `crates/zephyr/tests/router.rs`

**Interfaces:**
- Consumes: A3-1 `broker::{key, surrogate, parse_host, on, secrets}`.
- Produces: `vault::command(path, arg, may_approve) -> Result<(String, Option<Change>), String>` with `pub struct Change { pub key: String, pub old: String, pub new: String }` (`vault.NAME.hosts|url|raw`); `VaultAction::Bind { name, hosts: Vec<String>, why }`; `exec_env` unchanged in signature.
- `put`: `hosts` from the new entry when it lists any, else the old ones; `url` and `raw` always the old ones (false on a new entry).
- `bind`, `raw on`, `url on` need `may_approve`; `unbind [HOST]`, `raw off`, `url off` do not.
- The agent's `bind` checks the token exists and every host parses, stores `/vault bind NAME HOST..` with `ask_owner` (the ask reads `to send NAME to HOST, HOST`), and answers that it waits.

- [x] **Step 1: failing tests:** `exec_env_gives_a_surrogate_a_raw_value_or_nothing` (brokered config: bound → `zsur_…`, `raw` → the value, unbound → the `not run` copy; not brokered → the value); `put_keeps_hosts_across_a_new_value`; `bind_raw_and_url_need_an_approving_channel_and_tightening_does_not`; `the_agents_bind_is_a_proposal_the_owner_approves` (`/ok N` applies it); `each_binding_change_is_a_settings_log_row`; `scrubbed_output_hides_a_surrogate`; in `tools.rs`, `vault_bind_needs_a_name_and_hosts`; in `tests/router.rs`, `an_exec_with_a_bound_token_runs_on_its_surrogate` (the answer shows `[vault:GH_TOKEN]`, the `.procs` log holds `zsur_` and not the value), `a_token_bound_to_nothing_is_not_run`, and `the_upgrade_notice_is_sent_once`.
- [x] **Step 2–4:** fail, implement, pass, full `cargo test`.
- [x] **Step 5:** commit `feat(vault): hand the agent a stand-in for a bound token, and let only the owner bind it to hosts`.

### Task A3-4: Connectors hold surrogates

**Files:**
- Modify: `crates/zephyr/src/drafts.rs` (`prepare`, the draft's `hosts`, `/mcp ok`'s card), `crates/zephyr/src/config.rs` (`Mcp::check`), `crates/zephyr/src/settings.rs` (two rows, `find`, `value`, `read`, `write`, `parse`), `crates/zephyr/src/broker.rs` (`connector_secrets`), `DESIGN.md` §4.4 item 1, §4.6, §4.17, §7

**Interfaces:**
- Produces: `broker::connector_secrets(cfg: &Config, m: &Mcp, env: &[(String, String)]) -> Result<Vec<(String, String)>, String>`: the list `drafts::filled` gets. Real values when brokering is off or the connector is `plain`; surrogates otherwise; `Err(why)` for a connector that names a secret and has nowhere to send it (a `command` connector with no `hosts`, or an `http://` URL with none).
- Rows, verbatim from spec §4.11: `mcp.<name>.hosts` (`Kind::Text { max: 1000 }`, `Access::Propose`, strong, `Stricter::NotOrdered`) checked by `broker::parse_host`, and `mcp.<name>.secrets` (`Kind::Choice(&["surrogate", "plain"])`, `Access::Propose`, strong, `Stricter::Earlier`).

- [x] **Step 1: failing tests:** `mcp_json_holds_the_surrogate_and_never_the_secret`; `a_command_connector_without_hosts_is_left_out`; `a_plain_connector_holds_the_value`; `mcp_json_is_byte_identical_across_two_prepares`; `without_the_floor_connectors_hold_their_secrets_as_before`; `a_draft_carries_hosts_and_the_card_shows_them`; in `settings.rs`, `connector_hosts_and_secrets_are_strong_proposal_rows` and `a_wildcard_connector_host_is_refused`.
- [x] **Step 2–4:** fail, implement, pass, full `cargo test`.
- [x] **Step 5:** commit `feat(drafts): publish connectors with stand-ins for their secrets, bound to the connector's hosts`.

### Task A3-5: The proxy decrypts bound hosts and refuses a surrogate anywhere else

**Files:**
- Modify: `crates/zephyr/src/egress.rs` (`Floor` gains `roots: Arc<rustls::RootCertStore>` and `ca: Option<Arc<ca::Authority>>`, so it is `Clone`, not `Copy`; `Conn::answer`; `serve_tls`; `forward` scans; `Audit::note`; `write_audit` sends notices), `crates/zephyr/src/broker.rs` (`Maps`, the notice limiter), `DESIGN.md` §4.20 "Not done", §4.21

**Interfaces:**
- Consumes: A3-1 `Map`, `swap`, `unswap`, `scan`, `Refused`; A3-2 `Authority::leaf`, `client`, `public_roots`.
- Produces: `broker::Maps` (`current(&self, cfg: &Arc<Config>) -> Arc<Map>`: one `stat` of `vault.toml` and secrets.env per call; a load that fails keeps the last map with `unreadable` set); `broker::Notices` (`fn due(&mut self, now: i64, token: &str, host: &str) -> bool`: once per pair per hour, 10 an hour).
- `Conn::answer`, after `decide` and `connect`: any surrogate in the authority refuses; a `CONNECT` to a bound `host:port` answers `502 zephyr: the token broker is down` with no CA, `502 zephyr: the vault could not be read` with an unreadable map, else `200` and `serve_tls`. `forward` runs `scan` over the target and every header first.
- `serve_tls`: accept TLS with `leaf(host)`; a client SNI that is not the bound host answers every request `403 zephyr: Host does not match HOST`; TLS to the connected address with SNI the bound host and `Floor::roots`, a failure answering every request `502 zephyr: HOST did not prove it is HOST`; then hyper `http1` on both sides with keep-alive, 10 s for a head, an idle connection closed after 90 s. Per request: the current map (unbound now → `403 … is not sent to HOST`), `Host` equal to the bound host with or without its port, `swap`, send, `unswap` the reply's headers, bodies as frames; an `Upgrade` answered `101` is relayed by `relay`.
- Audit: `swap` rows `NAME -> host:port`, `allowed` keyed on the whole pair per hour; refusals as `refused: <reason>`.

- [x] **Step 1: failing tests** (tokio, a test CA as `Floor::roots`, a TLS upstream on 127.0.0.1 with a leaf for `bound.test`, `lookup` passed in, the client trusting the proxy's CA):
  - `a_bound_host_gets_the_real_token_and_never_the_surrogate`; `an_unbound_host_is_a_plain_tunnel_with_its_own_certificate`.
  - `two_requests_share_one_upstream_connection`; `a_chunked_upload_and_an_event_stream_pass_unbuffered` (the second event is read before the upstream sends the third); `a_websocket_handshake_is_swapped_and_frames_pass`; `a_client_offering_h2_gets_http1`.
  - `a_swap_is_audited_once_an_hour_by_name`.
  - Review Focus 1: `a_surrogate_on_plain_http_is_refused_audited_and_told_once` (header, query, `Basic`, host name: 403, zero accepts, one `refused` row naming the token, one notice, none for a second try); `another_tokens_surrogate_on_a_bound_host_is_refused` (the upstream reads no request); `a_host_header_for_another_name_is_refused`; `a_client_sni_for_another_name_is_refused`; `a_tunnel_to_an_unbound_host_carries_the_surrogate_not_the_value`; `a_rebound_name_with_a_private_address_is_refused` (and `lookup` is called once); `an_attackers_address_with_another_names_certificate_gets_502` (the attacker reads no request).
  - Review Focus 2: `a_ca_the_agent_planted_gains_no_trust` (`SSL_CERT_FILE` set to the planted CA's bundle: `502 … did not prove it is …`, the upstream reads no request, a `refused` row).
  - Review Focus 4: `an_unbind_stops_the_next_request_on_an_open_connection`. Review Focus 5: `an_unreadable_vault_answers_502_on_bound_hosts`.
  - `without_a_ca_bound_hosts_answer_502_and_others_still_tunnel`.
- [x] **Step 2–4:** fail, implement, pass, full `cargo test`.
- [x] **Step 5:** commit `feat(egress): decrypt only the hosts a token is bound to and swap its stand-in there, refusing it everywhere else`.

### Task A3-6: The agent's tools trust the CA; supervise, doctor and the image

**Files:**
- Modify: `crates/zephyr/src/agent.rs` (`CA_ENV`, `proxy_env`), `crates/zephyr/src/ca.rs` (`publish`, `trust_chromium`), `crates/zephyr/src/supervise.rs` (`Policy::ca`), `crates/zephyr/src/main.rs` (`run` loads the `Authority`, runs `certutil` as the agent, sends the upgrade and broker-down notices; `doctor`), `crates/zephyr/src/egress.rs` (`netcheck`: `ca.key` and `broker.key` unreadable, the trust locations unwritable), `deploy/Dockerfile` (`libnss3-tools`, Camoufox `distribution/policies.json`), `crates/zephyr/tests/host.rs`, `DESIGN.md` §4.8, §4.21, `README.md`, `CLAUDE.md`

**Interfaces:**
- `agent::CA_ENV: &[(&str, &str)]` = `NODE_EXTRA_CA_CERTS=/var/lib/zephyr-public/ca.pem`, `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE` = `/etc/ssl/certs/ca-certificates.crt`, part of `proxy_env`.
- `ca::publish(state: &Path, public: &Path) -> anyhow::Result<()>`: `/usr/local/share/ca-certificates/zephyr.crt`, root's `/usr/sbin/update-ca-certificates --fresh`, `<public>/ca.pem` 0644.
- `Policy { ca: Option<fn() -> anyhow::Result<()>> }`: run after the floor; an error is printed and the daemon starts.
- `broker::doctor(cfg, now) -> Vec<(Level, String)>` with `Level::{Ok, Warn, Fail}`: spec §4.10's list.

- [x] **Step 1: failing tests:** `the_proxy_env_points_tools_at_the_ca`; `host.rs` `shell_…` expects the three variables in bridge mode and none in host mode; `a_failing_ca_does_not_stop_the_daemon` (supervise); `doctor_lists_decrypted_hosts_and_warns_about_plain_secrets` (raw, url, plain, unbound, `[agent] env` names); `doctor_fails_when_mcp_json_holds_a_brokered_secret`; `every_copy_in_the_dockerfile_is_in_the_build_context` still passes.
- [x] **Step 2–4:** fail, implement, pass, full `cargo test`.
- [x] **Step 5:** commit `feat(ca): have the agent's tools trust the broker's CA, renew it only at a container start, and show the broker in doctor`.

### Task A3-7: The owner's forms and the host CLI

**Files:**
- Modify: `crates/zephyr/src/host/setup.rs` (`vault add token NAME --host HOST..`, `edit NAME --host`, `init`'s token step), `crates/zephyr/src/tui/overlays.rs` and `tui/conn.rs` (a hosts field on the token form, hosts on the entry's card), `README.md`, `DESIGN.md` §4.15
- Test: `crates/zephyr/tests/host.rs`, `tui/overlays.rs` tests, `crates/zephyr/tests/attach.rs`

- [x] **Step 1: failing tests:** `vault_add_token_takes_hosts` (`vault.toml` holds the normalized hosts; a wildcard fails and writes nothing); `the_token_form_saves_its_hosts`; `vault_put_over_attach_keeps_url_and_raw`.
- [x] **Step 2–4:** fail, implement, pass, full `cargo test`.
- [x] **Step 5:** commit `feat(vault): take a token's hosts where the owner saves it, in the TUI and on the host`.

### Task A3-8: The real container, the real rpi

**Files:**
- Create: `crates/zephyr/tests/token_broker.rs` (ignored; skips without `ZEPHYR_CONTAINER`, refuses `zephyr`)
- Modify: `crates/zephyr/tests/real_rpi.rs` (does rpi's own client connect through a decrypted host?), `DESIGN.md` §4.21 "Tested" and the pinning table, this plan (results)

- [x] **Step 1:** write the ignored test: as the agent in the side container, `env` under an `exec` with `secrets` holds `zsur_`; `mcp.json` holds no secrets.env value; `ca.key` and `broker.key` cannot be read; `/usr/local/share/ca-certificates`, `/etc/ssl/certs` and `<state>-public/ca.pem` cannot be written; `curl`, `git ls-remote`, Node `fetch`, Python `requests` and agent-browser reach a bound host whose echo shows the real token; the same token to another host is refused and audited; an unbound host shows its own certificate's issuer; `doctor` passes and lists the host.
- [x] **Step 2:** `docker build -f deploy/Dockerfile -t zephyr-dev:a3-<sha> .`; `zephyr up` as `zephyr-test-a3` with a scratch HOME, attach 18790, web 18793; run the test; remove the container and its network.
- [x] **Step 3:** `ZEPHYR_REAL_RPI=1 cargo test -p zephyr --test real_rpi -- --ignored`, `-p zephyr-policy --test rpi_load -- --ignored`, `--test attach -- --ignored`.
- [x] **Step 4:** record the results and every deviation here; commit `test(broker): check the token broker in a real container and with the real rpi`.

## Deviations

Each is in the code and in DESIGN §4.21 as built.

**Across tasks**
- The tests of A3-1, A3-2 and A3-6 were written in the same step as their code, with no separate failing run; A3-3, A3-4, A3-5 and A3-7 had the failing run first.
- Under the machine's load from the four lanes, four timing tests outside this lane failed once each and passed when run again: `memory::…a_hanging_filter_is_killed` (a 2 s bound), two of `tests/cli.rs` (a reply before a re-exec; exact ages in `status`) and `tests/web.rs` `a_410_drops_the_subscription`. One of this lane's own, `ca::tests::publish_…`, failed once on a just-written script being busy while another test forked, and now retries that.

**A3-1**
- Brokering is switched by the existing hidden `Config::floor` (`broker::on`), not a new `Config::brokered`.
- `[[mcp]] hosts` is one comma-separated string, as the registry row's `Kind::Text` and `network.allow` are, not a TOML array. A vault token's `hosts` is an array.
- A fourth refusal, `Unsendable`: a value that cannot be written into a header or a URL is refused by name. A `zsur_` not followed by a known surrogate (cut short, made up) is `Unknown`.
- `drafts::filled` changed by two lines: it leaves `hosts` and `secrets` out of `mcp.json`.

**A3-2**
- The CA's expiry is kept as unix seconds in `<state>/ca/expires`, written with the certificate, not parsed out of it (`shortcut:`); missing or unreadable reads as no CA.
- A leaf is valid from midnight UTC of the day before, not from an hour ago: `rcgen` takes dates, and `time` is not a dependency of its own.
- The Review Focus 2 test in `ca.rs` does not set `SSL_CERT_FILE`: it would be a process-wide change in a parallel test binary. It checks that `public_roots()` is exactly `webpki-roots` and that a CA made in the test is not trusted by it; nothing on the proxy's path reads a variable or a file.
- `Cargo.lock` lists `x509-parser` and its dependencies as `rcgen`'s optional ones; they are not compiled (`cargo tree` shows none). `cargo deny check advisories bans` passes.

**A3-3**
- The agent's `bind` takes `{name, hosts}` and no `why`: the owner's notice carries no text of the agent's.
- The upgrade notice lists `/vault bind NAME HOST..` per token (the daemon cannot know the hosts) and is marked told at the first tick with an owner, also when there was nothing to tell.
- `vault::command` returns `(text, Option<Change>)`; the router writes the `settings_log` row.

**A3-4**
- A connector on a plain `http://` URL that names a secret is left out like a `command` connector with no hosts: the proxy never swaps on plain HTTP.
- A draft cannot carry `secrets`, and an approved update keeps the owner's `secrets`, and the owner's `hosts` when the draft lists none.
- `zephyr mcp add` takes `--host HOST..` and `--plain`. The settings panel shows two more rows per connector.

**A3-5**
- One timer, 90 s, covers a request head and the wait for the next request on a decrypted connection (`shortcut:`), not 10 s and 90 s.
- A client handshake naming another host (SNI) is answered `403 zephyr: Host does not match HOST` on every request (wave Review Focus 1; the spec names only the `Host` header). A request with no `Host` or two is refused the same way.
- No CA and an unreadable vault are answered on the `CONNECT` itself (`502`), since without a CA there is no TLS to answer inside; the unreadable vault is also answered per request on an open connection.
- When the upstream connection ends, the client's is closed after the reply in flight; the proxy does not connect again on its own.
- An audit row's refusal says `this host` (the destination is in its args); the status line names the host, or `that host` when the name itself holds a stand-in.
- The owner's notice leaves with the router's next tick (5 s), not at once (`shortcut:`).

**A3-6**
- The agent-side checks (`ca.key`, `broker.key` unreadable; the three trust locations unwritable) run inside `zephyr __netcheck`, which `doctor` now gives the state and public folders.
- Camoufox's `Certificates.Install` is merged into the `policies.json` Camoufox ships (with `jq`), not written as a new file.

**A3-7**
- The web app has no token form (`vault_put` is the attach port's, used by the TUI), and `zephyr init` has no vault step: nothing was added there. `zephyr vault add token` asks for hosts at a terminal when no `--host` is given.
- The TUI's token form is name, value, hosts, so Enter on the value moves to the hosts.

**A3-8**
- The real-container test binds a connector secret to a public echo host (httpbin.org; `ZEPHYR_TEST_ECHO` names another), not to a local TLS server: by decision 5 the proxy trusts only public roots upstream, and there is no way to give it another.
- The real-rpi question was answered in the side container rather than in `tests/real_rpi.rs`, which runs outside a container and has no proxy: see the results.

## Results (2026-11-11, image `zephyr-dev:a3-dc07405`, container `zephyr-test-a3`, attach 18790, bridge)

- `cargo test --no-fail-fast`, last full run: 1193 passed, 1 failed, 19 ignored (lib 662, router 193, zephyr-policy 86, host 53, web 45, attach 33, and the rest). The one failure was `tests/cli.rs` `status_shows_the_main_child…`, which asserts exact ages in seconds and passed 22 of 22 in each of three runs of that file alone; see the load note above.
- Real rpi 0.21.2, `ZEPHYR_REAL_RPI=1`: `real_rpi` 5 passed, `rpi_load` 10 passed, `attach` (ignored) 1 passed.
- `tests/token_broker.rs -- --ignored`: passed. `tests/net_floor.rs -- --ignored`: passed against the same container.
- As the agent: `mcp.json` and `env` hold only `zsur_…`; an `exec` with `secrets` (driven by the scripted rpi stand-in, since a scratch install has no provider credentials) had `A3_TOKEN=zsur_…` in its environment, shown to the model as `[vault:A3_TOKEN]`.
- On the bound host the echo saw the real secret from curl, Node `fetch`, Python `urllib` and `requests`, Chromium (agent-browser) and Camoufox; git got an HTTP answer under the broker's certificate. The certificate's issuer there is `zephyr token broker (this container only)`.
- Sent to `http://example.com/`: `403 zephyr: the vault token … is not sent to example.com:80`, one `swap … refused` audit row, and the owner's notice on the chat channel. Sent to an unbound HTTPS host (postman-echo.com) it arrived as the stand-in.
- An unbound host (`example.com`) showed its own certificate's issuer.
- `ca.key` and `broker.key`: `Permission denied`. Writing to `/usr/local/share/ca-certificates`, `/etc/ssl/certs` and `/var/lib/zephyr-public/ca.pem`: `Permission denied`. `doctor`'s broker lines were all `ok`, and it listed the decrypted host.
- The CA's key was the same after a container restart.
- rpi's own client: with `api.anthropic.com` decrypted, `rpi -p` failed with `http transport error`; tunnelled again it got the provider's `401` for the dummy key. So spec open question 3(b) stays closed.
- Seen and written down in §4.21 "What it does not stop": the echo host returned the real token in its reply body, which `exec`'s answer scrubbed and the command's `.procs` log did not. Closed in the follow-up below.
- The side container and its network were removed; the image was kept.

## Follow-up: the echo leak (2026-11-11, asked for by the coordinator after the first report)

The first build left a bound host that sends the token back as a documented limit. It defeats the broker's purpose, so it was closed instead. Spec §4.3 ("Reply bodies stream through untouched") no longer holds.

- [x] **Replies from a decrypted host are swapped back** (`broker::Replacer`, `Map::unswapper`, `egress::Unswapped`): every value bound to that host, in headers, body and trailers, carried by this request or not. `Accept-Encoding: identity` replaces the client's; a reply that is encoded all the same is refused (`502 zephyr: HOST sent a reply zephyr cannot read (compressed)`, an audit row). The body streams, holding back only an end that could be the start of a value; `Content-Length` is dropped, so such replies reach the client chunked. Tests: `broker::tests::a_replacer_finds_a_value_however_the_stream_is_cut` (every cut point), `a_reply_gets_back_only_the_values_bound_to_its_host`, `egress::tests::decrypted::an_echo_from_a_bound_host_shows_the_stand_in_never_the_token` (a header, a body with a length, a chunked body cutting the value in two, a value this request did not carry, a gzip reply refused). The failing run came first, and it caught a real fault: a short value that is the start of a longer one was replaced too early.
- [x] **A command's log is scrubbed as it is written** (`vault::scrub_stream`, `zephyr __scrublog`, `procs::spawn`, the hidden `Config::scrubber`). A process of the daemon's, not a task in it, so a command still outlives a re-exec or a crash. Tests: `vault::tests::a_commands_output_reaches_its_log_without_a_vault_value`, and against the real binary `tests/cli.rs` `a_commands_log_never_holds_a_vault_value`. The gateway answers, `process log` and the reports were already scrubbed where they are written; the log was the only agent-readable file holding raw output.
- [x] **`tests/net_floor.rs`** finds the daemon after a re-exec too (`/proc/self/exe run`).
- [x] DESIGN §4.13, §4.15, §4.21 and the README say what is still not covered.

Deviations and limits of the follow-up:
- In-process tests (`tests/router.rs`, the unit tests) leave `Config::scrubber` unset and write logs directly, as before; the scrubber is exercised by the real binary in `tests/cli.rs` and in the side container. The router test `an_exec_with_a_bound_token_runs_on_its_surrogate` still reads the stand-in from the log, which is not a vault value.
- A held-back start of a value is let go after 250 ms with nothing more, in the log only; in a reply body it waits for the next frame or the end.
- A scrubber that ends after a daemon re-exec stays a zombie until the daemon exits, and the wrapper then waits its full second (`shortcut:`).
- Websocket frames after a `101` are still relayed unread, and a value sent back transformed (base64 in a page, hex, escaped) is not found by the proxy: both are in §4.21 "What it does not stop".
- The docs of both fixes are in the first commit of the three.

Results (image `zephyr-dev:a3-dc0115b`, container `zephyr-test-a3`, attach 18790, bridge; removed afterwards with its network, both images kept):
- `cargo test --no-fail-fast`: 1199 passed, 0 failed, 19 ignored. `clippy -D warnings` clean, `cargo deny check advisories bans` ok.
- `tests/token_broker.rs -- --ignored` passed: httpbin's `/basic-auth` answered 200 only for the real secret behind the stand-in, and `/get` showed the agent's curl, Node, Python and Chromium the stand-in where it used to show the secret.
- The agent's `curl -H 'Accept-Encoding: gzip'` on the echo: `"X-Key": "zsur_…"`, `"key": "zsur_…"`, and the server saw `Accept-Encoding: identity`.
- `https://httpbin.org/gzip`, which compresses whatever is asked: `502 zephyr: httpbin.org sent a reply zephyr cannot read (compressed)`, with a `swap … refused` audit row.
- `exec` with the bound vault token (scripted rpi stand-in): the answer showed `[vault:A3_TOKEN]`, the `.procs` logs held `A3_TOKEN=zsur_…` and `"X-Key": "zsur_…"`, and no file under `/zephyr` held either real value.
- `tests/net_floor.rs -- --ignored` passed twice: on a fresh daemon, and after the daemon had re-exec'd itself.

## Follow-up: the review of PR #48 (2026-11-11)

Each finding was confirmed against the code before it was changed; none was skipped. The failing tests came first for the leaks (`egress::tests::decrypted::a_token_sent_back_in_another_form_does_not_reach_the_agent`, `no_websocket_is_carried_on_a_bound_host`, `a_request_with_two_framings_reaches_the_server_as_one`); the rest were written with their fix. All of it is one commit, since the changes share `broker.rs`, `egress.rs` and `router.rs`.

- [x] **1. Basic echo.** `swap` returns what it sent in another form (`Swapped::sent`: the Basic payload, with the one the agent's program wrote), and a reply's unswap set is those, each bound value, and each value's `vault::base64_forms`.
- [x] **2. Reason phrase.** A decrypted reply's own status text is dropped.
- [x] **3. `/vault bind`, `url on`, `raw on`** need a channel in `settings.strong_channels`, and so does the `/ok` of the agent's `bind` proposal (`Router::not_now`). Tightening is taken anywhere.
- [x] **4. Drafts.** A connector draft can no longer set `hosts` (nor `secrets`); an approved update keeps the owner's. The agent proposes hosts with `gateway /set mcp.NAME.hosts`, which already needs a strong channel.
- [x] **5. `url` connectors.** Tested for real in `zephyr-test-a3` with rpi 0.21.2 and a scripted model that had rpi call `mcp_list`: against an HTTPS connector on a decrypted host the tool answered `MCP HTTP request failed: error sending request`; with the host a tunnel again it reached the server (`MCP message must use JSON-RPC 2.0`, httpbin not being an MCP server). So rpi's MCP client does not trust the CA. A `url` connector now keeps its real secret in `mcp.json` and its host stays a tunnel, as before the broker, unless the owner sets `secrets = "surrogate"`; `doctor` warns either way. An upgrade changes nothing for it.
- [x] **6. Range.** `Range` and `If-Range` are removed, and a `206` is refused (`502`, an audit row).
- [x] **7. Hop-by-hop.** Removed on the request and the reply; an `Upgrade` goes no further and a `101` is refused, so no websocket is carried on a decrypted host (which also closes the unread-frames gap). The upstream connection is one per client connection, made at its `CONNECT`, not from a pool. A request with both a length and a chunked body reaches the server as one request, and hyper closes the client connection after it.
- [x] **8.** `broker.upgrade_told` is set only once the vault was read and the notice stored.
- [x] **9.** `Maps::current` also keys on inode and ctime.
- [x] **10.** The upstream `ClientConfig` is built once (`Floor::upstream`, in place of `Floor::roots`); the live config is compared by pointer.

Changed with these: `settings::value` shows `mcp.NAME.secrets` and `.hosts` as they are in effect; the real-container test sets its `url` connector to `surrogate` to have a stand-in bound to the echo host.

Results (image `zephyr-dev:a3-2a41270`; container and network removed afterwards):
- `cargo test --no-fail-fast`: 1233 passed, 0 failed, 20 ignored. `clippy -D warnings` clean, `cargo deny check advisories bans` ok.
- Real rpi 0.21.2: `real_rpi` 6, `rpi_load` 10, ignored `attach` 1, all passed.
- `tests/token_broker.rs` and `tests/net_floor.rs`, ignored, both passed in the side container. `curl -u x:$ZSUR https://httpbin.org/get` showed the Basic credential as the agent wrote it and no trace of the real one; `/basic-auth` still answered 200 for the real secret behind the stand-in; a request with `Upgrade: websocket` was answered as a plain request.

## Follow-up: the final review of 2a41270 (2026-11-11)

Each finding was confirmed first; none was skipped. Failing tests came first for the proxy's behaviour (`a_reply_that_closes_upstream_closes_the_clients_connection`, `trailers_reach_the_client_with_their_values_turned_back`); the rest changed signatures or types, so their tests were written with the fix. One commit, as the changes share `broker.rs`.

Deviations from the spec made here, on the review's word:
- **Spec §4.3 "Any request header value: always swapped" no longer holds.** A stand-in is swapped only in `Authorization`, in the headers the owner listed for a token (`headers` on `[[token]]`, `/vault header`, `--header`), and for a connector's secret in the headers its own `[[mcp]] headers` put it in. In any other header the request is refused (`403 zephyr: NAME is not swapped in the header HEADER`), audited and told to the owner. A `command` connector whose program sends its secret in another header must be `plain`: there is no per-connector list.
- **Spec §4.2 "A `url` connector with no `hosts` is bound to its URL's host … needs nothing from the owner" no longer holds**, and neither does the implicit `plain` of the commit before. `plain` is only ever the owner's written word; a remote connector with a secret and none is not published; the upgrade writes `plain` once for those that were there.
- `/mcp ok` takes a `strong` argument: a draft that would make a connector hold a real secret, when it is new, names a further secret or moves its URL to another host, is approved from a strong channel only.
- `broker::shared`: one secrets.env name cannot be a plain connector's and another's stand-in. It is not a hard config error (the daemon still starts on a hand-edited file): neither connector is published, `/set` and `zephyr config set` refuse to make it so, `doctor` fails, and the owner is told at each start.

- [x] **1–3. Connector secrets**, as above; `Router::settle_remote_connectors`, `broker::{plain, undecided, unsettled, shared}`; `zephyr mcp add --url` with a secret asks for `--plain`.
- [x] **4, 9. Encoded echoes.** `broker::encodings`, worked out once per secret in `Map::build`: base64, JSON escapes (`\/`, `\uXXXX` in either case), percent-encoding (everything, a URI component, a form field; either case). The limits are in DESIGN §4.21.
- [x] **5.** Credential headers only, as above.
- [x] **6.** The web app needs a passkey for `/vault bind`, `header`, `url on` and `raw on` whatever `strong_channels` says.
- [x] **7.** A reply whose server closes says `Connection: close` to the client too.
- [x] **8.** `Trailer` is no longer stripped; trailers reach the client with their values turned back (the proxy sends its own `TE: trailers`).
- [x] **10.** The router tests wait on the heartbeat's ticks (`ticks`), in both places a sleep stood.

Not done with these: a hosts or headers field for the new `headers` list in the TUI's token form (the chat command and the host CLI set it).

Results (image `zephyr-dev:a3-cd25027`; container and network removed afterwards):
- `cargo test --no-fail-fast`: 1243 passed, 0 failed, 20 ignored. `clippy -D warnings` clean, `cargo deny check advisories bans` ok.
- Real rpi 0.21.2: `real_rpi` 6, `rpi_load` 10, ignored `attach` 1, all passed.
- `tests/token_broker.rs` and `tests/net_floor.rs`, ignored, both passed in `zephyr-test-a3`. Item 5: `curl -A "probe $ZSUR"` got `403 zephyr: A3_SECRET is not swapped in the header user-agent`, with its audit row. Item 4: a secret holding a quote and a slash, sent in `Authorization` and echoed JSON-escaped by httpbin, came back as its stand-in, with no trace of the secret.

## Follow-up: the third review, of cd25027 (2026-11-11)

Each finding was confirmed; none was skipped, and one was done another way than asked (3).

- [x] **1, 2, 4. `/mcp ok` and secrets.** A draft whose connector names a `${SECRET}`, or that replaces a connector that does, is approved from a strong channel only, whatever it changes. A weak channel approves only drafts with no secret in them, old or new. An approval never writes `secrets = "plain"`. The `exposes`/`widens` logic and its host closure are gone. Tests: `drafts::tests::a_draft_that_carries_a_secret_is_approved_only_from_a_strong_channel` (the attacker-URL draft on a surrogate connector, a `User-Agent` header draft, a changed draft of an undecided connector, a command connector's command and environment, a dropped secret, a new connector; an identical draft of an undecided connector is "already approved as drafted", writes nothing and leaves it unpublished), and from Discord through the router, `a_draft_with_a_secret_is_not_approved_from_discord`.
- [x] **3. `settle_remote_connectors` only on an upgrade. Deviation from what was asked:** it is not gated on `store.upgraded_from()`. That mark is written only when a database moves up from before schema v16, and the broker adds no schema step, so an install upgraded from 0.8.0 (v18) would never carry it: its remote connectors would go unpublished, the silent break this migration exists to prevent. Instead `Store::open` writes `broker.born` into `meta` when it makes a database, and the migration runs only where that mark is missing (`Store::made_with_broker`). A first start and a reset state.db both have the mark, so they settle nothing. Tests: `the_upgrade_keeps_remote_connectors_as_they_were_and_settles_no_later_one` (a database without the mark), `a_fresh_install_settles_no_remote_connector`.
- [x] **5.** The refusal of a stand-in in a header that is no credential's says `in a header that is not a credential header` in the 403, the audit's outcome and the notice; the header's name is only in the audit's args. Notices are per token and host.
- [x] **6.** `broker::json_forms`: one form per kind of encoder (slash three ways; HTML characters, line separators, non-ASCII and the quote each escaped or not; hex in either case), at most 96, tested with a worst-case value.
- [x] **7.** A reply whose header name holds a bound value or a form of it is refused with 502 and an audit row; a trailer's ends the body there, with its row.
- [x] **8.** `broker::creates_pair(old, new)` compares the pairs before and after, in `/set`, `zephyr config set`, `zephyr mcp add` and `/mcp ok`. An unrelated edit passes while an old pair exists.
- [x] **9, 10.** DESIGN's step 4 no longer lists `Trailer`; §4.15 has `/vault header`, `/vault unheader` and `--header`; the `settings.rs` comment is corrected; the redundant `dedup` in `Vault::header` is gone.

Results (image `zephyr-dev:a3-0195eac`; container and network removed afterwards):
- `cargo test --no-fail-fast`: 1248 passed, 0 failed, 20 ignored. `clippy -D warnings` clean, `cargo deny check advisories bans` ok.
- Real rpi 0.21.2: `real_rpi` 6, `rpi_load` 10, ignored `attach` 1, all passed.
- `tests/token_broker.rs` and `tests/net_floor.rs`, ignored, both passed in `zephyr-test-a3`; its fresh state.db carried `broker.born` and an empty `broker.remote_kept`.

## Follow-up: the fourth review (2026-11-11)

It found no general path to a secret. Seven narrow fixes, each confirmed first; the failing tests came first for 2, 3, 5 and 6.

- [x] **1.** The upgrade that settles remote connectors also writes `[broker] settled = 1` into `zephyr.toml`, in the same edit, and runs only while state.db lacks `broker.born`, lacks `broker.remote_kept` and the file lacks the mark. A state.db from before the broker put back later settles nothing more (`the_upgrade_keeps_remote_connectors_as_they_were_and_settles_no_later_one` now deletes every `broker.*` mark before its second start). On an upgrade with nothing to settle the file still gets the mark.
- [x] **2.** `json_forms`: `& < >` and `'` are separate choices, so Go's form (`p\u0026ss'w0rd-xyz`) is among them; DEL is escaped only with what is not ASCII. Every combination is generated, at most 192, rather than a list of named encoders: the named ones (serde, JavaScript, Python, PHP, Go, .NET) are each one of the combinations, and an encoder not thought of is likelier to be one too.
- [x] **3.** `creates_pair` compares the secrets' names held two ways, not the connector tuples.
- [x] **4.** After a strong `/mcp ok` of a remote connector that is still unsettled, the reply names `/set mcp.NAME.secrets plain` and `zephyr mcp add NAME --plain`, not `zephyr mcp add NAME --secret X`, which is refused for it.
- [x] **5.** `Store::made_with_broker` counts a mark it cannot read as there, and logs it.
- [x] **6.** `Map::in_names` looks only for values, and forms, of 8 characters or more.
- [x] **7.** `zephyr mcp add` checks the new config alone when the one before did not load.

Results: `cargo test --no-fail-fast` 1252 passed, 0 failed, 20 ignored; `clippy -D warnings` clean; `cargo deny check advisories bans` ok; real rpi 0.21.2 `real_rpi` 6, `rpi_load` 10, ignored `attach` 1. The side container was not run again for these.
