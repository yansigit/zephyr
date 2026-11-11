# Telegram and Signal channels (C1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The owner reaches zephyr over a Telegram bot and over a Signal number of zephyr's own, both learning the owner from a one-time code issued on a strong channel, with strangers and groups ignored.

**Architecture:** Two new adapters behind `channel::Channel`, `AnyChannel::Telegram` (Bot API long polling over `reqwest`) and `AnyChannel::Signal` (a root `signal-cli jsonRpc` child on stdio, nothing listening). Files go through `inbox::append`, which gains a local-file source for signal-cli's downloads. A pure `pairing.rs` makes and checks the codes; the router issues them for `/pair`, takes a claim before the owner check drops a stranger's row, and appends the sender to `owners.<channel>` through `toml_edit`. The host gets `zephyr channel add|remove telegram|signal`, their `init` steps and a container-side `zephyr signal-setup`; the image gets signal-cli behind `SIGNAL`.

**Tech Stack:** Rust (MSRV 1.89, CI 1.95), tokio, reqwest 0.12 (json, multipart), hyper 1 for test stubs, toml_edit, ring, data-encoding, subtle; Python for `tests/fake-signal-cli.py`; signal-cli 0.14.10 (JVM) on a jlinked Temurin 25 JRE.

**Spec:** `docs/superpowers/specs/2026-11-11-channels-voice-design.md` §4.1–4.3, §4.7 (the `[telegram]`/`[signal]` and `CHANNELS` lines), §4.8 (`signal` stage), §4.9 (`telegram`, `signal` lines), Tests (Telegram, Signal). Its closing "Decided with the owner (2026-11-11)" wins. Wave index: `docs/superpowers/plans/2026-11-11-wave2.md`.

## Global Constraints

