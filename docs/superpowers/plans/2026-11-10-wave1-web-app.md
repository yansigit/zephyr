# Web app (W) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A phone-first web app served by the daemon behind `tailscale serve`: chat, approval cards, settings and devices over the attach protocol on a WebSocket, sign-in with passkeys, every approval signed over what it approves, and Web Push for new cards and replies.

**Architecture:** `src/web/mod.rs` is a hyper listener on port 7793, beside `attach::serve` in the router's `select!`. It checks Host, Origin and the Tailscale header, serves embedded files, runs the passkey ceremonies, and upgrades `/api/ws` into an unchanged `attach::Conn` on channel `web`. The cookie alone is the weak `web` channel. A decision carrying a passkey assertion, verified against a challenge that commits to the proposal's digest and card, acts as the strong `passkey` channel. The router does the last comparison on its own loop. `web/passkey.rs` (WebAuthn ES256) and `web/push.rs` (VAPID + aes128gcm) are pure functions on `ring`, tested against RFC vectors.

**Tech Stack:** Rust, tokio, hyper 1 (`server`, `http1`, upgrades), hyper-util, `tokio-tungstenite =0.21.0` (new direct dependency, already in Cargo.lock), ring 0.17 (ECDSA P-256, ECDH, HKDF, AES-128-GCM, SHA-256), reqwest (push POSTs), data-encoding (base64url, base32), subtle, rusqlite. Plain ES2020 JS, no build step.

**Spec:** `docs/superpowers/specs/2026-11-09-web-app-design.md`. Its closing "Decided with the owner (2026-11-10)" section overrides the body. Wave index: `docs/superpowers/plans/2026-11-10-wave1.md`.

## Global Constraints