- Everything under the wave 1 and wave 2 index's **Global Constraints**: commit form `type(scope): sentence`, authored as the repo's user, no trailers; failing test first; `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, the task's tests, then the full `cargo test` before each commit; DESIGN.md and README.md in the same task as the behaviour.
- Branch `claude/channels` from `origin/docs/wave2-specs`. Task ids C1-1 … C1-6. **No schema change, no new crate.**
- A Telegram bot token and signal-cli's keys never appear in a log, `Debug`, an audit row, a notice or an error: every Bot API error goes through `without_url()` and the token is redacted from any text that leaves the adapter; no URL is logged.
- Root runs signal-cli by full path (`/opt/zephyr/signal/bin/signal-cli`, `Config::signal_cli`), environment cleared to `PATH`, `JAVA_HOME`, `JAVA_TOOL_OPTIONS=-Xmx256m -XX:+UseSerialGC`; data dir `<state>/signal` (root, 0700), never under `<home>`, not in the daily backup.
- Nothing new listens: no port, no socket (`egress::published` is unchanged).
- Copy, byte for byte: `[zephyr] send /pair ABCDE-FGHJK to the bot within 10 min; it works once`; `[zephyr] paired: you are an owner of this zephyr`; `[zephyr] Telegram user 123456789 ("Aiden", @aiden) is now an owner; /unpair telegram 123456789 removes them`; `[zephyr] the Signal safety number of 7f3c… changed; compare it, then run zephyr channel signal trust 7f3c…`.
- Pairing code: 6 random bytes, base32, shown `XXXXX-XXXXX`, lives 10 min, works once, only its SHA-256 kept (meta `pair_telegram` / `pair_signal`); 5 wrong codes in 10 min close pairing for `web_lockout` (15 min), the owner told once.
- Chunks: Telegram 4096 UTF-16 units, Signal 2000; retry window 5 min on both; a 429 waits its `retry_after`.
- Side container only: `ZEPHYR_CONTAINER=zephyr-test-c1`, attach port 19790, web port 19793, scratch HOME, bridge network. Never `zephyr`, `zephyr-test`, `~/.zephyr*` or the host's tailscale.
- C2 (voice, `inbox.derived`, takeover) is out of scope; leave its seams (below).

## Review Focus

1. **A stranger messages the bot or the Signal number** (text, a photo, a voice note): the row is `dropped`, no `sendMessage`/`sendChatAction`/`getFile` (Telegram) or `send`/`sendTyping` (Signal) is made, no file lands in `inbox/`, and signal-cli's copy of the file is deleted. Tests `a_stranger_gets_nothing` in C1-3 and C1-4.
2. **The bot is added to a group** (a `my_chat_member` for a group, supergroup or channel, or a group message after zephyr was down): `leaveChat`, no row. Test `a_group_is_left_and_never_a_row` in C1-3.
3. **A display name or `@username` equal to the owner's** on a stranger's account: never an owner; only `from.id` / `sourceUuid` count. Test `a_name_is_never_an_identity` in C1-3.
4. **The code used twice, late, after a restart's re-delivery, or guessed**: once only, expired at 10 min, a re-delivered `/pair CODE` is a duplicate row and claims nothing, wrong guesses get no reply and close pairing at five, and the stored row reads `/pair`. Tests in C1-2.
5. **signal-cli linked to the owner's own account** (every message a `syncMessage`, and data messages from zephyr's own number): the channel hears nothing. Test `sync_messages_and_our_own_are_ignored` in C1-4.

## Decisions this plan makes where the spec is silent or wrong

1. **Where the claim is checked.** `Inbound` has 41 constructors and C2 adds `derived` to it, so the display name for the pairing notice does not travel on `Inbound`. Adapters remember the last name each sender used (`Channel::known_as`, default `None`), and the router checks a claim in `main`'s intake, where it holds the channels, before `inbound` runs its owner check. The order the spec asks for holds: claim, then owner check, then the write.
2. **`[telegram]` is `discord::Config`** (`token_env`, `progress`, `progress_after_secs`): the same three keys.
3. **The lockout** reuses the web app's `signin::Limits` and `Config::web_lockout` (15 min), as the spec's "as the web app's sign-in lockout does" reads.
4. **"Every owner channel is told"** is each owner id on `chat` and on every channel whose table is set, except the new owner (`router::notify_owners`), plus attached clients.
5. **Group messages** on Telegram also get `leaveChat` (one line), so a group joined while zephyr was down for over 24 h is still left.
6. **signal-cli flags**: `--data-dir D -a ACCOUNT jsonRpc --ignore-stories --ignore-avatars --ignore-stickers`. 0.14.10's data message has `groupInfo` only; `groupV2` is checked too.
7. **signal-cli downloads every attachment**, a stranger's too, into `D/attachments/<id>`. The adapter deletes each one after `inbox::append`, owner or not; a stranger's is never read.
8. **Safety numbers.** The daemon's signal-cli holds the account lock, so `zephyr channel signal trust ID` runs `zephyr signal-setup trust ID` in the container, which drops `D/.trust` (root, 0600); the adapter checks it every 2 s and sends `trust {recipient, trustAllKnownKeys: true}`. The notice goes once per ACI per daemon run through `router::notify_owners`, opened on `state.db` by the adapter.
9. **doctor's "the child is running and `listAccounts` names `account`"**: the adapter writes `D/.ok` holding the account after `listAccounts` names it at each child start and removes it when the child exits; doctor compares.
10. **The link QR** comes from `qrencode -t ansiutf8`, apt in the image (no crate).
11. **`channel remove signal`** runs `unregister` (best effort; it also unlinks a linked device), then deletes `D`.
12. **The channel's `reconnecting` state** stays the snapshot's existing shortcut (adapters report no state); a restarting signal-cli is logged.
13. **Pairing replies the spec leaves open:** while pairing is closed `/pair` answers `[zephyr] pairing is closed until HH:MM`; the lockout notice goes to every owner channel (`notify_owners`, not the last owner: a command-only row never becomes that); `/unpair` answers `[zephyr] 55 is no longer an owner on telegram` or `[zephyr] 55 is not an owner on telegram`; pair and unpair are `settings_log` rows on `owners.<channel>` (actors `pairing`, `owner`).
14. **`zephyr pair KIND`** (container) asks again for 30 s while the daemon answers `not set up` or its socket is there but takes nothing: `channel add`'s reload may still be re-execing the daemon onto the new channel.

## Seams left for C2

- Telegram `voice`/`audio` and Signal `isVoiceNote` attachments reach `inbox::append` as `inbox::File` with their `audio/*` type and `Src::Get` / `Src::Path`; C2 hooks transcription in `append` after the bytes are read in that call.
- `telegram::send` sends an attachment named `voice.ogg` with `sendVoice`; Signal sends any attachment, `voice.m4a` included.
- `settings::CHANNELS` and `Snapshot.channels` already list `telegram` and `signal`.

---

### Task C1-1: Seams: shared chunks, generic progress note, local-file attachments, `[telegram]` and `[signal]` config

**Files:**
- Modify: `crates/zephyr/src/channel.rs`, `crates/zephyr/src/discord.rs`, `crates/zephyr/src/imessage.rs`, `crates/zephyr/src/inbox.rs`, `crates/zephyr/src/config.rs`, `crates/zephyr/src/reload.rs`, `crates/zephyr/src/drafts.rs`, `crates/zephyr/src/settings.rs`, `crates/zephyr/src/router.rs` (`publish` channel list), `crates/zephyr/src/lib.rs`
- Create: `crates/zephyr/src/telegram.rs`, `crates/zephyr/src/signal.rs` (each only its `Config` here)

**Interfaces:**
- Produces:
  ```rust
  // channel.rs
  pub fn chunks(text: &str, max: usize) -> Vec<String>;            // moved from discord
  trait Channel { fn known_as(&self, sender: &str) -> Option<String> { None } }
  // discord.rs
  pub enum Step<Id> { Wait, Post, Edit(Id) }
  pub struct Progress<Id> { .. pub msg: Option<Id> .. }            // fn warn(&mut self, what, e: &dyn Display)
  // inbox.rs
  pub enum Src { Get(reqwest::RequestBuilder), Path(PathBuf), Skip(&'static str) }
  pub struct File { pub name: String, pub mime: Option<String>, pub size: u64, pub src: Src }
  pub fn owner(live: &Live, inbound: &Inbound) -> bool;
  // telegram.rs
  pub use crate::discord::Config;
  // signal.rs
  #[serde(deny_unknown_fields)] pub struct Config { pub account: String }
  pub fn data_dir(cfg: &crate::config::Config) -> PathBuf;          // <state>/signal
  // config.rs
  pub telegram: Option<crate::telegram::Config>, pub signal: Option<crate::signal::Config>,
  #[serde(skip, default)] pub telegram_api: String  // "https://api.telegram.org"; kept by keep_hidden
  #[serde(skip, default)] pub signal_cli: PathBuf   // "/opt/zephyr/signal/bin/signal-cli"; kept by keep_hidden
  ```
- `Src::Path` is read as root (before `agent::fs()`), refused over `MAX_BYTES`; `Src::Skip(why)` lists the file `(…, why)` for an owner.
- `reload::diff`: a change to `telegram` or `signal` is `Reexec`. `drafts::reserved` adds `telegram.token_env`. `settings::CHANNELS` gains `telegram`, `signal`; `fixed("telegram", "Telegram", "The Telegram bot.")`, `fixed("signal", "Signal", "The signal-cli adapter.")`. `publish` lists `telegram` and `signal` as `off` when absent.

- [x] **Step 1: Write the failing tests:** `inbox::tests::a_local_file_is_copied_and_a_skipped_one_listed` (owner: `Src::Path` copied into `inbox/signal-…-1.m4a` with its bytes; `Src::Skip("too large, not downloaded")` → `[attached file: big.pdf (application/pdf, 30.0 MB, too large, not downloaded)]`; a stranger's `Src::Path` → `not downloaded`, file never read); `config::tests::telegram_and_signal_tables_parse` (`[telegram] token_env`, `[signal] account`, `telegram_api` default); `reload::tests` case `[telegram]`/`[signal]` added → `Reexec`; `drafts` reserved case for `telegram.token_env`; `settings` `CHANNELS` holds both. Existing chunk tests move to `channel.rs`.
- [x] **Step 2:** `cargo test -p zephyr --lib -- inbox config reload drafts settings channel` → FAIL.
- [x] **Step 3:** Implement.
- [x] **Step 4:** Same → PASS; `cargo test` → PASS.
- [x] **Step 5:** Commit `refactor(channel): share chunking and the progress note, take attachments from a local file, and read [telegram] and [signal]`.

### Task C1-2: Pairing: `/pair`, the claim, `/unpair`

**Files:**
- Create: `crates/zephyr/src/pairing.rs` (pure)
- Modify: `crates/zephyr/src/web/signin.rs` (`Limits`, `random`, `typed` become `pub(crate)`), `crates/zephyr/src/router.rs` (`main` intake, `command`, `notify_owners`), `crates/zephyr/tests/router.rs` (harness takes channel names), `crates/zephyr-policy/src/tools.rs` (test only), `DESIGN.md` §4.2 step 2, §4.3, §4.16, §4.17, `README.md`

**Interfaces:**
- Produces:
  ```rust
  // pairing.rs
  pub const LIFE: i64 = 600;
  pub const CHANNELS: [&str; 2] = ["telegram", "signal"];
  pub fn issue(now: i64) -> (String /* "ABCDE-FGHJK" */, String /* meta row: {"sha256","expires"} */);
  pub fn claimed(text: &str) -> Option<String>;           // "/pair CODE" whole text → typed code
  pub fn matches(row: &str, code: &str, now: i64) -> bool; // constant-time, unexpired
  // router.rs
  pub(crate) fn notify_owners(cfg: &Config, store: &Store, text: &str, except: Option<(&str, &str)>) -> rusqlite::Result<()>;
  ```
- Router: `("/pair", "telegram"|"signal")` only from `strong_channels` minus `passkey` (else `[zephyr] /pair needs tui or chat`), and only when that channel's table is set (else `[zephyr] telegram is not set up; zephyr channel add telegram on the host`); signal's reply says `to zephyr's Signal number`. `("/unpair", "CHANNEL ID")` from any owner: removes the id through `edit_config`. In `main`, a row on `telegram`/`signal` whose `pairing::claimed` is `Some` goes to `Router::claim(m, name)`: insert `dropped` with text `/pair` (duplicate → nothing), refuse while `Limits::closed`, `take_meta` on a match then append `from` to `owners.<channel>` and answer the copy above; a miss counts in `Limits::failed`, and the fifth tells the owner `[zephyr] 5 wrong pairing codes; pairing is closed until HH:MM` once.

- [x] **Step 1: Write the failing tests.** `pairing.rs`: `a_code_matches_once_typed_any_way` (lower case, without the hyphen), `a_code_expires`, `claimed_needs_the_whole_text`. `tests/router.rs` (gateway with a `telegram` test channel and `[telegram] token_env = 'T'`): `pairing_makes_the_sender_an_owner` (`/pair telegram` on chat → reply matches `send /pair [A-Z2-7]{5}-[A-Z2-7]{5} to the bot within 10 min; it works once`; `/pair CODE` from `55` on telegram → `owners.telegram = ["55"]` in the file, reply `paired…` on telegram, the chat owner gets `Telegram user 55 is now an owner; /unpair telegram 55 removes them`, the inbox row text is `/pair` with status `dropped`; the next message from `55` is answered by the agent); `a_pairing_code_works_once_and_expires`; `wrong_codes_close_pairing_and_get_no_reply` (5 wrong → one notice, then the right code claims nothing); `pair_needs_a_strong_channel` (from discord → refused); `unpair_removes_an_owner`. `zephyr-policy`: `gateway_kind("/pair telegram") == None`.
- [x] **Step 2:** `cargo test -p zephyr --lib pairing` and `cargo test -p zephyr --test router pair` → FAIL.
- [x] **Step 3:** Implement.
- [x] **Step 4:** Same → PASS; `cargo test` → PASS.
- [x] **Step 5:** DESIGN (pairing in §4.3, the claim in §4.2 step 2, Telegram and Signal weak in §4.16, `CHANNELS` in §4.17), README `/pair`, `/unpair`. Commit `feat(pairing): learn a Telegram or Signal owner from a one-time code issued on a strong channel`.