Every constraint in the wave index applies (commits, the lane worktree, TDD and lint order, event-waiting tests, scrubbing, root's rules, DESIGN/README in the same task, no live container). Specific to this lane:

- Branch `claude/web-app`. W merges last: rebase onto A4, A2 and B before the PR, then run the sibling tasks (W-11 to W-13) against their real code.
- Schema **v18**. `SCHEMA_VERSION` becomes 18. B owns v17. On this branch before the rebase, the v18 block runs straight after v16. On rebase, put B's block first and add `(17, "")` to the migration test.
- Port **7793** (`[web] port` default).
- The one new dependency: `tokio-tungstenite = { version = "=0.21.0", default-features = false, features = ["handshake"] }`. `cargo tree -d` shows no new duplicate. `cargo deny check advisories bans` passes.
- Values copied from the spec:
  - cookie `__Host-zephyr=<b64url of 32 random bytes>; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=2592000`;
  - sessions last 30 days from sign-in and end after 7 days unused; `seen_at` is written at most once a minute; at most 20 sessions, at most 10 passkeys, at most 10 push subscriptions;
  - enrolment code: 10 min, one use; ceremony and signing challenges: 2 min, one use, at most 16 pending;
  - 20 unauthenticated requests a minute (then 429); 5 failed finishes in 10 min close enrolment and sign-in for 15 min; per session: 60 socket requests, 10 `web_challenge` and 10 `/api/push/decide` a minute;
  - at most 4 web sockets (the 5th gets 503); ping every 30 s; session re-check every 60 s; `max_message_size` and `max_frame_size` = `MAX_LINE` (1 MiB);
  - 32 connections, a 10 s head timeout, `keep_alive(false)` and 64 KB bodies, as in `imessage.rs:278-320`;
  - push: card `Urgency: high`, `TTL: 86400`; reply `TTL: 3600`, body cut to 120 chars; VAPID `exp` = now + 12 h, `sub` = `https://HOST`; each push has a 10 s timeout.
- The CSP and frame headers are exactly the spec's §4.1 list (W-4 pins the strings).
- `zephyr-policy` and `fake-rpi.py` are untouched. `gateway_kind` already refuses `/web`.
- The Tailscale headers decide nothing beyond "refuse". They never appear in a log, an audit row or `settings_log`.

**Decisions this plan adds** (the spec is silent, or wrong against the code):

1. `proposal_digest` also covers `targets`, because `/ok` reads them (`actions::remember`, actions.rs:168).
2. `store.rs` never turns on `PRAGMA foreign_keys`, so `ON DELETE CASCADE` would do nothing. The `REFERENCES` clauses stay as documentation, and `remove_passkey` deletes the passkey's sessions and pushes itself, in one transaction.
3. The web state lives in an `Arc<web::Hub>` passed to `web::serve` and the router. It is not a set of statics, because the integration tests run several gateways in one process.
4. The final check of a signed request (digest, card hash, current value) runs in the router, on its loop, just before `/ok` or `set_setting`. So nothing can change between the check and the effect.
5. The `audit` table has no channel column. A signed decision therefore adds its own audit row (`session "web"`, `tool "passkey"`, `args "#7 ok"`, `outcome "signed"`), and the existing outcome strings stay as they are.
6. The 20-a-minute limit covers only `/api/enrol/*` and `/api/login/*`. Loading the app is 7 GETs.
7. Two challenge queues of 16 each: sign-in ceremonies, and signed decisions per session. Spam on `login/start` then cannot evict the owner's pending signature.
8. A wrong code at `enrol/start` counts as a failure, since the code is checked there.
9. The enrolment code is 6 random bytes in RFC 4648 base32 without padding: 10 characters, 48 bits, shown as `XXXXX-XXXXX`. Input is uppercased, and `-` and whitespace are dropped.
10. A signed decision must come from the session's own passkey: the assertion's credential id equals that passkey's `cred_id`.
11. Hidden `Config` fields, `#[serde(skip)]` like `tick`:
    - `push_hosts: Vec<String>`: exact origins also accepted, over `http` too; empty by default;
    - `web_lockout: Duration`: default 15 min;
    - `web_ping: Duration`: default 30 s; the dead-socket timeout is twice this.
12. `Snapshot.channels` gains `web`: `connected` while a web socket is open, else `off`.
13. The listen address is not configurable, so the spec's "any other address is refused at start" has nothing to check. It is dropped.
14. `zephyr web setup --yes` answers yes to every question (for tests and scripts).

## Review Focus

1. **A passkey assertion replayed for a different proposal, or for the same proposal after its payload changed** (wave index item 4): refused, and the action does not run. Test in W-7.
2. **Chrome's clientDataJSON with its extra `other_keys_can_be_added_here` key and `"crossOrigin":false`**: accepted, since unknown keys are ignored; `crossOrigin: true` is refused. A hand-rolled parser that compares against a template breaks every Android sign-in. Test in W-2.
3. **A held card whose summary is 1500 Korean characters**: the push still goes out, in one record of at most 4096 bytes, and decrypts. Push services refuse larger bodies. Test in W-3.
4. **A phone that backgrounds the app without closing the socket** (iOS freezes it): the dead socket is closed within two ping intervals, `connected()` drops to 0, and the reply is pushed. Without this, a reply in that window is marked sent to a socket nobody reads. Test in W-8.
5. **An enrolment code typed on a phone as `abcde fghjk` or with its hyphen**: accepted. A wrong code counts toward the lockout. Test in W-6.

---

## Core

### Task W-1: Store v18: passkeys, sessions, pushes, `meta`, `proposal_digest`

**Files:**
- Modify: `crates/zephyr/src/store.rs` (`SCHEMA_VERSION` `:7`, `open` `:536-797`, migration test `:3108`)

**Interfaces:**
- Produces:
  ```rust
  pub struct Passkey { pub id: i64, pub name: String, pub cred_id: Vec<u8>, pub public_key: Vec<u8>,
      pub sign_count: u32, pub created_at: i64, pub last_used_at: Option<i64> }
  pub struct WebSession { pub token_hash: [u8; 32], pub passkey: i64, pub created_at: i64,
      pub seen_at: i64, pub expires_at: i64 }
  pub struct WebPush { pub id: i64, pub endpoint: String, pub p256dh: Vec<u8>, pub auth: Vec<u8>,
      pub passkey: i64, pub created_at: i64 }
  // hand-written Debug on all three: lengths only for cred_id, public_key, token_hash, endpoint, p256dh, auth
  pub const SESSION_LIFE: i64 = 30 * 86_400; pub const SESSION_IDLE: i64 = 7 * 86_400;
  pub const MAX_SESSIONS: i64 = 20; pub const MAX_PASSKEYS: usize = 10; pub const MAX_PUSH: i64 = 10;
  impl Store {
    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>>;
    pub fn set_meta(&self, key: &str, value: Option<&str>) -> rusqlite::Result<()>; // None deletes
    pub fn add_passkey(&self, name: &str, cred_id: &[u8], point: &[u8]) -> rusqlite::Result<i64>;
    pub fn passkeys(&self) -> rusqlite::Result<Vec<Passkey>>;               // oldest first
    pub fn passkey(&self, id: i64) -> rusqlite::Result<Option<Passkey>>;
    pub fn passkey_by_cred(&self, cred_id: &[u8]) -> rusqlite::Result<Option<Passkey>>;
    pub fn passkey_used(&self, id: i64, sign_count: u32, now: i64) -> rusqlite::Result<()>;
    pub fn remove_passkey(&self, name: &str) -> rusqlite::Result<Option<usize>>; // sessions ended; None = no such name
    pub fn add_web_session(&self, token_hash: &[u8; 32], passkey: i64, now: i64) -> rusqlite::Result<()>;
    pub fn web_session(&self, token_hash: &[u8; 32], now: i64) -> rusqlite::Result<Option<WebSession>>;
    pub fn web_sessions(&self) -> rusqlite::Result<Vec<WebSession>>;
    pub fn end_web_session(&self, token_hash: &[u8; 32]) -> rusqlite::Result<()>;
    pub fn end_web_sessions(&self) -> rusqlite::Result<usize>;
    pub fn expire_web_sessions(&self, now: i64) -> rusqlite::Result<usize>;
    pub fn add_push(&self, endpoint: &str, p256dh: &[u8], auth: &[u8], passkey: i64) -> rusqlite::Result<bool>; // upsert by endpoint; false when 10 others exist
    pub fn pushes(&self) -> rusqlite::Result<Vec<WebPush>>;
    pub fn drop_push(&self, endpoint: &str) -> rusqlite::Result<()>;
    pub fn proposal_digest(&self, id: i64) -> rusqlite::Result<Option<[u8; 32]>>;
  }
  ```
- The v18 SQL is the spec's §4.9 block, verbatim.
- `web_session` returns `None` past `expires_at`, or when `now - seen_at > SESSION_IDLE`. It writes `seen_at = now` when the stored one is 60 s or more old.
- `add_web_session` sets `expires_at = now + SESSION_LIFE`, then deletes all but the newest 20.
- `expire_web_sessions` deletes rows `web_session` would refuse.
- `proposal_digest` is SHA-256 of `serde_json::to_vec(&json!([kind, class, command, targets, payload, secrets, key, seen]))` over the row's columns as stored (NULL → `null`).

- [x] **Step 1: Write the failing tests** in `store.rs` `mod tests`:
  - Extend `older_databases_migrate_in_order_and_keep_their_rows` with `const V18: &str = "DROP TABLE web_push; DROP TABLE web_sessions; DROP TABLE web_passkeys;"`, run first in the undo chain, and add the `(16, "")` case.
  - `a_session_ends_30_days_after_sign_in_or_7_days_unused`:
    - `add_web_session(h, 1, 0)`;
    - `web_session(h, 6*86_400)` is `Some`;
    - after `web_session(h, 100)`, a read at `100 + 7*86_400 + 1` is `None`;
    - a session touched every 6 days is `None` at `30*86_400 + 1`.
  - `seen_at_is_written_at_most_once_a_minute`: reads at 10 and 59 leave `seen_at == 0`; a read at 60 makes it 60.
  - `the_21st_session_drops_the_oldest`.
  - `removing_a_passkey_removes_its_sessions_and_pushes`:
    - two passkeys, each with a session and a push;
    - `remove_passkey("phone") == Some(1)`;
    - the other passkey's rows remain;
    - `remove_passkey("nope") == None`.
  - `a_push_endpoint_is_saved_once_and_the_eleventh_is_refused`.
  - `proposal_digest_changes_with_every_column_ok_reads`: for each of the 8 columns, `UPDATE` it on a held action and assert that the digest differs from the one before. `summary` and `reason` do not change it.
  - `debug_of_web_rows_prints_lengths_only`: `format!("{p:?}")` holds `cred_id: 3 bytes` and no byte value.
- [x] **Step 2: Run** `cargo test -p zephyr --lib store::` → FAIL (no such functions).
- [x] **Step 3: Implement** in `store.rs`. Add an `if version < 18` block after v16's, ending `PRAGMA user_version = 18;`, and bump `SCHEMA_VERSION`. Write `remove_passkey` as one transaction: delete pushes, delete sessions (count them), delete the passkey.
- [x] **Step 4: Run** `cargo test -p zephyr --lib store::` → PASS; then `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(store): keep the web app's passkeys, sessions and push subscriptions in schema v18`.
- Deviation: decision 2 is wrong about the code: the bundled SQLite of rusqlite 0.41 enforces foreign keys, so the spec's `REFERENCES` clauses are live. The schema stays verbatim; `remove_passkey` still deletes the rows itself (to count the sessions); the tests give a session or a push a real passkey first instead of `passkey = 1` on an empty table.
- Deviation: `upgraded_from` is the approvals gate's v16 mark (the router writes `preset = "full"` on it), so a v16 database migrating to v18 does not set it; the migration test's `(16, "")` case expects `None`. `tests/router.rs` `old_install` also drops the v18 tables before stamping v15.
- Deviation: added the test `meta_values_are_set_read_and_deleted` for `meta`/`set_meta`.

### Task W-2: `web/passkey.rs`: WebAuthn ES256 verification, preimages, challenge queues

**Files:**
- Create: `crates/zephyr/src/web/passkey.rs`, `crates/zephyr/src/web/mod.rs` (only `pub mod passkey; pub mod push;` for now)
- Modify: `crates/zephyr/src/lib.rs` (`pub mod web;`)

**Interfaces:**
- Produces:
  ```rust
  pub struct Assertion { pub id: Vec<u8>, pub client_data: Vec<u8>, pub auth_data: Vec<u8>, pub signature: Vec<u8> }
  pub struct Created { pub id: Vec<u8>, pub client_data: Vec<u8>, pub auth_data: Vec<u8>, pub public_key: Vec<u8>, pub alg: i64 }
  pub fn assertion(v: &serde_json::Value) -> Result<Assertion, &'static str>; // fields id, clientDataJSON, authenticatorData, signature: base64url, no padding
  pub fn created(v: &serde_json::Value) -> Result<Created, &'static str>;     // id, clientDataJSON, authenticatorData, publicKey, alg
  pub fn verify(host: &str, challenge: &[u8; 32], point: &[u8], stored: u32, a: &Assertion) -> Result<u32, &'static str>; // the new sign count
  pub fn check_created(host: &str, challenge: &[u8; 32], c: &Created) -> Result<[u8; 65], &'static str>; // the point to store
  pub enum Signed { Decide { id: i64, verb: String, digest: [u8; 32], card_sha256: String },
                    Set { key: String, value: String, until: Option<String>, current: Option<String> } }
  pub fn preimage(s: &Signed, nonce: &[u8; 16]) -> Vec<u8>;
  // ["zephyr-web/1","decide",id,verb,hex(digest),card_sha256,hex(nonce)] | ["zephyr-web/1","set",key,value,until,current,hex(nonce)]
  pub struct Challenges<T> { /* Mutex<VecDeque<(String, [u8;32] owner, T, [u8;32] challenge, Instant)>> */ }
  impl<T> Challenges<T> {
      pub fn new(ttl: Duration, cap: usize) -> Self;
      pub fn issue(&self, owner: [u8; 32], what: T, challenge: [u8; 32]) -> String;         // ids "c1", "c2", …; the oldest goes past cap
      pub fn take(&self, id: &str, owner: [u8; 32]) -> Option<(T, [u8; 32])>;            // removed whether or not it then verifies; None past ttl
  }
  ```
- Refusal reasons, used as-is in tests and logs: `"wrong type"`, `"wrong challenge"`, `"wrong origin"`, `"cross-origin"`, `"short authenticator data"`, `"wrong rp"`, `"no user presence"`, `"no user verification"`, `"no attested credential"`, `"credential id differs"`, `"not ES256"`, `"not a P-256 key"`, `"bad signature"`, `"sign count went back"`.
- `clientDataJSON` is parsed with `serde_json::Value`, and only these keys are read: `type`, `challenge`, `origin`, `crossOrigin`.
  - `challenge` is base64url-decoded and compared as bytes.
  - `origin` must be `"https://" + host`.
  - `crossOrigin` must be absent or `false`.
- Authenticator data:
  - at least 37 bytes;
  - bytes 0..32 are `SHA-256(host)`;
  - flags: UP `0x01` and UV `0x04` always, and AT `0x40` for `check_created`;
  - the count is the big-endian u32 at 33..37.
- `check_created`:
  - the credential id is read at 55.., its length the u16 at 53..55;
  - `public_key.len() == 91`, and it starts with the 26-byte SPKI prefix `3059301306072a8648ce3d020106082a8648ce3d030107034200` (hex);
  - returns `public_key[26..]`.
- The signature is checked with `ring::signature::ECDSA_P256_SHA256_ASN1` over `auth_data ‖ SHA-256(client_data)`.
- Sign count: a new count that is non-zero and at or below a non-zero `stored` is refused. Synced passkeys send 0.

- [x] **Step 1: Write the failing tests** in `passkey.rs` `mod tests`. The software authenticator is `EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING)`; helpers `client_data(type, challenge, origin)` and `auth_data(host, flags, count)`.
  - `the_rfc_6979_p256_sha256_signature_verifies`: RFC 6979 A.2.5 through the same path `verify` uses (factor it as `fn es256(point, msg, der) -> bool`):
    - point `04 ‖ 60FED4BA255A9D31C961EB74C6356D68C049B8923B61FA6CE669622E60F29FB6 ‖ 7903FE1008B8BC99A41AE9E95628BC64F2F1B20C2D7E9F5177A3C294D4462299`;
    - message `b"sample"`;
    - DER `3046022100EFD48B2AACB6A8FD1140DD9CD45E81D69D2C877B56AAF991C34D0EA84EAF3716022100F7CB1C942D657C41D436C7A1B6E29F65F3E900DBB9AFF4064DC4AB2F843ACDA8` → true;
    - the same signature over `b"test"` → false.
  - `verify_accepts_a_good_assertion_and_returns_its_count` (count 5 over stored 4 → `Ok(5)`; count 0 over stored 9 → `Ok(0)`).
  - `verify_refuses_each_broken_field`, table-driven, one case per reason above: type `webauthn.create`, another challenge, origin `https://evil.test`, `crossOrigin: true`, 36 bytes, the rpIdHash of `other.test`, UP clear, UV clear, a flipped signature byte, count 4 over stored 4.
  - `chromes_extra_client_data_key_and_cross_origin_false_are_accepted` (Review Focus 2): clientData holds `"crossOrigin":false,"other_keys_can_be_added_here":"do not compare clientDataJSON against a template. See https://goo.gl/yabPex"` → `Ok`.
  - `check_created_reads_the_point_and_refuses_bad_registrations`: good → the 65-byte point. Refused:
    - AT clear;
    - attested id ≠ `id`;
    - `alg: -257`;
    - an SPKI with one prefix byte changed;
    - a 90-byte key;
    - type `webauthn.get`.
  - `preimages_that_differ_in_any_field_give_different_challenges`: vary each field of both shapes; all SHA-256 values distinct.
  - `a_challenge_is_taken_once_by_its_owner_within_its_ttl`:
    - `take` by another owner → `None`, and the challenge is then gone;
    - a second `take` → `None`;
    - after `ttl` → `None`;
    - the 17th `issue` evicts the first.
- [x] **Step 2: Run** `cargo test -p zephyr --lib web::passkey` → FAIL.
- [x] **Step 3: Implement** the signatures above. Use `data_encoding::BASE64URL_NOPAD`, `ring::digest`, `ring::signature`; no other crate.
- [x] **Step 4: Run** the same → PASS.
- [x] **Step 5: Commit** `feat(web): verify passkey sign-ins and signatures with ES256 on ring`.
- Deviation: `web/mod.rs` declares only `passkey` here; `push` joins it in W-3, when the file exists.
- Deviation: `assertion` and `created` answer `"malformed"` for a missing or non-base64url field (the plan lists no reason for it), covered by the added test `browser_results_are_read_from_base64url_and_malformed_ones_refused`; `sha256` is public for W-6 and W-7.

### Task W-3: `web/push.rs`: aes128gcm, VAPID, the endpoint allowlist, payloads

**Files:**
- Create: `crates/zephyr/src/web/push.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn encrypt(ecdh_secret: &[u8], as_public: &[u8; 65], ua_public: &[u8], auth: &[u8], salt: &[u8; 16], plaintext: &[u8]) -> Vec<u8>; // header ‖ ciphertext
  pub fn decrypt(ecdh_secret: &[u8], ua_public: &[u8], auth: &[u8], body: &[u8]) -> Option<Vec<u8>>;  // the integration test's receiver
  pub fn vapid_jwt(key: &ring::signature::EcdsaKeyPair, endpoint: &url::Url, host: &str, now: i64) -> String;
  pub fn allowed_endpoint(endpoint: &str, extra: &[String]) -> Option<url::Url>;
  pub fn card(id: i64, body: &str, quick: bool) -> serde_json::Value;  // {"kind":"card","id","title":"zephyr: approve #ID","body","quick"}
  pub fn reply(body: &str) -> serde_json::Value;                         // {"kind":"reply","body"}
  pub const MAX_PLAINTEXT: usize = 4096 - 86 - 17;                        // 3993: header 86, delimiter 1, tag 16
  ```
- `encrypt` follows RFC 8291 §3.4 and RFC 8188:
  1. `PRK_key = HMAC(auth, ecdh_secret)`;
  2. `IKM = HMAC(PRK_key, "WebPush: info\0" ‖ ua_public ‖ as_public ‖ 0x01)`;
  3. `PRK = HMAC(salt, IKM)`;
  4. `CEK = HMAC(PRK, "Content-Encoding: aes128gcm\0" ‖ 0x01)[..16]`;
  5. `NONCE = HMAC(PRK, "Content-Encoding: nonce\0" ‖ 0x01)[..12]`;
  6. one record, `plaintext ‖ 0x02`, sealed with AES-128-GCM;
  7. header `salt ‖ 00001000 ‖ 41 ‖ as_public`.

  Write it with `ring::hkdf` (a `KeyType` newtype for 32/16/12) and `ring::aead::LessSafeKey`. It panics if `plaintext.len() > MAX_PLAINTEXT`.
- The JWT header segment is exactly `eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9` (`{"typ":"JWT","alg":"ES256"}`).
  - Claims: `{"aud": <scheme://host[:port] of the endpoint>, "exp": now+43200, "sub": "https://HOST"}`.
  - Signature: `ECDSA_P256_SHA256_FIXED_SIGNING`, 64 bytes r‖s, base64url. JOSE needs the fixed form, not DER.
- `allowed_endpoint` accepts:
  - `https`, no userinfo, no port or 443, host `fcm.googleapis.com` exactly or ending in `.push.apple.com`, `.push.services.mozilla.com` or `.notify.windows.com`;
  - or an endpoint whose origin (`scheme://host:port`) equals an entry of `extra`.
- `card` and `reply` cut `body`: card to 200 chars plus `…`, reply to 120 chars. `card` passes the text through unchanged otherwise, since the caller scrubs.

- [x] **Step 1: Write the failing tests** in `push.rs` `mod tests`:
  - `encrypt_matches_rfc_8291_appendix_a` (values base64url, whitespace removed):
    - ecdh_secret `kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs`;
    - auth `BTBZMqHH6r4Tts7J_aSIgg`;
    - salt `DGv6ra1nlYgDCS1FRnbzlw`;
    - as_public `BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8`;
    - ua_public `BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4`;
    - plaintext `b"When I grow up, I want to be a watermelon"`;
    - the output, base64url-encoded, equals RFC 8291 §5's whole message `DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN`. Compare the whole string: Appendix A's header and ciphertext strings cannot be joined as text, because the 86-byte header is not a multiple of 3;
    - `decrypt` of it gives the plaintext back.
  - `the_vapid_jwt_verifies_and_carries_the_claims`:
    - our JWT's header segment is the literal above;
    - the signature verifies with `ECDSA_P256_SHA256_FIXED` under `key.public_key()`;
    - claims `aud == "https://fcm.googleapis.com"`, `exp == now + 43200`, `sub == "https://zephyr.test"`;
    - the same test helper verifies RFC 8292 §2.4's example token under its `k=BA1Hxzyi1RUM1b5wjxsn7nGxAszw2u61m164i3MrAIxHF6YK5h4SDYic-dRuU_RCPCfA5aq9ojSwk5Y2EmClBPs` (copy `t=` from the RFC text), which proves the helper reads the fixed form.
  - `the_allowlist_takes_push_services_only`:
    - accepted: `https://fcm.googleapis.com/fcm/send/x` and `https://web.push.apple.com/Q`;
    - refused: `https://fcm.googleapis.com.evil.test/x`, `http://fcm.googleapis.com/x`, `https://u@fcm.googleapis.com/x`, `https://fcm.googleapis.com:8443/x`, `https://push.apple.com.evil.test/x`;
    - `http://127.0.0.1:9/p` passes only with `extra = ["http://127.0.0.1:9"]`.
  - `a_long_korean_card_still_fits_one_push` (Review Focus 3):
    - `card(7, &"가".repeat(1500), false)` serialised is ≤ `MAX_PLAINTEXT`;
    - `encrypt(...)` of it is ≤ 4096 bytes;
    - the body ends with `…`;
    - `reply(&"x".repeat(500))["body"]` has 120 chars.
- [x] **Step 2: Run** `cargo test -p zephyr --lib web::push` → FAIL.
- [x] **Step 3: Implement.**
- [x] **Step 4: Run** → PASS.
- [x] **Step 5: Commit** `feat(web): encrypt Web Push messages (RFC 8291) and sign VAPID tokens on ring`.
- Deviation: `allowed_endpoint` refuses userinfo for an `extra` origin too, not only for the push services.

### Task W-4: The listener: `[web]`, channels `web` and `passkey`, checks, headers, files

**Files:**
- Modify:
  - `crates/zephyr/src/web/mod.rs`;
  - `crates/zephyr/src/config.rs`: `Config` `:41`, the hidden fields `:51-70` plus `push_hosts`, `web_lockout`, `web_ping`; `strong_default` `:211`; the test at `:618`;
  - `crates/zephyr/src/settings.rs`: `CHANNELS` `:184` gains `"web"` and `"passkey"`;
  - `crates/zephyr/src/reload.rs`: `diff` `:27`, `old.web != new.web` → `Reexec`;
  - `crates/zephyr/src/router.rs`: `run` `:297` (a `select!` arm), `publish` `:1189-1198` (`web` in channels);
  - `crates/zephyr/src/main.rs`: `doctor` `:391` and `status` `:329`;
  - `crates/zephyr/tests/attach.rs`: the exact hello JSON `:476` gains `{"name":"web","state":"off"}`.
- Create: `crates/zephyr/assets/web/{index.html,app.js,app.css,sw.js,manifest.webmanifest,icon-192.png,icon-512.png}` as placeholders (W-9 writes them). Create `crates/zephyr/tests/web.rs`.
- Docs:
  - DESIGN §1 line 14: drop "a web UI";
  - DESIGN §4.3: a Web bullet;
  - new §4.3.3 "The web app": "One listener" and "Reaching it";
  - §4.17: `strong_channels` may name `web` and `passkey`; default `["tui", "chat", "passkey"]`;
  - §5: `src/web/`, `assets/web/`;
  - README: `[web]` in the config reference;
  - CLAUDE.md: one architecture bullet for `web/` (mod, passkey, push, and the `web`/`passkey` channels).

**Interfaces:**
- Produces:
  ```rust
  // config.rs
  #[derive(Debug, Clone, PartialEq, Deserialize)] #[serde(deny_unknown_fields)]
  pub struct Web { pub host: String, #[serde(default = "web_port")] pub port: u16, #[serde(default)] pub tailnet_users: Vec<String> }
  pub web: Option<Web>,   // on Config
  // validate: host non-empty, lowercase letters, digits, '-' and '.', no scheme, port, path or trailing dot
  // web/mod.rs
  pub struct Hub { /* sockets: AtomicUsize, revoked: watch::Sender<u64>; W-6 adds ceremonies: passkey::Challenges<Ceremony>
                     and limits: Mutex<Limits>, W-7 adds signing: passkey::Challenges<passkey::Signed> */ }
  impl Hub { pub fn new() -> Arc<Hub>; pub fn connected(&self) -> usize; pub fn revoke(&self);
             pub fn revocations(&self) -> watch::Receiver<u64>; }
  pub async fn serve(cfg: &Config, handles: crate::attach::Handles, hub: Arc<Hub>) -> anyhow::Result<()>; // pending forever without [web]
  pub fn admit(method: &Method, path: &str, h: &HeaderMap, web: &crate::config::Web) -> Result<(), StatusCode>;
  pub struct Window { /* start: Instant, n: u32, limit: u32, span: Duration */ }
  impl Window { pub fn new(limit: u32, span: Duration) -> Self; pub fn hit(&mut self, now: Instant) -> bool; } // false past the limit until the span rolls
  ```
- `attach::Handles` is introduced here and used by W-5: `#[derive(Clone)] pub struct Handles { pub inbound: mpsc::Sender<Inbound>, pub router: mpsc::Sender<AttachRequest>, pub snap: watch::Receiver<Snapshot>, pub cfgs: watch::Receiver<Arc<Config>> }`. `attach::serve` becomes `serve(cfg: &Config, handles: Handles)`, and `router::run` builds one `Handles` and clones it into both listeners.
- `Router` gains `pub(crate) web: Option<Arc<web::Hub>>`, set in `run` when `cfg.web` is set, as `gate_notes` is.
- `admit`, in order:
  1. Host ≠ `[web] host`, ignoring case → 421.
  2. No `Tailscale-User-Login`, or one not in a non-empty `tailnet_users` (ignoring case) → 403 `open the app through tailscale serve`.
  3. POST, DELETE or a WebSocket upgrade:
     - `Origin` ≠ `https://HOST` → 403;
     - `Sec-Fetch-Site` present and ≠ `same-origin` → 403;
     - a POST body without `Content-Type: application/json` (parameters allowed) → 415.
- The cookie check is the handler's (401), in W-5 and W-6.
- Response headers, on every response:
  - `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self' wss://HOST; manifest-src 'self'; worker-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'`;
  - `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin`;
  - `Cache-Control: no-store` on `/api/*`, and `no-cache` plus `ETag: "<CARGO_PKG_VERSION>"` on files (304 on a matching `If-None-Match`).
- Files are `include_bytes!` with their content types. Any other path → 404.
- The bind is `("127.0.0.1", port)` (W-13 adds bridge mode). A bind error is logged as `zephyr: web app not listening on port P: …`, and `serve` then pends: the daemon runs on without the app.
- At start `serve` makes `<state>/web/` (0700) and `vapid.pk8` (0600, `EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING)`) if missing. A key that does not load is logged, and push stays off.
- `doctor` lines: `web app listening on 127.0.0.1:P` (a TCP connect) and `the web push key loads`, only when `[web]` is set. `status`: `web: listening` or `web: not listening`.

- [x] **Step 1: Write the failing tests:**
  - `web/mod.rs` unit:
    - `admit_checks_host_then_tailnet_then_origin_fetch_site_and_type`: table over (method, headers) → 421/403/403/403/415/Ok, including `tailnet_users = ["owner@github"]` with `Owner@GitHub` → Ok and `x@y` → 403;
    - `a_window_allows_its_limit_then_refuses_until_it_rolls`;
    - `the_app_holds_no_inline_code` (reads `assets/web` at test time): `app.js` and `sw.js` contain none of `innerHTML`, `insertAdjacentHTML`, `outerHTML`, `eval(`, `new Function`; `index.html` has no `<script>` without `src=` and no `style=`.
  - `config.rs`:
    - `a_web_section_needs_a_bare_host`: `https://x`, `x:443`, `x/`, `x.` and an empty host are refused; `box.tail1234.ts.net` is accepted, port 7793;
    - update the default test to `["tui", "chat", "passkey"]`;
    - `strong_channels_may_name_web_and_passkey`.
  - `reload.rs`: `a_web_change_reexecs`.
  - `tests/web.rs`: a harness copied from `tests/attach.rs:59-100`, with `[web]\nhost = "zephyr.test"\nport = P` and `cfg.tick = 200ms`, plus helper `req(gw, method, path)` (reqwest; sets `Host: zephyr.test`, `Origin: https://zephyr.test`, `Tailscale-User-Login: owner@github`).
    - `the_files_carry_the_security_headers`: `GET /` → 200 with the exact CSP string above, `X-Frame-Options: DENY`, `Cache-Control: no-cache`, `ETag`.
    - `GET /nope` → 404; `GET /` with `Host: evil.test` → 421; without the Tailscale header → 403.
- [x] **Step 2: Run** `cargo test -p zephyr --lib web:: config:: reload::` and `cargo test -p zephyr --test web` → FAIL.
- [x] **Step 3: Implement.** The accept loop is a copy of `imessage.rs:278-320` with `.with_upgrades()`. The placeholder `index.html` is `<!doctype html><title>zephyr</title><link rel=stylesheet href=/app.css><script src=/app.js defer></script>`. Make the PNGs once with a throwaway `python3 -I` script: a solid square, zlib + struct, not committed. Write the docs.
- [x] **Step 4: Run** the new tests, then `cargo test -p zephyr` (the attach exact-JSON test included) → PASS.
- [x] **Step 5: Commit** `feat(web): serve the web app's files behind Host, Origin and tailscale serve checks`.
- Deviation: with `passkey` in the default `strong_channels`, every refusal and card that names the default list now reads `tui or chat or passkey` (the code joins with ` or `; the spec's example wording `tui, chat or passkey` would mean changing seven call sites). The tests and DESIGN examples that pin the default wording follow (gate.rs, settings.rs, tests/router.rs).
- Deviation: `admit` wants `Content-Type: application/json` on a DELETE as well as a POST (`DELETE /api/push` has a JSON body), and every 403 carries the one body `open the app through tailscale serve`.
- Deviation: `push::vapid(cfg, make)` loads (and at start makes) the key; `doctor` calls it with `make = false`. `Window` counts its span from the first hit, not from `new`. README gets a `Web app` section here (there is no single config reference), which later tasks extend.
- Deviation: the README's and DESIGN's `[web]` text is written here; a second integration test `unknown_paths_hosts_and_callers_are_refused` holds the 404/421/403 cases.

### Task W-5: The WebSocket bridge into `attach::Conn` on channel `web`

**Files:**
- Modify:
  - `crates/zephyr/Cargo.toml`: `tokio-tungstenite`;
  - `crates/zephyr/src/attach.rs`:
    - `AttachRequest` `:48-74`;
    - `serve` `:99`, building `Handles`;
    - `Conn` `:145`: `channel`, `web`;
    - `run` and `talk` `:169-224`, generic;
    - `request` `:226`: the method gate;
    - `dequeue`, `settings_set`, `decide` `:341-411`, which pass the channel;
    - `send` `:618`: channel and sender `web`;
    - `live` `:685`: own-channel check;
    - `read_lines` `:747`, generic;
  - `crates/zephyr/src/router.rs`:
    - `attach` `:1274-1311`;
    - `settings_set` `:1354` and its `Actor` channel `:1371`;
    - `decide` `:1446` and `:1462`;
    - `dequeue` `:1522`;
    - the reply `show` `:1712`;
    - `owner_on` `:2766`;
    - `login_notice` `:3102`;
    - `flush` `:3629`, `:3645`;
    - `show` `:3703`;
  - `crates/zephyr/src/config.rs`: `is_owner` `:462`;
  - `crates/zephyr/src/web/mod.rs`: the `/api/ws` route and `fn ws`.
- Test: `crates/zephyr/tests/web.rs`.
- Docs: DESIGN §4.3.1 (the methods closed to `web`, `channel` on `zephyr_reply`), §4.3.3 "The WebSocket bridge".

**Interfaces:**
- Produces:
  ```rust
  // attach.rs
  pub fn attached(channel: &str) -> bool;             // "tui" | "web"
  pub enum Origin { Tui { token: Arc<str> }, Web { session: [u8; 32], passkey: i64, hub: Arc<crate::web::Hub> } }
  pub async fn serve_stream<S: AsyncRead + AsyncWrite + Send + 'static>(stream: S, origin: Origin, handles: Handles);
  const WEB_METHODS: &[&str] = &["zephyr.hello", "zephyr.subscribe", "send", "zephyr.models", "zephyr.dequeue",
      "zephyr.turns", "zephyr.settings", "zephyr.settings_set", "zephyr.proposals", "zephyr.decide",
      "zephyr.facts_list", "zephyr.vault_list", "ping", "zephyr.web_challenge", "zephyr.web_devices"];
  // AttachRequest variants change to carry the channel:
  Dequeue { channel: &'static str, reply: oneshot::Sender<Vec<String>> },
  SettingsSet { key, value, until, channel: &'static str, expect: Option<Option<String>>, reply },
  Decide { id, verb, channel: &'static str, expect: Option<Expect>, reply },
  pub struct Expect { pub digest: [u8; 32], pub card_sha256: String }   // read by W-7; None here
  // router.rs
  fn show(&self, text: &str, command: Option<&str>, channel: Option<&str>); // zephyr_reply gains "channel"
  ```
- `Conn` gets `channel: &'static str` (`"tui"` or `"web"`) and `web: Option<(…session, passkey, hub)>`. A web `Conn` starts `authed`.
  - The gate: on `web`, a method not in `WEB_METHODS` → `-32601 "not available in the web app"`. `authenticate` is not in the list.
  - The TUI's `Conn` is unchanged: no gate, an `assertion` param ignored.
  - `zephyr.web_challenge` and `zephyr.web_devices` answer `-32601` until W-7 and W-6.
- In `talk`, a `zephyr_reply` whose `channel` is non-null and ≠ `self.channel` is skipped, for both raw and stock clients.
- Router:
  - `attached()` replaces `== attach::CHANNEL` at each line listed in Files;
  - the reply `show` passes `Some(row.channel)`; notices pass `None`;
  - in `flush`, rows on `web` are handled like `tui` rows when `hub.connected() > 0`, and are skipped (left pending) when it is 0. W-8 adds the push there.
  - `tui` rows keep `tap.receiver_count()`: a notice shown to a web client counts as delivered.
- `web::ws` (route `GET /api/ws`):
  - `admit`; the cookie (`__Host-zephyr`, SHA-256 → `store.web_session`) else 401;
  - `hub.connected() >= 4` → 503;
  - the `Sec-WebSocket-Key` → `tungstenite::handshake::derive_accept_key`, and a 101 with `Upgrade: websocket`, `Connection: Upgrade`, `Sec-WebSocket-Accept`;
  - after the upgrade: `WebSocketStream::from_raw_socket(TokioIo::new(upgraded), Role::Server, Some(cfg))` with both size limits at 1 MiB, joined to `serve_stream` through `tokio::io::duplex(2 * MAX_LINE)`. Each text frame goes in with `\n`; each line out becomes one text frame.
- The socket closes on:
  - a binary frame, or a text frame holding `\n`;
  - more than 60 frames in a minute (a `Window`);
  - no frame or pong within `2 * cfg.web_ping`;
  - a session that `store.web_session` no longer returns, re-checked every 60 s and at once when `hub.revocations()` changes.
- It pings every `cfg.web_ping`. The socket count goes up on upgrade and down on drop (a guard).

- [x] **Step 1: Write the failing tests** in `tests/web.rs`. Helper `cookie(gw) -> String`: `Store::add_passkey` plus `add_web_session` with a known token. Helper `ws(gw, cookie) -> WebSocketStream` (`tokio_tungstenite::client_async` with Host, Origin and the Tailscale header). Helper `rpc(ws, id, method, params) -> Value`, which reads frames until the reply with that id.
  - `the_socket_needs_a_cookie_origin_host_and_tailnet_header`: upgrade with no cookie → 401; `Origin: https://evil.test` → 403; `Host: evil.test` → 421; no Tailscale header → 403 (the spec's header test).
  - `a_web_send_is_a_web_row_and_its_reply_streams_back`:
    - `zephyr.hello` answers with `version`;
    - after `zephyr.subscribe`, `send {"type":"prompt","content":"hi"}` → `status: sent`;
    - the inbox row has channel `web` and sender `web`;
    - a `message_end` event with fake-rpi's reply arrives.
  - `slash_status_answers_on_web_only`:
    - with `[attach]` also set and a TUI connection authenticated and subscribed;
    - the web `send` of `/status` → a `zephyr_reply` with `"channel":"web"` on the web socket;
    - the TUI connection gets no `zephyr_reply` for it before its own `ping` answer.
  - `methods_closed_to_the_web_app_answer_32601`: `authenticate`, `start_session`, `zephyr.vault_put`, `zephyr.facts_put`, `zephyr.sessions`, `zephyr.login_start` → code `-32601`, message `not available in the web app`.
  - `a_fifth_socket_gets_503`.
  - `a_frame_over_1_mib_or_with_a_newline_closes_the_socket`: 2 MiB of text → closed; `"{}\n{}"` → closed.
- [x] **Step 2: Run** `cargo test -p zephyr --test web` → FAIL.
- [x] **Step 3: Implement.** Thread the channel through the three `AttachRequest`s; the TUI passes `"tui"`. Then run `cargo tree -d` and check that its output is unchanged from `main`.
- [x] **Step 4: Run** `cargo test -p zephyr --test web`, then `--test attach` and `--test router` (the TUI behaviour is unchanged), then `cargo test -p zephyr` and `cargo deny check advisories bans` → PASS.
- [x] **Step 5: Commit** `feat(web): bridge a WebSocket into the attach protocol as the owner on channel web`.
- Deviation: the listener leaves HTTP keep-alive on. With `keep_alive(false)` hyper 1.12 appends `close` to the 101 (`Connection: Upgrade, close`), which tungstenite's client refuses and a browser or `tailscale serve` need not accept. The 10 s head deadline runs again for each request on a connection, so an idle one is still bounded, and the 32-connection cap stands.
- Deviation: a signed (`passkey`) decision's approved command runs as a `web` message and `owner_on("passkey")` answers `web`, since `passkey` has no rows (spec §4.3); `attach::sender(channel)` gives `token` or `web`.
- Deviation: the upgrade also needs `Upgrade: websocket` and `Sec-WebSocket-Version: 13` (else 400), and a send a stalled client does not take within two ping intervals closes the socket, so a full TCP buffer cannot hold the bridge past the dead-socket limit. Added cases: the closed-method list also holds the stock and `zephyr.login_*` methods; a binary frame and the 61st frame in a minute close the socket.

### Task W-6: Enrolment, sign-in, sessions, limits, and `/web`

**Files:**
- Modify:
  - `crates/zephyr/src/web/mod.rs`: routes `/api/enrol/{start,finish}`, `/api/login/{start,finish}`, `/api/logout`; `pub fn command`; `Limits`;
  - `crates/zephyr/src/attach.rs`: `zephyr.web_devices`; a new variant `AttachRequest::Notice(String)`;
  - `crates/zephyr/src/router.rs`:
    - `command` `:2001`: a `("/web", arg)` arm;
    - the `Notice` arm in `attach`: `self.show(text, None, None)` and `store.notify_last_owner`;
  - `crates/zephyr/src/retention.rs`: `sweep` `:19` calls `store.expire_web_sessions(now)`;
  - `crates/zephyr/src/main.rs`: the container command `("web", Ok(cfg))` beside `set` `:86`, through `ask` `:236`.
- Test: `crates/zephyr/tests/web.rs`, `crates/zephyr/tests/router.rs`.
- Docs:
  - DESIGN §4.3.3 "Passkeys" (enrolment, sign-in, removal) and "Sessions, cookies, rate limits";
  - §4.1 item 6: the sweep ends expired web sessions;
  - §4.8: `zephyr web enrol`;
  - README: `/web`, `/web enrol`, `/web rm`, `/web signout`.

**Interfaces:**
- Consumes: W-1's store functions; W-2's `created`, `check_created`, `assertion`, `verify`, `Challenges`; W-4's `Window` and `Hub`.
- Produces:
  ```rust
  pub fn command(cfg: &Config, store: &Store, text: &str, channel: &str) -> Option<(String, bool)>; // (reply, sessions revoked)
  pub enum Ceremony { Enrol { code_sha256: [u8; 32] }, Login }
  pub fn cookie(token: &[u8; 32]) -> String;   // the Global Constraints' cookie string
  ```
- `meta` keys:
  - `web_user`: 16 random bytes, hex, made on the first `enrol/start`;
  - `web_enrol`: `{"sha256": hex, "name": Option<String>, "expires": unix}`.
- Replies (copy):
  - `/web` → `[zephyr] web app https://HOST · passkeys: phone (used 2h ago), ipad · 3 sessions · push on 2 devices`. With none: `passkeys: none`. Without `[web]`: `[zephyr] the web app is not set up; zephyr web setup on the host`.
  - `/web enrol [NAME]`, taken only from a channel in `strong_channels` other than `passkey` → `[zephyr] open https://HOST/#enrol=ABCDE-FGHJK on the phone within 10 min, or type the code there; it works once`. From elsewhere → `[zephyr] /web enrol needs ` + the strong channels without `passkey`, joined ` or `. With 10 passkeys → `[zephyr] 10 passkeys is the most; /web rm NAME first`. NAME: 1–32 of `[A-Za-z0-9_-]`; default `passkey-N`.
  - `/web rm NAME` (any owner channel) → `[zephyr] removed passkey NAME and its N sessions and pushes`, or `[zephyr] no passkey NAME; /web lists them`. Returns `true`, and the router calls `hub.revoke()`.
  - `/web signout` → `[zephyr] ended N web sessions`, then `revoke`.
  - Bad form → `usage: /web | /web enrol [NAME] | /web rm NAME | /web signout`.
  - Enrol notice → `[zephyr] passkey NAME was added to the web app; /web rm NAME removes it`.
  - Lockout notice, once per lockout → `[zephyr] 5 failed passkey attempts on the web app; sign-in is closed until HH:MM` (the configured time zone, as other notices).
- HTTP. Each POST is under `admit`. The 20-a-minute `Window` covers only these four ceremony routes; past it → 429. A route answers 429 `{"error":"sign-in is closed until HH:MM"}` while locked out.
  - `enrol/start {code}` → `{challengeId, publicKey}`. `publicKey` holds:
    - `rp {id: HOST, name: "zephyr"}`;
    - `user {id: b64url(web_user), name: "zephyr owner", displayName: "zephyr owner"}`;
    - the challenge;
    - `pubKeyCredParams [{type:"public-key", alg:-7}]`;
    - `authenticatorSelection {residentKey:"required", userVerification:"required"}`;
    - `attestation "none"`, `timeout 120000`;
    - `excludeCredentials` (the stored passkeys).

    The code is compared in constant time (`subtle`) after normalising, and must be within its expiry. A wrong code → 403 `{"error":"that code is wrong, used or expired"}`, and a failure is counted.
  - `enrol/finish {challengeId, name?, id, clientDataJSON, authenticatorData, publicKey, alg}`:
    1. the code in `meta` still has the hash its ceremony recorded;
    2. `check_created` passes;
    3. fewer than 10 passkeys;
    4. `set_meta("web_enrol", None)` and `add_passkey`;
    5. a new session: `Set-Cookie`, 200 `{"name"}`;
    6. send `Notice`.

    The name is the code's, else `name`, else `passkey-N`.
  - `login/start {}` → `{challengeId, publicKey: {challenge, rpId: HOST, timeout: 120000, userVerification: "required"}}`.
  - `login/finish {challengeId, id, clientDataJSON, authenticatorData, signature}` → `passkey_by_cred`, then `verify`, then `passkey_used`, then `Set-Cookie`.
  - Every refusal in a finish (unknown credential, bad signature, a stale or used challenge) counts. 5 failures within 10 min lock both ceremonies for `cfg.web_lockout`; open sessions keep working.
  - `/api/logout` (cookie) → `end_web_session`, then 204 with `__Host-zephyr=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0`.
- `zephyr.web_devices {}` → `{"passkeys":[{"name","created_at","last_used_at"}],"sessions":[{"passkey": name,"created_at","seen_at","this"}],"push":[{"passkey","created_at"}]}`.
- In the container, `zephyr web` sends `/web`, and `zephyr web enrol [NAME]` sends `/web enrol [NAME]`, over the chat socket; it prints the reply. It exits 1 on a reply that holds no `https://`, and 3 when no daemon answers (`NotRunning`).

- [x] **Step 1: Write the failing tests.** In `tests/web.rs`, helper `Authenticator` (ring P-256 key, cred id, `create(options) -> Value`, `get(options) -> Value`) builds clientData and authData as W-2's helpers do.
  - `enrolment_takes_a_code_once_and_says_so` (spec):
    - `/web enrol phone` on chat → a reply with a code; parse it with the regex `#enrol=([A-Z2-7]{5}-[A-Z2-7]{5})`;
    - a wrong code → 403;
    - the right code → 200, a `Set-Cookie` that equals the cookie pattern (attributes in order), and the notice `passkey phone was added …` on chat;
    - the same code again → 403.
  - `an_enrol_code_typed_in_lowercase_with_spaces_is_accepted` (Review Focus 5): `"abcde fghjk"` (the real code, lowercased and spaced) → 200 options.
  - `five_failures_lock_sign_in_and_tell_the_owner`: with `cfg.web_lockout = 1s`:
    - 5 wrong `login/finish` → the 6th `login/start` gets 429;
    - the notice `5 failed passkey attempts …` arrives once on chat;
    - an open session's socket still answers `ping`;
    - after the lockout, `login/start` → 200.
  - `sign_in_with_a_passkey_sets_a_cookie_and_logout_ends_it`.
  - `web_enrol_from_a_weak_channel_is_refused` (Discord stub, as `tests/router.rs` does) → `[zephyr] /web enrol needs tui or chat`.
  - `the_enrol_code_never_reaches_a_web_socket`: a web socket subscribed while chat sends `/web enrol` → no frame on it contains `#enrol=`.
  - `web_rm_closes_that_passkeys_socket_at_once` (spec "Limits") and `web_signout_ends_every_session`.
  - `the_unauthenticated_routes_take_20_a_minute`: 21 `login/start` → the 21st is 429.
  - `web_devices_lists_this_session`.
- In `tests/router.rs`: `the_agent_cannot_run_web_commands`: `[call gateway {"command":"/web enrol"}]` → a `tool error: … is not available to the agent` line.
- In `retention.rs`: `the_sweep_ends_expired_web_sessions`.
- [x] **Step 2: Run** `cargo test -p zephyr --test web`, plus the router test and `--lib retention` → FAIL.
- [x] **Step 3: Implement.** Write the docs.
- [x] **Step 4: Run** the same, then `cargo test -p zephyr` → PASS.
- [x] **Step 5: Commit** `feat(web): enrol passkeys with a one-time code from a strong channel and sign in with them`.
- Deviation: the ceremonies, the cookie, the limits, `command` and `devices` live in a new file `src/web/signin.rs` (re-exported from `web`), so `mod.rs` stays the listener and the bridge.
- Deviation: `Store::take_meta(key, value)` deletes the enrolment code in one statement at `enrol/finish`: without it two ceremonies started with one code could both enrol. `/web enrol NAME` also refuses a NAME that is taken.
- Deviation: the `Notice` arm does not write a second copy for an attached owner: when the owner last answered is on `tui` or `web` the outbox row alone is written and the flush shows it to every client; otherwise the notice is shown at once and a row goes to that owner. `notify_last_owner` knows only an owner the agent has answered, so the tests say `hi` on chat before they expect a notice there.
- Deviation: a request refused while sign-in is closed does not count toward the 20 a minute, so the page can ask again when the lockout ends; the 429 bodies are `sign-in is closed until HH:MM` and `too many attempts; wait a minute`; a failed finish answers 403 `that did not verify; start again`, and is logged with its fixed reason. `/api/logout` also bumps the hub, so that session's socket closes at once.
- Deviation: counts are worded `1 session`, `2 sessions` in `/web`'s line; `/web rm`'s and `/web signout`'s replies are the plan's text. Added tests: `tests/cli.rs` `web_asks_the_daemon_for_a_code_and_says_when_it_cannot` (written after the command, the one test here not seen failing first) and two unit tests in `signin.rs`.

### Task W-7: Signed approvals and settings: `zephyr.web_challenge` and the `passkey` channel

**Files:**
- Modify:
  - `crates/zephyr/src/attach.rs`: `zephyr.web_challenge`; `zephyr.decide` and `zephyr.settings_set` with `challengeId` and `assertion`; new `AttachRequest::Card` and `AttachRequest::Current`;
  - `crates/zephyr/src/router.rs`:
    - `attach` gains the `Card` and `Current` arms;
    - `decide` and `settings_set` check `expect`;
    - on a passkey decision that ran, `store.audit("web", "passkey", "#ID VERB", "signed")`.
- Test: `crates/zephyr/tests/web.rs`.
- Docs:
  - DESIGN §4.3.1: `zephyr.web_challenge`, `challengeId`/`assertion`, `zephyr.web_devices`;
  - §4.3.3 "Signing an approval";
  - §4.16 "Strong channels": a payment's `/ok` is strong when signed (`passkey`);
  - §4.17: remove "Not done: the web app" (`:749`), and `web` joins the read-only row.

**Interfaces:**
- Consumes: W-1 `proposal_digest`; W-2 `Signed`, `preimage`, `Challenges`, `assertion`, `verify`; W-5 `Expect`.
- Produces:
  ```rust
  Card { id: i64, reply: oneshot::Sender<Option<(String, [u8; 32])>> },          // proposals_json's card for id (router.rs:1390), and its digest
  Current { key: String, reply: oneshot::Sender<Result<Option<String>, String>> }, // Router::current(s, key); Err = unknown key
  ```
- `zephyr.web_challenge`:
  - Accepts `{"op":"decide","id","verb":"ok"|"once","card_sha256"}` or `{"op":"set","key","value","until"?}`.
  - Card hash: `card_sha256` must equal the lowercase hex SHA-256 of `Card`'s card text, else `-32602 "the card changed; look at it again"`; `None` → `-32602 "no proposal #ID is waiting"`.
  - On success: a 16-byte nonce, `challenge = SHA-256(preimage)`, `hub.signing.issue(session, signed, challenge)` → `{"challengeId", "publicKey": {"challenge", "rpId", "timeout": 120000, "userVerification": "required", "allowCredentials": [{"type": "public-key", "id": <this session's passkey cred_id>}]}}`.
  - More than 10 a minute in a session → `-32602 "too many signatures; wait a minute"`.
- `zephyr.decide` on `web`:
  - `ok` or `once` without `assertion` → `-32602 "approve with your passkey"`; `no` and `cancel` act on `web` unsigned.
  - With `challengeId`:
    1. `take`, else `-32602 "that signature expired or was used; tap again"`;
    2. the stored `Signed::Decide`'s `id` and `verb` must equal the request's, else `-32602 "that signature is for another request"`;
    3. the assertion's id must equal the session passkey's `cred_id`, and `verify` must pass, else `-32602 "the passkey signature did not verify"`;
    4. `passkey_used`;
    5. `Decide { channel: "passkey", expect: Some(Expect { digest, card_sha256 }) }`.
- `zephyr.settings_set` on `web`:
  - Without `assertion` it acts on `web`.
  - With one: the same steps with `Signed::Set`, then `SettingsSet { channel: "passkey", expect: Some(current) }`.
- Router `decide` with `expect`: `proposal_digest(id)` and the card's hash must still match, else `{"ok": false, "text": "[zephyr] the card changed; look at it again"}`, and nothing runs. Router `settings_set` with `expect`: `current(s, key)` must equal it, else `refused "KEY changed; look at it again"`.

- [x] **Step 1: Write the failing tests** in `tests/web.rs`. Config: `[approvals] preset = "manual"`. Helper: hold a `himalaya` send with `store.hold_action(&NewAction{kind: Run, class: "messages", …})`, as `tests/router.rs:4744` does, with the payload `run_payload` of `himalaya message send …`. Helper `sign(ws, auth, op) -> Value`.
  - `an_unsigned_ok_or_a_changed_card_is_refused`: `decide ok` without `assertion` → `approve with your passkey`; `web_challenge` with `card_sha256` of `"x"` → `the card changed`.
  - `a_signed_ok_runs_the_stored_command_and_is_audited_as_passkey`:
    - `log.jsonl` (or the procs log) shows the stored command;
    - `audit_rows()` has `("web","passkey","#1 ok","signed")`.
  - `a_challenge_used_twice_is_refused` (spec).
  - `a_signature_for_one_proposal_does_not_approve_another` (Review Focus 1):
    - hold #1 and #2, and sign `decide 1 ok`;
    - `decide 2 ok` with #1's `challengeId` and assertion → `that signature is for another request`;
    - #2 stays `pending`; #1 is pending too, since its challenge was consumed.
  - `a_signature_is_refused_after_the_payload_changes` (Review Focus 1):
    - sign `decide 1 ok`;
    - then `UPDATE proposals SET payload = '{"command":"himalaya message send --to b@x.org"}' WHERE id = 1` through rusqlite;
    - `decide` → `the card changed; look at it again`;
    - nothing ran (no process for #1), and it is still `pending`.
  - `with_money_strong_only_a_payment_needs_the_signature` (spec):
    - `strong_only = ["loosen","money"]`, a `Pass` money action;
    - an unsigned `ok` is refused (needs passkey);
    - a signed one → `approved #1`.
  - `a_strong_loosening_is_refused_unsigned_and_applied_signed`:
    - with `[budget] daily_cost_usd = 5.0`, `settings_set budget.daily_cost_usd 8` unsigned → `refused`, and the reason names `passkey`;
    - signed → `changed`;
    - `settings_log` channel `passkey`.
  - `a_signed_change_is_refused_when_the_value_moved`: sign `set tasks.parallel 3` at 2; `/set tasks.parallel 4` on chat; the signed `settings_set` → `tasks.parallel changed; look at it again`.
- [x] **Step 2: Run** `cargo test -p zephyr --test web` → FAIL.
- [x] **Step 3: Implement.** Write the docs.
- [x] **Step 4: Run** the same, then `cargo test -p zephyr` → PASS.
- [x] **Step 5: Commit** `feat(web): approve cards and strong settings only with a passkey signature over exactly what is shown`.
- Deviation: the signing queue is reached through `Hub::sign` and `Hub::signed` (the field stays private to `web`), and the 10-a-minute limit on `zephyr.web_challenge` is a `Window` on the connection, so it is counted per socket rather than per session (`shortcut:` in the code).
- Deviation: the test reads the approved command from the `procs` table (`command`, `action`), where `/ok` puts a `run` action's command, rather than from `log.jsonl`, which only fake-rpi writes.
- Deviation: `web` is added to the settings registry as a read-only row (`fixed("web", …)`, test `the_web_table_is_read_only`), which is what "web joins the read-only row" needs in code. Added cases: a signature by another key with the same credential id, an assertion made for another challenge, another session using this one's challenge, an `ok` signature used for `once`, a changed `targets` column, and a signature for one value used for another.

### Task W-8: Web Push: subscriptions, cards, replies, notification buttons

**Files:**
- Modify:
  - `crates/zephyr/src/web/push.rs`: `pub fn spawn` and `async fn send`;
  - `crates/zephyr/src/web/mod.rs`: routes `/api/push/key`, `POST|DELETE /api/push`, `/api/push/decide`;
  - `crates/zephyr/src/router.rs`:
    - `push_cards`, called from `publish` `:1183` and the tick `:677`;
    - `pushed_up_to: AtomicI64`, set at start to the newest pending id;
    - `pushed_replies: HashSet<i64>`;
    - the web branch of `flush`.
- Test: `crates/zephyr/tests/web.rs`.
- Docs:
  - DESIGN §4.3.3 "Web Push";
  - README "Web app" → "Notifications";
  - the CLAUDE.md `web/` bullet gains push.

**Interfaces:**
- Consumes: W-1 `pushes` and `drop_push`; W-3 everything; W-5 `hub.connected()`.
- Produces:
  ```rust
  pub fn spawn(cfg: Arc<Config>, payload: serde_json::Value, urgency: &'static str, ttl: u32); // fire and forget; the router passes self.cfg_tx.borrow().clone()
  impl Router { fn push_cards(&self); }   // &self: AtomicI64, so Router stays Sync for the spawned router future
  ```
- `send` runs in a spawned task with a 10 s timeout.
  1. It opens `Store` and loads `vapid.pk8` itself.
  2. For each subscription: a fresh `EphemeralPrivateKey` (ECDH P-256), `agree_ephemeral` with `p256dh`, a random salt, then `encrypt`.
  3. The POST, from a `reqwest` client with `redirect(Policy::none())`, carries `TTL`, `Urgency`, `Content-Encoding: aes128gcm`, `Content-Type: application/octet-stream` and `Authorization: vapid t=<jwt>, k=<b64url public key>`.
  4. A 404 or 410 → `drop_push`. Any other failure → `eprintln!` once, with no endpoint path and no key (`shortcut: no retry; add one if pushes are seen lost`).
- `push_cards`: for each pending proposal with id > `pushed_up_to`:
  - `body` is the class and the summary (`facts::scrubbed`), as `proposals_json` gives them, or a setting card's first line;
  - `quick` = `class ∈ {messages, posts}` and `!strong`;
  - the agent's reason is never in the payload;
  - `spawn(card(...), "high", 86_400)`.
- `flush`: a `web` row while `hub.connected() == 0` → if `pushed_replies.insert(row.id)`, `spawn(reply(row.text), "normal", 3600)`. The row stays pending (`shortcut: the pushed set is in memory, so a restart may push a reply twice`).
- Routes (cookie):
  - `GET /api/push/key` → `{"key": b64url}`, or 503 while push is off;
  - `POST /api/push {endpoint, keys: {p256dh, auth}}`: `allowed_endpoint(endpoint, &cfg.push_hosts)`, else 400; then `add_push` against the session's passkey; `false` → 409; else 204;
  - `DELETE /api/push {endpoint}` → 204.
- `/api/push/decide {id, verb}`, 10 a minute per session (then 429):
  - it re-reads `AttachRequest::Proposals` and finds `id`;
  - `ok` only for a held action (`kind` `run`, `pass` or `wait`) with class `messages` or `posts` and `strong == false`; `no` for any waiting proposal;
  - else 409 `{"error":"open the app to approve this"}`;
  - then `Decide { channel: "web", expect: None }` → 200 with its `{ok, text}`.

- [ ] **Step 1: Write the failing tests** in `tests/web.rs`. The fake push service is a hyper server on 127.0.0.1 that records each request (headers, body), answering 201 or a status from a file. Set `cfg.push_hosts = ["http://127.0.0.1:PP"]` and `cfg.web_ping = 300ms`. Each subscription gets its own ring `EphemeralPrivateKey`: ring cannot import a static ECDH key, and one ephemeral key decrypts one push.
  - `a_new_card_is_pushed_encrypted_without_the_agents_reason`:
    - subscribe; hold a `messages` action with `reason: Some("trust me")`;
    - the service receives one POST with `Urgency: high`, `TTL: 86400` and `Content-Encoding: aes128gcm`;
    - `Authorization` starts `vapid t=`, its JWT verifies under `k`, and `aud` is `http://127.0.0.1:PP`;
    - `decrypt` gives `{"kind":"card","id":1,"quick":true,…}`, and the body does not contain `trust me`;
    - a restart of the gateway pushes nothing again.
  - `a_reply_is_pushed_only_while_no_socket_is_open`:
    - with a socket open, `store.notify("web", "web", "[zephyr] one")` → shown on the socket, no POST;
    - after closing it, `store.notify("web", "web", "[zephyr] two")` → one POST, `kind: reply`, body `[zephyr] two`;
    - the next tick's flush does not push it again.
  - `a_phone_that_stops_answering_pings_gets_the_reply_pushed` (Review Focus 4):
    - write `slow` = `1` in the home; open a socket and `send` a prompt;
    - then stop polling the socket, so the client answers no pings;
    - the reply arrives at the push service as `kind: reply`. That only happens once the server has closed the dead socket within `2 * web_ping`.
  - `a_410_drops_the_subscription`.
  - `push_decide_takes_ok_only_for_quick_messages`:
    - `ok` on a `messages` action → `approved #1`;
    - `ok` on a money action under `strong_only = ["money"]` → 409;
    - `no` on it → `ok: true`, and the action is `dropped`.
  - `a_subscription_outside_the_allowlist_is_refused` → 400.
- [ ] **Step 2: Run** `cargo test -p zephyr --test web push` → FAIL.
- [ ] **Step 3: Implement.** Write the docs.
- [ ] **Step 4: Run** the same, then `cargo test -p zephyr` → PASS.
- [ ] **Step 5: Commit** `feat(web): push new cards and replies to the owner's phone, and take deny from a notification`.

### Task W-9: The app: sign-in, chat, drawer, card sheet, settings, facts, vault, devices

**Files:**
- Modify: `crates/zephyr/assets/web/{index.html,app.js,app.css,sw.js,manifest.webmanifest,icon-192.png,icon-512.png}`
- Test: `web/mod.rs` `the_app_holds_no_inline_code` (W-4) keeps passing. New test `every_method_the_app_calls_is_open_on_web`: it scans `app.js` for `"zephyr.…"` and `"send"` method strings, and each must be in `attach::WEB_METHODS`.
- Docs: README "Web app": what it does, and the manual check before a release that touches `assets/web/` (iOS Safari as a Home Screen app and Android Chrome: enrol, sign in, chat, approve a card signed, deny from a notification, swipe one away on Android).

**Interfaces:**
- Consumes: W-5 to W-8's routes and methods; Snapshot fields `run`, `paused`, `queue`, `proposals`, `jobs`.
- Behaviour (spec §4.8, with these decisions):
  - **Text and the card.**
    - Daemon text only goes in through `textContent`. Control and direction marks (U+0000–U+001F except `\n`, U+007F–U+009F, U+200E/F, U+202A–E, U+2066–9) are dropped for display.
    - **The card hash is SHA-256 (`crypto.subtle.digest`) of the raw `card` string received, never the cleaned one.**
    - On the card sheet, the line starting `the agent says: ` goes into `<details><summary>the agent's reason (not checked)</summary>`.
    - An open sheet is never rebuilt. When a `zephyr.proposals` reload no longer has its id with the same card hash, it greys out ("this card changed or was handled; close and look again") and disables its buttons.
  - **Signing.**
    - Approve calls `zephyr.web_challenge` and then `navigator.credentials.get({publicKey})`, with base64url ↔ ArrayBuffer conversion, then `zephyr.decide` with `challengeId` and `assertion`.
    - Approve once is shown only when `once`. Deny and Cancel (for `scheduled`) are unsigned.
    - A settings row with `strong: true` is always signed.
  - **Enrolment and sign-in.**
    - `#enrol=CODE` fills the code field and is then dropped with `history.replaceState(null, "", "/")`.
    - Add this device: `navigator.credentials.create`, then `response.getAuthenticatorData()`, `getPublicKey()` and `getPublicKeyAlgorithm()`.
  - **The socket.**
    - Reconnect with backoff, 1 s doubling to 30 s, and `zephyr.subscribe` again.
    - Before each reconnect, `GET /api/push/key`: a 401 shows sign-in. A browser cannot read a failed upgrade's status.
    - On `visibilitychange` to hidden, the app closes its socket (so replies are pushed); on visible, it reconnects.
  - **Devices.** A button for push: `Notification.requestPermission()` on the tap, `pushManager.subscribe({userVisibleOnly: true, applicationServerKey})`. On each open, the app compares the key with the subscription's and subscribes again when they differ. Sign out; remove a passkey (sends `/web rm NAME` as a prompt).
  - **`sw.js`.**
    - Notifications tagged `card-ID` or `reply`. A card gets `actions` `[Approve, Deny]` when `quick`, else `[Deny]`.
    - `notificationclick` with an action → `fetch("/api/push/decide", {method: "POST", headers: {"Content-Type": "application/json"}, body})`. Without one → `clients.openWindow("/#held/ID")`.
    - `notificationclose` sends `no` **only when `navigator.userAgent` contains `Android`** (owner decision 3).
  - **Layout.** Phone-first; from 900 px the drawer is a side panel. Drawer tabs: Approvals (opens first when `proposals > 0`) and Upcoming (`Snapshot.jobs` and `scheduled` proposals). Activity and Goals tabs exist but stay hidden (W-11, W-12).
  - **Settings, Facts, Vault.** Settings edit each row by its `kind`, with an optional "for 2h"; a refusal shows the daemon's reason. Facts show names, kinds and targets only; Vault shows names, sites and grants. Both say "change these in the TUI or with zephyr facts / zephyr vault".

- [ ] **Step 1: Write the failing test** `every_method_the_app_calls_is_open_on_web`, and run `cargo test -p zephyr --lib web::` → FAIL (the placeholder has no calls; assert that at least `zephyr.decide` is found).
- [ ] **Step 2: Write the files** (about 1,200 lines of `app.js`). Run `node --check` on `app.js` and `sw.js` where node exists; it is optional and not a CI dependency.
- [ ] **Step 3: Run** `cargo test -p zephyr --lib web::` → PASS. Then bring up a side container (`ZEPHYR_CONTAINER=zephyr-test ZEPHYR_IMAGE=zephyr-dev:<sha> zephyr up --build`) with `[web]` set, and load the app in a desktop browser through `tailscale serve` on a test port: the sign-in page renders, and the console shows no CSP errors.
- [ ] **Step 4: Commit** `feat(web): the phone app for chat, approvals, settings and devices`.

### Task W-10: Host CLI: `zephyr web setup | enrol`, and the status line

**Files:**
- Modify: `crates/zephyr/src/host/mod.rs` (`USAGE` `:16`, `COMMANDS` `:49`, which becomes `[&str; 28]`; the `status` arm `:1176`; a new `"web"` arm)
- Test: `crates/zephyr/tests/host.rs` (a `tailscale` shim beside the `docker` one: it logs its argv to `calls.log`; `status --json` prints `$SHIM_TS_STATUS`; `serve status` prints `$SHIM_TS_SERVE`)
- Docs:
  - DESIGN §4.8: `zephyr web setup`;
  - §4.16: setup offers `money` for `strong_only`;
  - README "Web app" → "Setting it up" (the steps and the funnel warning).

**Interfaces:**
- `zephyr web setup [--yes]` asks before every change (`dialoguer::Confirm`; `--yes` answers yes) and prints each step:
  1. `tailscale status --json` → `.Self.DNSName` without its trailing dot. When `tailscale` is missing or `BackendState` ≠ `Running`, it prints how to install or log in and exits 1.
  2. `.CertDomains` must contain the name, else it prints `turn on HTTPS certificates in the Tailscale admin console (DNS → HTTPS Certificates)` and exits 1.
  3. `conf::set` `web.host` and `web.port = 7793`, then `h.data.save` (the `"config"` arm's path, `:1262-1270`).
  4. `tailscale serve --bg http://127.0.0.1:7793`. When it fails with "access denied", it prints `sudo tailscale serve --bg http://127.0.0.1:7793` and `sudo tailscale set --operator=$USER`.
  5. It prints `never point tailscale funnel at this port: the web app refuses funnel traffic, and a funnel would put it on the internet`.
  6. `h.up()`.
  7. If zephyr.toml sets `settings.strong_channels` without `passkey`: `zephyr config set settings.strong_channels <list>,passkey`, through the daemon.
  8. `docker exec … zephyr web enrol` (prints the code).
  9. After the owner presses Enter, `docker exec … zephyr web`. If the reply does not say `passkeys: none`, it offers `zephyr config set approvals.strong_only loosen,money`.
- `zephyr web enrol [NAME]` → `docker exec … zephyr web enrol [NAME]`.
- Host `zephyr status`, when `web.host` is set: run `tailscale serve status`. If the output contains `http://127.0.0.1:<port>`, print `web: tailscale serve forwards https://HOST`; else `web: tailscale serve does not forward to 127.0.0.1:PORT; run zephyr web setup`.

- [ ] **Step 1: Write the failing tests:**
  - `web_setup_writes_the_host_serves_the_port_and_enrols`:
    - `SHIM_TS_STATUS='{"BackendState":"Running","Self":{"DNSName":"box.tail1234.ts.net."},"CertDomains":["box.tail1234.ts.net"]}'`, with `--yes`;
    - zephyr.toml has `[web]`, `host = "box.tail1234.ts.net"`, `port = 7793`;
    - `calls.log` has `tailscale serve --bg http://127.0.0.1:7793`;
    - `exec.log` has `zephyr web enrol`.
  - `web_setup_stops_without_https_certificates`: `CertDomains: []` → exit 1 with the text, and zephyr.toml unchanged.
  - `status_says_whether_tailscale_serve_forwards_the_port`: both lines.
- [ ] **Step 2: Run** `cargo test -p zephyr --test host web_` → FAIL.
- [ ] **Step 3: Implement.** Write the docs.
- [ ] **Step 4: Run** the same, then `cargo test -p zephyr --test host` → PASS.
- [ ] **Step 5: Commit** `feat(web): set up the web app from the host with tailscale serve and a first passkey`.

---

## Built against sibling lanes (run after the rebase onto A4, A2 and B)

### Task W-11: Activity tab (A4's `zephyr.activity`)

**Files:**
- Modify: `crates/zephyr/src/attach.rs` (`WEB_METHODS` gains `"zephyr.activity"`), `crates/zephyr/assets/web/app.js` (show the Activity tab), `crates/zephyr/tests/web.rs`
- Docs: DESIGN §4.3.3 (the drawer's Activity tab), README "Web app".

**Interfaces:**
- Consumes (A4 plan `2026-11-10-wave1-activity.md`, else spec `2026-11-09-activity-design.md` §4.7). Assumed:
  - method `zephyr.activity`, params `{"range"?: "today"|"6h"|"7d", "kind"?: str, "before"?: "<AT.SRC.ID>", "limit"?: 1..200}`;
  - result `{"since": i64, "entries": [{"at", "kind", "subject", "status", "text", "line", "stream"}], "next": str|null}`;
  - with `kind: "grants"`, entries carry `revoke` (a command);
  - errors: `-32602` with usage, `-32000` while shutting down;
  - answered in the router through `AttachRequest::Activity { params, reply }`.
- App: Activity shows each `line` (`tool` rows muted) and asks for the next page with `before: next` when the last row scrolls into view. A Grants sub-list sends a row's `revoke` as a `send` prompt; revoking only takes away, so `web` may. A `-32601` hides the tab.

- [ ] **Step 1: Write the failing test** `activity_is_open_on_web_and_pages`: after a web `send`, `zephyr.activity {"limit":1}` → one entry with a `line`, and a non-null `next`, which `before` accepts. Also extend `every_method_the_app_calls_is_open_on_web`.
- [ ] **Step 2: Run** `cargo test -p zephyr --test web activity` → FAIL (`-32601`).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS, then `cargo test -p zephyr`.
- [ ] **Step 5: Commit** `feat(web): show the activity record and its grants in the app's drawer`.

### Task W-12: Goals screen (B's `zephyr.goals`)

**Files:**
- Modify: `crates/zephyr/src/attach.rs` (`WEB_METHODS` gains `"zephyr.goals"`), `crates/zephyr/assets/web/app.js` (the Goals screen), `crates/zephyr/tests/web.rs`
- Docs: DESIGN §4.3.3 (Goals), README "Web app".

**Interfaces:**
- Consumes (B plan `2026-11-10-wave1-goals-memory.md`, else spec `2026-11-09-goals-memory-design.md`, decision 3). Assumed:
  - `zephyr.goals {}` → `{"goals": [{"n": i64, "text": str, "done": bool}]}`, read-only;
  - changes go as the owner's text commands `/goal add TEXT`, `/goal done N`, `/goal rm N`, whose replies are `[zephyr] goal N set; …`, `[zephyr] goal N done: TEXT`.
- App: it lists the goals (done ones struck), sends the three commands as `send` prompts, and reloads `zephyr.goals` on their `zephyr_reply`. Goal text goes in through `textContent`. A `-32601` hides the screen.

- [ ] **Step 1: Write the failing test** `goals_are_read_on_web_and_changed_by_command`: a web `send` of `/goal add ship W` → a `zephyr_reply` on `web`; `zephyr.goals` → one goal, `done: false`.
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS, then `cargo test -p zephyr`.
- [ ] **Step 5: Commit** `feat(web): read the owner's goals in the app and change them with /goal`.

### Task W-13: Bridge mode: bind the container's address and publish 7793 on host loopback (A2)

**Files:**
- Modify: `crates/zephyr/src/web/mod.rs` (the bind address), `crates/zephyr/src/host/mod.rs` (A2's bridge-mode `up` argument list: `-p 127.0.0.1:<web.port>:<web.port>` when `web.host` is set), `crates/zephyr/src/main.rs` (`doctor`/`status` connect to the same address), `crates/zephyr/tests/host.rs`
- Docs: DESIGN §4.3.3 "Reaching it"; A2's §4.4/§4.6 list of listeners gains the web app.

**Interfaces:**
- Consumes (A2 plan `2026-11-10-wave1-network-floor.md`, else spec `2026-11-09-network-floor-design.md` §4.2 and §4.6). Assumed:
  - `crate::egress::own_ip() -> anyhow::Result<std::net::Ipv4Addr>` (the `zephyr` entry of `/etc/hosts`, IPv4, not loopback);
  - bridge mode is `std::env::var("ZEPHYR_NETWORK").as_deref() == Ok("bridge")`;
  - `up` builds the bridge arguments from `conf::get(&doc, "attach.port")`.

  If A2 adds a shared helper that picks the listener address, use it for the web app as `attach::serve` does.
- `web::serve` binds `(own_ip, port)` in bridge mode, else `("127.0.0.1", port)`. `own_ip` failing makes `serve` return the error, as attach does under A2. `tailscale serve` still points at `http://127.0.0.1:7793` on the host.

- [ ] **Step 1: Write the failing test** in host.rs: `up_in_bridge_mode_publishes_the_web_port_on_loopback`. A `[web]` config and `network = "bridge"` → the `docker run` args contain `-p 127.0.0.1:7793:7793`; with no `[web]`, they do not.
- [ ] **Step 2: Run** `cargo test -p zephyr --test host bridge` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS; `cargo test -p zephyr`. On the side container in bridge mode: as the agent's uid, `curl http://<own_ip>:7793/` fails, and from the host `curl -H 'Host: HOST' -H 'Tailscale-User-Login: x' http://127.0.0.1:7793/` → 200.
- [ ] **Step 5: Commit** `feat(web): listen on the container's address and publish the web app on host loopback under the bridge network`.

---

## Verification (end to end)

1. `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo deny check advisories bans`, and `cargo tree -d` unchanged from main.
2. Side container (`ZEPHYR_CONTAINER=zephyr-test ZEPHYR_IMAGE=zephyr-dev:<sha>`, its own data dir; test tailnet name): `zephyr web setup` against a second `tailscale serve` port, and `zephyr doctor` passes (the listener and the push key).
3. README's manual check on iOS Safari (Home Screen) and Android Chrome: enrol from `/web enrol`, sign in, chat, approve a held message signed, deny from a notification, swipe a card away on Android (denied), and `/web rm` from Discord ends the phone's session.