### Task C1-3: The Telegram adapter

**Files:**
- Modify: `crates/zephyr/src/telegram.rs`, `crates/zephyr/src/channel.rs` (`AnyChannel::Telegram`), `crates/zephyr/src/main.rs` (build it; doctor line), `DESIGN.md` §4.3, §4.20, `README.md`
- Create: `crates/zephyr/tests/telegram.rs` (a stub Bot API on `Config::telegram_api`)

**Interfaces:**
- Consumes: C1-1 `chunks`, `Progress<i64>`, `inbox::{File, Src, owner, append}`, `Channel::known_as`; C1-2 the router's claim.
- Produces: `pub struct Telegram` with `pub fn new(cfg: &Config, api: &str) -> anyhow::Result<Self>`, `impl Channel` (`name` `telegram`), `pub async fn me(&self) -> anyhow::Result<String>` (getMe's `@username`, for doctor); `pub const MAX_TEXT: usize = 4096`.
- Behaviour: spec §4.1 exactly (deleteWebhook once; `getUpdates {offset, timeout: 50, allowed_updates: ["message", "my_chat_member"]}` on a 60 s request timeout; offset confirmed on the next call; 409 logged once, retried after 30 s; other errors back off 5 s doubling to 5 min, never ending `run`; private chats with `from.id == chat.id`, not a bot; bare `/start` dropped; `platform_msg_id = "<chat.id>-<message_id>"`; text or caption; files photo (largest), document, audio, voice, video, video_note; `getFile` only when `inbox::owner`; over 20 MB `Src::Skip("too large, not downloaded")`; group `my_chat_member` (status not `left`/`kicked`) or group message → `leaveChat`; sends `sendMessage` without `parse_mode`, then `sendDocument` (`sendVoice` for `voice.ogg`) multipart; chunks a `client_id` already delivered are skipped on a retry for 5 min; 429 → sleep `retry_after`, then again; typing `sendChatAction typing` every 4 s; the progress note posted, edited with `editMessageText`, deleted with `deleteMessage`). Names remembered: `"Aiden", @aiden`.
- doctor: `ok   telegram bot @NAME answers getMe` or the scrubbed failure; `env T is set (telegram token)`.

- [x] **Step 1: Write the failing tests** in `tests/telegram.rs` with the stub recording `(method, body)` and the router on fake-rpi with `chat` and the real adapter: `a_stranger_gets_nothing` (text, a photo, a voice note from `999` → rows `dropped`, the stub saw only `deleteWebhook`/`getUpdates`, no `inbox/`); `an_owner_is_answered_in_chunks_with_files` (owner `55` sends a photo with a caption → saved to `inbox/telegram-55-7-1.jpg` via `getFile` + `/file/bot<T>/photos/a.jpg`, the agent's reply arrives as `sendMessage {chat_id: 55, text}` with no `parse_mode`); `a_group_is_left_and_never_a_row`; `start_alone_is_not_a_row`; `a_name_is_never_an_identity` (stranger whose first name and username equal the owner's → dropped); `pairing_over_telegram_names_the_user` (notice holds `("Aiden", @aiden)`); `a_retry_resends_only_the_failed_chunk` (stub fails the second `sendMessage` once → chunks sent 1, 2(fail), 2); `a_conflict_backs_off`; `the_token_never_shows` (a 500 and an unreachable API → every log line and error lacks the token); `offsets_are_confirmed_on_the_next_call` (restart re-reads unconfirmed updates once; the duplicate row is ignored). Unit: `to_inbound` filters, chunk size 4096.
- [x] **Step 2:** `cargo test -p zephyr --test telegram` → FAIL.
- [x] **Step 3:** Implement.
- [x] **Step 4:** Same → PASS; `cargo test` → PASS.
- [x] **Step 5:** DESIGN §4.3 Telegram bullet, §4.20 (its connections are the daemon's own); README "Telegram". Commit `feat(telegram): talk to the owner through a Telegram bot in private chats`.

Deviations: `Telegram` has two hidden public fields, `conflict_wait` (30 s) and `backoff` (5 s), so tests need not wait 30 s. `the_token_never_shows` checks the errors `me()` and `send()` return (against a stub whose error text holds the token, and an unreachable API) rather than captured log lines: the adapter logs to the test's own stderr, and every log line prints one of those scrubbed errors. The token's secret half (after `:`) is redacted too. `/start PAYLOAD` is kept; only a bare `/start` is dropped. Downloads get 120 s. The 409 is logged once per run of conflicts (the flag resets after a good poll). The names map is cleared at 1000 senders (`shortcut:`). `pairing_over_telegram_names_the_user` was added at integration, with the claim from C1-2.

### Task C1-4: The Signal adapter

**Files:**
- Modify: `crates/zephyr/src/signal.rs`, `crates/zephyr/src/channel.rs` (`AnyChannel::Signal`), `crates/zephyr/src/main.rs` (build it; doctor lines; `signal-setup` command), `DESIGN.md` §4.3, §4.4 item 1, §4.20, `README.md`
- Create: `crates/zephyr/tests/fake-signal-cli.py`, `crates/zephyr/tests/signal.rs`

**Interfaces:**
- Consumes: C1-1, C1-2 (`router::notify_owners`).
- Produces: `pub struct Signal` with `pub fn new(cfg: &crate::config::Config) -> anyhow::Result<Self>`, `impl Channel` (`name` `signal`); `pub const MAX_TEXT: usize = 2000`; `pub async fn setup(cfg, args: &[String]) -> anyhow::Result<()>` for `zephyr signal-setup register NUMBER [--voice] | link | trust ACI | remove ACCOUNT`.
- Behaviour: spec §4.2 with decisions 6–9 (child spawned with `kill_on_drop`, restarted 5 s doubling to 5 min; JSON-RPC ids `z<n>`; `receive` notifications; only `dataMessage` without `groupInfo`/`groupV2` from a `sourceUuid` that is not our own account; `syncMessage`, typing, receipts, stories, `editMessage`, reactions, remote deletes ignored; row `"<sourceUuid>-<timestamp>"`, sender `sourceUuid`; attachments `Src::Path(D/attachments/<id>)` for an owner when `id` is a plain file name, then deleted; `send {recipient: [ACI], message, attachments}`, chunked; `IDENTITY_FAILURE` → error and the once-per-ACI notice; `sendTyping` every 10 s, `stop: true` on `Stop`). Names: `sourceName`, `sourceNumber`.
- fake-signal-cli.py: logs argv and each request to `D/log.jsonl`; replays lines appended to `D/feed.jsonl` as `receive` notifications; answers `listAccounts` `[{"number": ACCOUNT}]`, `send` `SUCCESS` (or `IDENTITY_FAILURE` while `D/untrusted` exists), `sendTyping`/`trust` `{}`; exits when `D/exit` appears (removing it).

- [x] **Step 1: Write the failing tests** in `tests/signal.rs`: `a_stranger_gets_nothing` (no `send`/`sendTyping`, row dropped, their attachment deleted from `D/attachments`, nothing in `inbox/`); `an_owner_is_answered_and_their_file_copied` (`inbox/signal-…-1.png` holds the bytes, the copy in `D` is gone, reply via `send {recipient: [ACI]}`); `sync_messages_and_our_own_are_ignored`; `group_messages_are_ignored`; `pairing_works_by_source_uuid`; `the_child_restarts_after_an_exit` (two `argv` lines with `jsonRpc`); `a_changed_safety_number_is_told_once` (`D/untrusted` → one notice to the chat owner with the copy above; `D/.trust` consumed → `trust` sent); `the_data_dir_is_roots_and_private` (`D` 0700, not under `home` when `state` is elsewhere); `the_child_gets_a_clean_environment` (fake logs its env: only `PATH`, `JAVA_HOME`, `JAVA_TOOL_OPTIONS`).
- [x] **Step 2:** `cargo test -p zephyr --test signal` → FAIL.
- [x] **Step 3:** Implement.
- [x] **Step 4:** Same → PASS; `cargo test` → PASS.
- [x] **Step 5:** DESIGN §4.3 Signal bullet, §4.4 item 1 (signal-cli is a root child), §4.20; README "Signal" (the durability gap, never your own account). Commit `feat(signal): talk to the owner through a signal-cli child on a number of zephyr's own`.

Deviations: `link` runs without `-a` (signal-cli 0.14.10 refuses an account there). Our own ACI comes from signal-cli's `data/accounts.json` (`listAccounts` gives only the number); a message from it or from `account` is dropped. Every attachment an envelope names (a sync's, a group's, a quote's thumbnail) is deleted from `D/attachments`, not only a taken message's. Also ignored: `callMessage`, a `dataMessage` with `reaction` or `remoteDelete` or with no text and no files, a `sourceUuid` that is not a UUID. `remove [ACCOUNT]` falls back to `[signal] account` and gives `unregister` 60 s. `trust` removes an unconsumed `.trust` after 30 s and fails. The backoff starts over at 5 s after a child that ran over 5 min. signal-cli's `PATH` is `/usr/sbin:/usr/bin:/sbin:/bin`, and `JAVA_HOME` the `jre` beside its `bin`. **Found at integration and fixed:** the captcha token and the SMS code went to signal-cli as arguments, readable by the agent's uid in `/proc/<pid>/cmdline`; `register` and `verify` now go over the stdin of a multi-account `signal-cli jsonRpc` (`Registrar`, test `the_captcha_and_the_code_never_go_on_signal_clis_command_line`). `pairing_works_by_source_uuid` was added at integration. Not run against a real signal-cli: register, link, the trust wait and remove (only their refusals are tested).

### Task C1-5: The host: `zephyr channel add|remove telegram|signal`, `init`, `zephyr pair`

**Files:**
- Modify: `crates/zephyr/src/host/setup.rs` (`channel`, `channel_add`, `channel_remove`, `channel_list`, `init`), `crates/zephyr/src/host/mod.rs` (`USAGE`), `crates/zephyr/src/main.rs` (`pair` command), `crates/zephyr/tests/host.rs`, `README.md`

**Interfaces:**
- Consumes: C1-2 `/pair`, C1-4 `signal-setup`.
- Produces: container `zephyr pair telegram|signal` (asks `/pair KIND` over the chat socket, retrying for 30 s while the reply says `not set up`, prints the reply). Host: `channel add telegram` asks the token (hidden, `ZEPHYR_TELEGRAM_TOKEN`), prints the `@BotFather /setjoingroups → Disable` instruction, writes `telegram.token_env`, reloads, runs doctor, then `zephyr pair telegram`; `channel add signal` refuses with `signal: not in this image (built with SIGNAL=0)` when `/opt/zephyr/signal` is missing, asks the number, runs `docker exec -it zephyr signal-setup register|link`, writes `signal.account`, reloads, pairs; `channel remove signal` also runs `signal-setup remove`; `channel signal trust ACI`; `list` shows both; `init` offers both.

- [x] **Step 1: Write the failing tests** in `tests/host.rs`: `channel_list_and_remove_cover_telegram_and_signal` (tables written by hand → listed; `remove telegram` drops `[telegram]`, `owners.telegram` and `ZEPHYR_TELEGRAM_TOKEN`); `channel_add_refuses_without_a_terminal` for both; `channel_usage_names_telegram_and_signal`.
- [x] **Step 2:** `cargo test -p zephyr --test host channel` → FAIL.
- [x] **Step 3:** Implement.
- [x] **Step 4:** Same → PASS; `cargo test` → PASS.
- [x] **Step 5:** README channel commands. Commit `feat(host): add and remove the Telegram and Signal channels, and pair their owner`.

Deviations: `channel add` of a paired kind asks for the code before running doctor, and a failed doctor fails the command for every kind, as before (review fix: no doctor check depends on pairing). `zephyr pair signal` first waits up to 60 s for signal-cli to hold the account. Adding Signal again asks everything first, and setup holds the account (`.hold`, its pid) so the daemon's signal-cli stops meanwhile; the config changes only once setup succeeded (second review). `add signal` checks the number's form on the host and needs the container running. `init` asks `Set up telegram?` and `Set up signal?` after iMessage and pairs each one set up after its final doctor; the driven-tty test `init_asks_for_a_preset_and_writes_it` answers the two new prompts. A `zephyr pair` test covers its usage and the missing daemon (exit 3); the code path itself is the router's, tested in C1-2.

### Task C1-6: The image: signal-cli behind `SIGNAL`

**Files:**
- Modify: `deploy/Dockerfile`, `README.md` "Building the image" (the doctor lines came with C1-4: the data dir, `.ok`, a missing signal-cli; no separate `--version` line)

**Interfaces:**
- `ARG SIGNAL=1`, `SIGNAL_CLI_VERSION=0.14.10` with its tarball's sha256, Temurin 25 JDK pinned by sha256; stage `signal-1` (amd64: unpack signal-cli, `jlink --add-modules $(jdeps --print-module-deps …)` into `/out/signal/jre`, a `bin/signal-cli` wrapper setting `JAVA_HOME`), `FROM scratch AS signal-0`, `FROM signal-${SIGNAL} AS signal-out`; arm64 builds an empty stage. Final: `COPY --from=signal-out / /opt/zephyr/signal/` only when non-empty (copy a directory that always exists), `qrencode` in the apt list.

- [x] **Step 1:** `cargo test -p zephyr every_copy_in_the_dockerfile_is_in_the_build_context` still passes after the edit.
- [x] **Step 2:** `docker build -t zephyr-dev:c1-<sha> -f deploy/Dockerfile --secret id=gh_token,env=GH_TOKEN .` succeeds; `docker run --rm zephyr-dev:c1-<sha> /opt/zephyr/signal/bin/signal-cli --version` prints `signal-cli 0.14.10`; the image size is recorded below.
- [x] **Step 3:** Side container `zephyr-test-c1` (scratch HOME, ports 19790/19793, bridge) with `[telegram]` against an unreachable `api` and `[signal]` on an unregistered number: the daemon runs, `zephyr status` and `doctor` answer, the logs show the adapters retrying, the log holds no token. Remove the container, keep the image.
- [x] **Step 4:** Commit `build(image): ship signal-cli and its own Java runtime behind the SIGNAL build arg`.

Deviations: pins `SIGNAL_CLI_SHA256=706a57d9…121c33` and Temurin `25.0.5+8` (`68982529…78d7fb`). BuildKit reuses the cached layer of a `COPY --from` an empty stage, so a `SIGNAL=0` build after a `SIGNAL=1` one still had the folder: both stages hold `/out` and the final stage copies `/out/` to `/opt/zephyr/`. `jdeps` follows the main jar (`--recursive --class-path 'lib/*'`), since the whole `lib/` does not resolve as modules; `jlink --strip-debug` needs binutils in the build stage. Upstream's start script is kept as `bin/signal-cli-jvm`, and `bin/signal-cli` is a two-line wrapper pinning `JAVA_HOME`. Measured: `/opt/zephyr/signal` 190 MB (jars 126 MB, runtime 64 MB); the image's `docker image inspect` size grows by 365 MB, the content to pull from 1.45 GB to 1.61 GB. `signal-cli link` with no account fetched a `sgnl://linkdevice` URI from Signal's servers on the trimmed runtime.

Side-container result (image `zephyr-dev:c1-e53f613`, the branch with every task; container `zephyr-test-c1`, scratch HOME, attach on 19790, its own bridge network; removed afterwards, image kept): `signal-cli --version` prints `signal-cli 0.14.10` on Temurin 25.0.5+8, `/opt/zephyr/signal` is root's 0755. With `[telegram]` on a fake token and `[signal]` on an unregistered number the daemon ran: Telegram's real API answered `401 Unauthorized` and the poll backed off 5, 10, 20 s; signal-cli printed `User +15550100999 is not registered.`, exited, and was started again after 5, 10, 20 s. `zephyr status` exit 0; `zephyr doctor` exit 1 on exactly `telegram getMe … 401` and `signal-cli is running and holds +15550100999`, with `env ZEPHYR_TELEGRAM_TOKEN is set` and `/var/lib/zephyr/signal is the daemon's and 0700` ok and every earlier check ok. The token appears 0 times in `docker logs`. The JVM runs as root, a child of `zephyr run`, its environment `PATH`, `JAVA_HOME`, `JAVA_TOOL_OPTIONS` and the wrapper's `PWD`; as uid 1000, `ls /var/lib/zephyr/signal` and `ls /var/lib/zephyr` are refused. The agent's uid can read the JVM's argv in `/proc`, which is why the registration's secrets moved to stdin.

## Review fixes (`/code-review high`, 2026-11-11)

1. A claim during a drain (shutdown or re-exec) went through `take_in` past the claim and stored its code: the claim is now at the top of `Router::inbound`, one path (`a_claim_taken_in_while_draining_is_still_a_claim`; a shutdown drain with a run going, the same `drain`/`take_in` a re-exec uses).
2. and 10b. A send the router stopped waiting on after 10 s resent its delivered piece: `channel::Delivered` (shared) runs each Telegram or Signal send in a task of its own that records each piece taken, refuses a retry while a send still goes, and forgets after `router::retry_window` (now `pub`, the source of truth) (`a_send_the_router_stopped_waiting_for_never_sends_a_piece_twice` in both channels' tests).
3. Wrong codes counted with no code out: now only while `pair_<channel>` holds a live code; the notice was already once per lockout (claims while closed are not counted) (`wrong_codes_with_no_code_out_close_nothing`).
4. Any `/pair WORD` was a claim: only ten base32 characters are, so `/pair telegram` from a channel in `strong_channels` is the command (`a_strong_telegram_owner_may_ask_for_a_code`).
5. Telegram's offset moved on after the mpsc send: it moves once the router stored each row (`Store::has_inbox`, read on a blocking thread, a minute at most). Discord and iMessage hand rows over the same way and have no offset to hold, and storing in the adapter would race the router's own insert, so the adapter reads the store instead (`an_update_is_confirmed_only_once_its_row_is_stored`).
6. `channel add` ignored doctor: a failed doctor fails it again for every kind; paired kinds ask for the code first. Doctor has no check that waits on pairing, so none is ignored (`channel_add_fails_when_doctor_does`).
7. A second signal-cli waited on the daemon's lock: adding Signal again drops `[signal]` and reloads first, setup waits up to 30 s for `.ok` to go, and every setup signal-cli is bounded (2 min; a link's scan 10 min) (`adding_signal_again_stops_the_daemons_signal_cli_first`; the bounds themselves are untested constants).
8. The safety-number notice counted as told before it was written, from a synchronous `Store::open` on the send path, and stopped at one owner's error: it is written on a blocking thread, counts only once stored, and `notify_owners` tries every owner and returns the first error (`a_safety_number_counts_as_told_only_once_the_notice_is_stored`).
9. The JVM used `/tmp`: `JAVA_TOOL_OPTIONS` adds `-XX:-UsePerfData -Djava.io.tmpdir=<state>/signal/tmp`, that folder made 0700 (`the_jvm_keeps_its_temporary_files_in_roots_folder`).
10a. One code helper (`pairing::{issue, live, matches, typed, issuers}`) for pairing and web enrolment.

After the fixes: the full `cargo test` 1202 passed, 0 failed, 18 ignored. The image's signal-cli (`zephyr-dev:c1-e53f613`) runs with the new `JAVA_TOOL_OPTIONS` and leaves no `hsperfdata` in `/tmp`; the image was not rebuilt for these fixes.

Found while fixing, then fixed: `an_owner_is_answered_in_chunks_with_files` failed about one run in five, already at `6787b09`. Root cause, from probes on the router's loop: the auth check came due right after the prompt and `auth_check()` held the main loop for the whole run (1002 ms of a 1 s run) waiting on the child's `get_auth_status`, so `agent_start` and every later event were taken only after the run had ended; the snapshot never read `busy`, and `show_activity` sent only `Stop`. It is a product bug: with real rpi the same loop waits on that answer and, near a token's expiry, on the `rpi auth refresh` subprocess, holding a run's events, typing, progress note and steering meanwhile. The auth check now never runs while `busy` (a check due during a run waits for its settle; a token refreshed then would bind only at an idle restart anyway). fake-rpi logs each command's arrival with whether a run was going (`arrived.jsonl`), and `an_auth_check_due_during_a_run_waits_for_its_settle` failed first; `refresh_while_busy_restarts_only_after_the_settle` now expects the refresh after the run's settle. The Telegram test then passed 20 of 20 beside the router suite and 40 of 40 (two at a time) beside the cli suite. The origin's channel name matches the adapter's (`telegram`), the snapshot is published before the run, `live` is wired before the channels run, and an `activity` before a run exists is a no-op that the next tick's `Start` repeats; none of those was the cause.

## Owner checks (not verifiable here)

- A real Telegram round trip: `zephyr channel add telegram` with the owner's bot token, `/pair`, a message, a photo, a voice note saved as a file, the bot added to a group leaves.
- A real Signal registration on zephyr's own number (captcha, SMS or voice code), `/pair`, a message and a file both ways, and a safety-number change and `zephyr channel signal trust`.
