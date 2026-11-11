# Voice and desktop takeover (C2) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** An owner's voice note on any channel is transcribed locally and appended to its row as file-derived text, a reply to it is also spoken, and the owner can take the agent's desktop over from the web app while the agent's browser and `xdotool` calls wait.

**Architecture:** `voice.rs` runs ffmpeg, whisper-cli and Piper from the image's root-owned `/opt/zephyr/voice/` as `nobody`, on memfds, one transcription at a time. `inbox::append` hands it only the bytes it fetched in that call and marks the row `derived = "voice"` (schema v19), which taints main's conversation at dispatch. `desktop::Control` is the one lock a takeover and the vault's fill share; while it is held, root's `desktop-held` file makes the `agent-browser` and `xdotool` wrappers wait on the gate socket and exit 1. The web app signs "Take over" with the passkey, gets a one-use ticket, and `/api/vnc` joins a WebSocket to an `x11vnc -inetd` that listens nowhere.

**Tech Stack:** Rust (no new crates: `libc` for memfd, setuid and socketpair; `tokio-tungstenite` for the VNC socket; `ring` for tickets), whisper.cpp 1.9.5, a cut-down static ffmpeg n7.1, piper-tts 1.8.0, noVNC 1.7.0 `core/` embedded, x11vnc from the image, plain ES2020 JS.

**Spec:** `docs/superpowers/specs/2026-11-11-channels-voice-design.md`, sections 4.4 to 4.9 except Telegram and Signal. Its closing "Decided with the owner (2026-11-11)" section wins. Wave index: `docs/superpowers/plans/2026-11-11-wave2.md`.

## Global Constraints

Every constraint in the wave index and in `2026-11-10-wave1.md` applies (commit style with no trailers, failing test first, `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, the task's tests, then the full `cargo test` before each commit, event-waiting tests, scrubbing, root's rules, DESIGN and README in the same task). Specific to this lane:

- Branch `claude/voice-takeover`, cut from `origin/docs/wave2-specs`. C2 merges last; it does not build Telegram or Signal. Where the spec names them (`voice.ogg` on Telegram, `voice.m4a` on Signal), the code keys on the channel name so C1 needs no change here, and the tests use `discord` (Ogg Opus) and `imessage` (M4A).
- Schema **v19**: `ALTER TABLE inbox ADD COLUMN derived TEXT`. `SCHEMA_VERSION` becomes 19.
- **No new crates.** `Cargo.lock` does not change.
- The image's layout, which `voice.rs` and `deploy/Dockerfile` share:
  - `/opt/zephyr/voice/bin/ffmpeg`, `/opt/zephyr/voice/bin/whisper-cli`
  - `/opt/zephyr/voice/models/ggml-small-q5_1.bin`, `/opt/zephyr/voice/models/ggml-silero-v5.1.2.bin`
  - `/opt/zephyr/voice/py/bin/python` with `piper` installed (`python -m piper`)
  - `/opt/zephyr/voice/voices/en_US-lessac-medium.onnx` (+ `.onnx.json`), and `ko_KR-kss-medium.onnx` only with `KO_VOICE=1`
  - all root-owned, world-readable, nothing writable by the agent or `nobody`.
- Decoders and Piper run as uid and gid 65534 with no groups, `cwd /`, a cleared environment, their own process group. Limits: whisper 3 min, ffmpeg 30 s, Piper and its encode 60 s.
- Values from the spec: transcript cut at 4000 characters; voice out refused over 1500 characters; ticket 32 random bytes, 30 s, one use, bound to its session; one VNC socket; binary frames of at most 1 MiB; a held wrapper answered at hand back or after 2 minutes; control goes back after 10 min without input, 60 min in all, or 60 s after the socket closes.
- Side container: `ZEPHYR_CONTAINER=zephyr-test-c2`, attach port 20790, web port 20793, a scratch HOME, the bridge network. Never `zephyr-test`, `zephyr`, `~/.zephyr*` or the host's tailscale.
- `zephyr-policy` and `fake-rpi.py` are untouched: the desktop wait is asked by the wrappers, never by the plugin.

**Decisions this plan adds** (the spec is silent, or the task brief overrides it):

1. Tests reach the fakes through a hidden `Config::voice_dir` (default `/opt/zephyr/voice`), a directory of the image's layout holding small scripts, not through PATH: root's binaries are named by full path everywhere else, and a PATH lookup here would be the one exception.
2. The drop to uid 65534 happens only when the daemon is root (as `agent::cmd` drops only when ids are set). The fakes log their uid; the test expects 65534 as root and the test's own uid otherwise. The ignored in-image test is the one that proves 65534.
3. The audio's length is read from the WAV ffmpeg wrote (16 kHz mono s16: bytes / 32000). ffmpeg gets `-t <max_secs + 1>`, so an over-long note costs a bounded decode and no whisper run.
4. `voice.languages` with `KO_VOICE=0`: Korean is still transcribed (whisper's model is multilingual); only the spoken reply is missing. `speak` returns `None` when the voice file for the reply's language is absent.
5. The hand-back and take notes go through the router's existing gate-note path (`router::GateNote`), so they are the daemon's own text and do not taint.
6. `desktop::Control` is an `Arc` created once by the daemon and handed to the web hub, the gate server and the vault; it is not a static, since the integration tests run several gateways in one process.
7. x11vnc's two ends are one unnamed `socketpair`, set as both its stdin and stdout: `-inetd` reads and writes one socket.
8. Hidden `Config` fields, `#[serde(skip)]` like `tick`: `voice_dir`, `x11vnc` (default `/usr/bin/x11vnc`), `desktop_hold` (2 min, the gate's answer), `desktop_idle` (10 min), `desktop_cap` (60 min), `desktop_gone` (60 s).
9. The wrappers find `desktop-held` beside the gate socket (`$ZEPHYR_GATE_SOCKET`'s folder), else at `/var/lib/zephyr-public/desktop-held`. Held with no gate answering exits 1 with `browser_gate::NOT_ANSWERING`.

**Execution:** three parts run in parallel, each in its own worktree cut from the plan's commit, then are integrated in this order: voice (C2-1 to C2-5), takeover (C2-6 to C2-8), image (C2-9). C2-10 runs on the integrated branch.

## Review Focus

1. **A forged voice-note file in `inbox/`** (wave index item 4): a file the agent wrote under the stem the owner's next voice note will get is never what is transcribed, and a file with no message makes no row. Tests in C2-3.
2. **A transcript that reads as a command** (`/ok 3`, `/set approvals.preset off`): the row starts with `[`, proposal 3 stays pending, the setting is unchanged, and the conversation is tainted. Tests in C2-4.
3. **The agent reaching x11vnc, or acting while the owner holds the desktop** (wave index item 5): no listening socket, and both wrappers exit 1 without running the real binary. Tests in C2-6 and C2-7, live in C2-10.
4. **A ticket replayed, expired, or taken by another session**, and a second VNC socket: all refused. Tests in C2-7.
5. **A phone that locks mid-takeover:** control goes back by itself 60 s after the socket closes, and the held wrappers are answered. A takeover that never ends would stop the agent's browser for good. Test in C2-7.
6. **whisper or ffmpeg hanging on a crafted file:** killed at the limit, the line reads `transcription timed out`, and the next voice note is transcribed. Test in C2-2.

---

## Voice

### Task C2-1: Store v19 and `Inbound.derived`

**Files:**
- Modify: `crates/zephyr/src/store.rs` (`SCHEMA_VERSION`, `open`, `insert_inbox_as`, `InboxRow`, `inbox_with_status`, the migration test), `crates/zephyr/src/channel.rs`, every `Inbound { .. }` literal (41, in `src/` and `tests/`), `crates/zephyr/tests/router.rs` (`old_install`)

**Interfaces:**
- Produces:
  ```rust
  // channel.rs
  pub struct Inbound { /* … */ #[serde(default, skip_serializing_if = "Option::is_none")] pub derived: Option<String> }
  // store.rs
  pub struct InboxRow { /* … */ pub derived: Option<String> }   // "voice" when the text holds a transcript
  ```
- `insert_inbox_as` stores `m.derived`. `/retry`'s `copy_owner` also copies `derived`.

- [x] **Step 1: Failing tests** in `store.rs`:
  - extend `older_databases_migrate_in_order_and_keep_their_rows`: `const V19: &str = "ALTER TABLE inbox DROP COLUMN derived;"` first in the undo chain, the case `(18, "")`, and after the reopen assert a row inserted with `derived: Some("voice".into())` reads back so and one without reads `None`;
  - `a_retried_voice_row_is_still_derived`.
  - `old_install` in `tests/router.rs` drops `derived` before stamping v15.
- [x] **Step 2: Run** `cargo test -p zephyr --lib store::` → FAIL.
- [x] **Step 3: Implement** the `if version < 19` block and the field; add `derived: None` to every literal.
- [x] **Step 4: Run** `cargo test -p zephyr` → PASS.
- [x] **Step 5: Commit** `feat(store): mark an inbox row whose text holds a transcript, in schema v19`.

### Task C2-2: `voice.rs`: the transcription pipeline, as `nobody`

**Files:**
- Create: `crates/zephyr/src/voice.rs`, `crates/zephyr/tests/fixtures/voice/bin/ffmpeg`, `…/bin/whisper-cli`, `…/py/bin/python` (fake scripts)
- Modify: `crates/zephyr/src/lib.rs`, `crates/zephyr/src/config.rs` (`[voice]`, `voice_dir`), `crates/zephyr/src/egress.rs` (`ruleset`)

**Interfaces:**
- Produces:
  ```rust
  pub const DIR: &str = "/opt/zephyr/voice";
  pub const NOBODY: u32 = 65534;
  pub enum Heard { Said { lang: String, text: String }, Silence, TooLong, Failed, TimedOut }
  pub fn present(cfg: &Config) -> bool;                          // the image has voice (VOICE=1)
  pub async fn transcribe(cfg: &Config, audio: &[u8]) -> (Heard, u64); // and the audio's seconds
  pub fn line(heard: &Heard, max_secs: u64) -> String;
  // config.rs
  pub struct Voice { pub transcribe: bool, pub reply: String, pub languages: Vec<String>, pub max_secs: u64 }
  impl Config { pub fn voice(&self) -> Voice }                   // defaults: true, "mirror", ["ko","en"], 300
  ```
- `line` gives exactly:
  - `[transcript of the voice note above, by zephyr (ko), may be wrong: "TEXT"]`
  - `[voice note above: no speech heard]`
  - `[voice note above: not transcribed: longer than 5:00]`
  - `[voice note above: transcription failed]`, `[voice note above: transcription timed out]`
- `TEXT` is one line: control characters and Unicode direction marks removed, whitespace runs one space, `"` kept, cut at 4000 characters, and any `]` followed by a newline cannot occur since there is no newline.
- The fake scripts append one JSON line per run to `voice_dir/log` (their environment is cleared, so no variable can name it; they find the folder from their own path): `{"prog", "uid", "argv", "sha256"}`, the hash being of their stdin or fd 3. Tests copy the fixture folder into a temp dir first. The fake whisper prints the text found in `voice_dir/say` (default `hello from the fake`), sleeps when `voice_dir/hang` exists, and prints its language line as the real binary does (C2-9 pins that shape against 1.9.5).
- `egress::ruleset` gains `meta skuid 65534 reject` in the `output` chain, after the agent's jump.

- [x] **Step 1: Failing tests** in `voice.rs` and `egress.rs`:
  - `lines_read_as_the_spec_writes_them` (the five strings above, `300` → `5:00`);
  - `a_transcript_is_one_clean_line_cut_at_4000` (input with `\n`, `\u{202e}`, `\u{0007}` and 5000 characters);
  - `the_decoders_get_the_fetched_bytes_on_fd_3_and_a_cleared_environment` (the log's sha256 equals the input's; argv holds `/proc/self/fd/3`; no `HOME` in the fake's environment);
  - `a_hung_whisper_is_killed_and_the_next_note_is_heard` (Review Focus 6; `voice_wait` shortened to 300 ms);
  - `two_notes_are_transcribed_one_at_a_time` (the fake's start and end stamps do not overlap);
  - `a_note_over_max_secs_is_not_given_to_whisper`;
  - `a_language_outside_the_list_is_run_again_as_the_first`;
  - `the_floor_refuses_nobody` (`ruleset(1000)` contains `meta skuid 65534 reject`).
- [x] **Step 2: Run** `cargo test -p zephyr --lib voice::` → FAIL.
- [x] **Step 3: Implement.** memfds through `libc::memfd_create` with `MFD_ALLOW_SEALING`, sealed after the write; `pre_exec` does `dup2` to fd 3, `setgroups(0)`, `setgid`, `setuid` when root, and `setpgid(0, 0)`; `nice(10)`; a `tokio::sync::Semaphore` of 1; threads `min(8, nproc / 2)`. A timeout kills the process group.
- [x] **Step 4: Run** the same → PASS; then `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(voice): transcribe audio with whisper as nobody, one note at a time`.
- Deviation: the drop to `nobody` is `Command::uid`/`gid` (std also calls `setgroups(0)` when root), not hand-written calls in `pre_exec`; `pre_exec` does only the `dup2`s and `nice(10)`, and the process group is `process_group(0)`.
- Deviation: whisper-cli runs without `-np`: as far as is known that flag also drops the `auto-detected language` line, and with `-nt` only the text goes to stdout either way. C2-9 pins this against 1.9.5 (`voice::whisper_args`, `voice::detected`).
- Deviation: one more hidden field, `Config::voice_wait` (3 min, whisper's limit), which the hang test shortens; decision 8 does not list it.
- Deviation: `voice::gpu` is the device check plus "a failed run on the GPU is done again with `-ng`, as every later one is". It does not read the Vulkan device's name or spot `llvmpipe`: whisper's output for that is unknown until C2-9.
- Deviation: the fakes are one script, `bin/ffmpeg`, with `bin/whisper-cli` and `py/bin/python` as links to it; it also logs `env`, `start` and `end`, and has the markers `lang`, `secs`, `slow` and `piper_fail`.
- Deviation: `[voice] languages` takes only whisper's own codes (`voice::LANGUAGES`), checked in `Config::validate`, since each goes into argv.
- Deviation (pinned later from the image part's measurements, commit `fix(voice)`): whisper always gets `-ng` and `voice::gpu` is gone, against spec §4.4: Vulkan as `nobody` with a cleared environment is slower than the CPU (12 s against 8 s for a 9 s clip), since the NVIDIA driver recompiles shaders on every run without a writable cache. `status` and `doctor` read `voice: whisper small on CPU`.
- Deviation (same commit): ffmpeg writes the WAV to its stdout (`-c:a pcm_s16le -f wav -t N pipe:1`), which the daemon puts into the second sealed memfd; the length is read from that output. Nothing is written through `/proc/self/fd/4`, and every memfd is sealed when it is made.

### Task C2-3: `inbox::append` transcribes what it fetched

**Files:**
- Modify: `crates/zephyr/src/inbox.rs`, `DESIGN.md` §4.3 (the voice-note line), `README.md` (Voice)

**Interfaces:**
- Consumes: `voice::transcribe`, `voice::line`, `Inbound.derived`.
- `save` returns the fetched bytes with the path. When `existing` finds the file, the bytes are fetched again for the transcript only; the saved file is never read.
- An audio file (`audio/*` served or declared) is listed as
  `[attached audio: PATH (voice note, MIME, M:SS, SIZE)]` and followed by `voice::line`. `inbound.derived = Some("voice")` only when the line is a transcript.
- With `voice.transcribe` off or no voice in the image, the audio line stands alone and nothing runs.

- [x] **Step 1: Failing tests** in `inbox.rs` (Review Focus 1):
  - `a_voice_note_is_transcribed_from_the_fetched_bytes`: discord `audio/ogg`; the text is the two lines; `derived == Some("voice")`; the fake's sha256 is the stub body's;
  - `a_file_planted_under_the_notes_stem_is_not_what_is_heard`: write `inbox/discord-7-1.ogg` with other bytes first; the logged sha256 is the stub's, the stub was asked once, and the planted file is left as it was (never read as root);
  - `a_file_in_the_inbox_with_no_message_is_never_heard`: a planted `.ogg` and no `append`: the fake's log is empty;
  - `a_strangers_voice_note_is_neither_fetched_nor_heard`;
  - `an_imessage_caf_is_heard_and_a_caption_stays_first` (the row's text starts with the caption).
- [x] **Step 2: Run** `cargo test -p zephyr --lib inbox::` → FAIL.
- [x] **Step 3: Implement.**
- [x] **Step 4: Run** → PASS; `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(inbox): append a transcript of an owner's voice note, from the bytes the daemon fetched`.
- Deviation: the audio line carries `M:SS` only when ffmpeg decoded the note: with transcription off, no voice in the image, a failed decode or a note over `max_secs` (cut at `-t`, so its real length is unknown) it reads `(voice note, MIME, SIZE)`.
- Deviation: a file already saved is fetched again only when its declared type is `audio/*` (the served type is not known before a fetch); if that fetch fails the line under it is `transcription failed`.
- Deviation: one more test, `with_transcription_off_or_no_voice_the_audio_line_stands_alone`; `a_file_in_the_inbox_with_no_message_is_never_heard` sends a typed message with no file, so that `append` runs.

### Task C2-4: A transcript taints and is never a command; the `voice.*` settings

**Files:**
- Modify: `crates/zephyr/src/router.rs` (`dispatch_new`, about `:2190`), `crates/zephyr/src/settings.rs`, `crates/zephyr/src/reload.rs` if `[voice]` needs a plan, `DESIGN.md` §4.6 check 2, §4.16, §4.17
- Test: `crates/zephyr/tests/voice.rs` (new), `settings.rs` tests

**Interfaces:**
- `dispatch_new`: a row with `derived` set calls `taint_session("took in a voice note's transcript")` before it is sent (the clock is added by `taint_session`).
- Registry rows, all `Access::Propose`, not strong, `Apply::Toml`, read in place: `voice.transcribe` (bool, `true`), `voice.reply` (choice `off`, `mirror`, `always`; `mirror`), `voice.languages` (list; `ko, en`), `voice.max_secs` (int 10..900; `300`).

- [x] **Step 1: Failing tests** in `tests/voice.rs` (a `Stub` channel that pushes an `Inbound` already carrying the transcript lines and `derived`), Review Focus 2:
  - `a_spoken_ok_approves_nothing`: hold an action, send a voice row whose transcript is `/ok 1`: the proposal is still pending and the row was dispatched to the agent as text;
  - `a_spoken_set_changes_nothing` (`/set approvals.preset off`);
  - `a_voice_row_taints_main_and_a_later_typed_row_does_not_auto_save`: `session_origin` is `Tainted("took in a voice note's transcript at …")`; with `memory.auto = "owner"` the typed row's memory change waits for `/memory ok`;
  - in `settings.rs`: the four rows exist with those kinds and defaults, and the agent's `/set voice.reply always` is a proposal.
- [x] **Step 2: Run** `cargo test -p zephyr --test voice` → FAIL.
- [x] **Step 3: Implement.**
- [x] **Step 4: Run** → PASS; `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(router): treat a voice note's transcript as file-derived text that taints the conversation`.
- Deviation: `reload.rs` needs no change: a `[voice]` edit is `Plan::Live`, and `inbox::append` reads the live config. One more test, `the_owner_sets_voice_settings_and_the_agent_only_proposes`, takes `/set voice.*` through the router and the file.
- Deviation: the two "approves nothing" tests pass before the change (the row never starts with what was said, by C2-3's line format); they are kept as guards. Only the taint test failed first.

### Task C2-5: Voice out, and the `voice` lines of doctor and status

**Files:**
- Modify: `crates/zephyr/src/voice.rs`, `crates/zephyr/src/router.rs` (the settle's reply write, `:4411`), `crates/zephyr/src/store.rs` (the second outbox row), `crates/zephyr/src/main.rs` (doctor, status), `DESIGN.md` §4.3 and §4.4 item 1, `README.md`
- Test: `crates/zephyr/tests/voice.rs`

**Interfaces:**
- Produces:
  ```rust
  pub fn speakable(text: &str) -> Option<String>;   // None: a notice, a code block, or over 1500 characters once marks and URLs are gone
  pub fn format_for(channel: &str) -> (&'static str, &'static str); // ("voice.ogg","opus") for telegram and discord; ("voice.m4a","aac") otherwise
  pub async fn speak(cfg: &Config, text: &str, channel: &str) -> Option<Vec<u8>>;
  impl Store { pub fn voice_reply(&self, row: &InboxRow, path: &str) -> rusqlite::Result<()> } // client id "<client id>-v", empty text
  ```
- Voice: Korean when Hangul is at least 30% of the letters, else English; a missing voice file is `None`.
- Spoken for a reply when `voice.reply` is `mirror` and the row's `derived` is `voice`, or `always` on `telegram`, `signal`, `discord`, `imessage`. Never for `[zephyr] …`, a card, job output or `/btw`.
- It runs on a task of its own after the text row is written; the file goes to `<state>/files/<millis>-<seq>/voice.ogg` as `send_file`'s copies do.
- `doctor`: `voice: whisper small on CPU|GPU (…)`, the models and voices present, a one-second silent sample transcribed within 30 s; `voice: not in this image (built with VOICE=0)` when `DIR` is missing. `status` prints the first form.

- [x] **Step 1: Failing tests:**
  - `voice.rs`: `notices_code_and_long_replies_are_not_spoken`; `hangul_picks_the_korean_voice_and_a_missing_one_speaks_nothing`; `discord_gets_ogg_and_imessage_m4a`;
  - `tests/voice.rs`: `a_reply_to_a_voice_note_is_also_spoken` (two outbox rows, the second `…-v` with `voice.ogg`, after the text); `a_typed_message_gets_no_voice_under_mirror`; `a_failed_piper_leaves_the_text_reply_sent`;
  - an `#[ignore]` test `the_images_binaries_hear_what_piper_says`: with `DIR` present, `speak` an English sentence, `transcribe` the result, expect its words and `lang == "en"`, and the fake-free processes' uid 65534; with `tests/fixtures/voice/ko.ogg` present, expect its words in Korean.
- [x] **Step 2: Run** → FAIL.
- [x] **Step 3: Implement.**
- [x] **Step 4: Run** → PASS; `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(voice): speak the reply to a voice note with Piper, as a second attachment`.
- Deviation: the second outbox row has no inbox id: `outbox.inbox_id` is UNIQUE and is the text reply's. It has the same channel and recipient and the client id `<client id>-v`.
- Deviation: ffmpeg encodes to its stdout (`pipe:1`), not into a memfd, as the image part measured it; M4A to a pipe needs `-movflags frag_keyframe+empty_moov`.
- Deviation: `doctor`'s voice lines are warnings, like the other extras, and do not include the spec's "the floor refuses uid 65534" probe (the plan does not list it; `the_floor_refuses_nobody` checks the rule text). `doctor` also prints the uid the decoders get (`voice::uid`, `/usr/bin/id -u` run as they are), which the ignored test uses too.
- Deviation: `speak` runs on a tokio task, not a blocking one (its work is two child processes). More tests than listed: `always_speaks_a_typed_reply_but_never_a_notice_or_on_chat`, `a_reply_is_spoken_only_where_the_setting_and_the_channel_allow`, `doctor_names_what_is_there_and_hears_a_silent_sample`, `a_spoken_reply_is_a_second_row_written_once`.
- Deviation: no Korean sample is committed, so the ignored test's Korean half runs only when `tests/fixtures/voice/ko.ogg` (and its words in `ko.txt`) is there. CLAUDE.md is left to C2-9.

## Desktop takeover

### Task C2-6: `desktop::Control`, the `desktop-held` flag, and the waiting wrappers

**Files:**
- Modify: `crates/zephyr/src/desktop.rs`, `crates/zephyr/src/gate.rs` (`Conn::run`, `serve`), `crates/zephyr/src/browser_gate.rs`, `crates/zephyr/src/main.rs` (argv[0] `xdotool`), `crates/zephyr/src/vault.rs` (`fill`), the daemon's start (removes a stale flag), `DESIGN.md` §4.11

**Interfaces:**
- Produces:
  ```rust
  // desktop.rs
  pub const HELD_FILE: &str = "desktop-held";            // in drafts::public_dir, root's, 0644, the start time
  pub struct Control { /* Mutex<State>, watch::Sender<Option<Held>> */ }
  pub struct Held { pub since: i64, pub session: [u8; 32] }
  pub struct Fill(/* guard */);
  impl Control {
      pub fn new(cfg: &Config) -> Arc<Control>;           // removes a flag left by a crash
      pub fn take(&self, session: [u8; 32], now: i64) -> Result<(), &'static str>; // "another device has the desktop" | "the vault is signing in; try again in a moment"
      pub fn give(&self, now: i64) -> Option<(i64, i64)>; // (since, until) when it was held
      pub fn held(&self) -> Option<Held>;
      pub fn fill(&self) -> Option<Fill>;                 // None while the owner holds the desktop
      pub fn changes(&self) -> watch::Receiver<Option<Held>>;
  }
  // browser_gate.rs
  pub const XDOTOOL: &str = "/usr/bin/xdotool"; pub const XDOTOOL_ENV: &str = "ZEPHYR_XDOTOOL";
  pub fn xdotool_main(args: Vec<String>) -> ExitCode;
  pub fn held(env: &Env) -> Option<String>;               // Some(text): do not run, exit 1 with it
  ```
- Gate request `{"v": 1, "desktop": "wait", "session": …}`, from the agent's uid like any other: answered `{"verdict": "deny", "text": …}` at the hand back, after `cfg.desktop_hold`, or at once when nothing is held (the wrapper then runs: only `"text": ""` means go on).
  - after 2 minutes: `zephyr: the owner has the desktop (since 14:04); this command did not run. Do other work, or try again later.`
  - at the hand back: `zephyr: the owner used the desktop from 14:04 to 14:08; this command did not run. The page may have changed: take a fresh snapshot before acting.`
- `agent-browser --help` and `--version` are not held; every other command is, before the page is read. `xdotool` is held whatever its arguments.
- `vault::fill` takes `Control::fill` first and answers `not filled: the owner has the desktop; try again when they hand it back` without it.

- [x] **Step 1: Failing tests** (Review Focus 3):
  - `desktop.rs`: `take_writes_the_flag_and_give_removes_it`; `a_second_take_and_a_take_during_a_fill_are_refused`; `a_fill_during_a_takeover_is_refused`; `a_start_removes_a_stale_flag`;
  - `gate.rs` (with `serving_with`): `a_desktop_wait_is_answered_at_the_hand_back`; `a_desktop_wait_gives_up_after_the_hold_limit` (`desktop_hold` 200 ms, the "still has the desktop" text); `another_uid_gets_no_answer` already covers the peer check;
  - `browser_gate.rs`: `a_held_desktop_stops_agent_browser_and_xdotool_before_the_real_binary` (a fake real binary that logs; the log stays empty; the text is the gate's; exit 1); `with_no_flag_both_run_as_before`; `held_with_no_gate_is_refused` (`NOT_ANSWERING`).
- [x] **Step 2: Run** `cargo test -p zephyr --lib desktop:: gate:: browser_gate::` → FAIL.
- [x] **Step 3: Implement.**
- [x] **Step 4: Run** → PASS; `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(desktop): make the agent's browser and xdotool wait while the owner holds the desktop`.
- Deviation: the two files' tests and the code were written together, not test-first; the tests were run only against the finished code.
- Deviation: `Control::take` by the session that already holds the desktop succeeds and changes nothing (it needs a fresh ticket after its socket dropped, C2-7), and has a third refusal, `the desktop could not be held; zephyr doctor says why`, when the flag cannot be written: without the flag the wrappers would not wait.
- Deviation: one more test, `tests/cli.rs` `xdotool_is_the_wrapper_and_does_not_run_while_the_desktop_is_held`, runs the binary under the name `xdotool`, since the `argv[0]` dispatch is in `main.rs`, which no unit test reaches. `desktop.rs` also has `a_held_command_is_told_since_when_and_what_to_do` for the two texts.
- Deviation: `vault::agent` takes the `Control` as a parameter (the fill itself is spawned there), and `vault::fill` is unchanged.

### Task C2-7: Take over, the ticket, `/api/vnc`, x11vnc, the notes and the timeouts

**Files:**
- Create: `crates/zephyr/src/web/vnc.rs`
- Modify: `crates/zephyr/src/web/mod.rs` (`route`, `Hub`), `crates/zephyr/src/web/passkey.rs` (`Signed::Desktop`), `crates/zephyr/src/attach.rs` (`WEB_METHODS`, `zephyr.desktop`, the `desktop` op), `crates/zephyr/src/router.rs` (`Snapshot.desktop`, the notes), `crates/zephyr/src/procs.rs` (spare x11vnc), `crates/zephyr/src/main.rs` (doctor's `desktop` line), `DESIGN.md` §4.3.1, §4.3.3, §4.11, §4.20, `README.md`
- Test: `crates/zephyr/tests/web.rs`

**Interfaces:**
- Consumes: `desktop::Control`, `web::Hub`, `passkey::Challenges`, the tests' `Authenticator` and `signed_in`.
- Produces:
  ```rust
  // passkey.rs
  Signed::Desktop                                         // preimage ["zephyr-web/1","desktop","take",hex(nonce)]
  // router.rs
  pub struct DesktopState { pub owner: bool, pub since_unix: i64 }   // Snapshot.desktop: Option<DesktopState>, None without a desktop
  // web/vnc.rs
  pub struct Tickets { /* one-use, 30 s, bound to a session hash; only SHA-256 of the ticket is kept */ }
  pub fn issue(&self, session: [u8; 32]) -> String;       // base64url of 32 random bytes
  pub fn redeem(&self, ticket: &str, session: [u8; 32]) -> bool;
  pub fn ws(req: Request<Incoming>, app: &Arc<App>) -> Res;   // GET /api/vnc
  ```
- `zephyr.web_challenge {"op": "desktop", "verb": "take"}`; `zephyr.desktop {"verb": "take", challengeId, assertion}` → `{"ticket"}` or `-32602` with `approve with your passkey`, `there is no desktop`, `another device has the desktop`, or the vault's refusal; `zephyr.desktop {"verb": "give"}` unsigned. Not open to the TUI's `Conn`.
- `/api/vnc`: passes `admit`, needs the cookie, offers `Sec-WebSocket-Protocol: zephyr-vnc, zt.<ticket>` and is answered `zephyr-vnc`. One at a time (503), outside `MAX_SOCKETS`. Binary frames only, at most 1 MiB, no request cap. A text frame, the session ending, or the hand back closes it.
- x11vnc: `cfg.x11vnc` with `-inetd -once -norc -nocmds -noremote -nossl -quiet -display $ZEPHYR_DESKTOP -auth <home>/.zephyr-xauthority`, through `agent::cmd`, its stdin and stdout one socketpair, killed when the socket ends.
- Notes to main through the gate-note path: at take `[zephyr] the owner took over the desktop at 14:04; browser and desktop commands wait until they hand it back. Do other work meanwhile.`; after the hand back the "used the desktop from … to …" sentence, steered into a run or put in front of the next prompt. An automatic hand back tells the owner `[zephyr] the desktop went back to the agent after 10 min without input` (or `after 60 min`, or `after the app closed`).

- [x] **Step 1: Failing tests** in `tests/web.rs` (a fake x11vnc script that echoes its stdin, on `cfg.x11vnc`; Review Focus 3, 4, 5):
  - `take_over_needs_a_passkey_signature_and_give_does_not`;
  - `a_ticket_works_once_for_its_session_within_30_s` (no ticket, a used one, another session's, one made 31 s ago: each 401);
  - `a_second_vnc_socket_is_refused_and_a_text_frame_closes_the_first`;
  - `bytes_go_to_x11vnc_and_back` (a binary frame comes back from the fake);
  - `x11vnc_is_started_with_inetd_and_listens_nowhere` (the fake logs its argv: holds `-inetd`, no `-listen`, no `-rfbport`);
  - `a_closed_socket_gives_the_desktop_back_after_the_wait` (Review Focus 5; `desktop_gone` 200 ms), and the same for `desktop_idle` and `desktop_cap`;
  - `main_is_told_at_take_and_at_hand_back_and_stays_clean` (`session_origin` is still `Clean`);
  - `a_fill_during_a_takeover_is_refused`;
  - `snapshot_shows_who_holds_the_desktop`;
  - `every_method_the_app_calls_is_open_to_it` already exists: it must pass with `zephyr.desktop`.
- [x] **Step 2: Run** `cargo test -p zephyr --test web` → FAIL.
- [x] **Step 3: Implement.**
- [x] **Step 4: Run** → PASS; `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(web): let the owner take the desktop over with a passkey, over an x11vnc that listens nowhere`.
- Deviation: one more hidden `Config` field than decision 8 lists, `desktop` (`$ZEPHYR_DESKTOP`): the tests cannot set the variable, which would make every child start try to run Xvfb, and the takeover needs to know there is a desktop and its display.
- Deviation: the 30 s of `a_ticket_works_once_for_its_session_within_30_s` are checked in `web/vnc.rs` (`a_ticket_is_its_sessions_for_30_s_and_only_its_hash_is_kept`, with the clock passed in), not in `tests/web.rs`, which cannot age a ticket without a 31 s wait. A ticket is spent by whoever presents it.
- Deviation: the 60 s after a closed socket count only once a socket has been open; a take whose socket never opens goes back after the 10 idle minutes. Counting from the take would race the app's own first connect, and made the test's 200 ms a coin toss.
- Deviation: "no input" is no RFB KeyEvent or PointerEvent, read from the first byte of each client frame (noVNC sends one message per frame), since noVNC's screen requests would otherwise keep a forgotten tab's takeover alive. `shortcut:` in `vnc::is_input`.
- Deviation: the notes do not go through `GateNote` (decision 5), whose path drops a note for a run that is not busy and needs the session's name; the router watches `Control::changes` itself and calls the same `steer_note`. The hand-back note "in front of the next prompt" is a steer queued just before that `prompt`, which rpi (and fake-rpi) delivers as the run starts; putting it into the prompt's own text would break `untag` and `reconcile`. Its text drops "this command did not run", which is true only of a held command.
- Deviation: the idle and cap tests are named `an_idle_takeover_gives_the_desktop_back` and `a_takeover_ends_at_its_cap`, and their arithmetic is `vnc::the_desktop_goes_back_after_no_input_the_cap_or_a_closed_socket`. Added: `without_a_desktop_there_is_nothing_to_take`, the cross-signature checks in `take_over_needs_a_passkey_signature_and_give_does_not`, the TUI's `-32601` in `tests/attach.rs`, and `desktop::doctor_names_what_the_takeover_lacks`.
- Deviation: `procs.rs` needed no change: x11vnc is the daemon's own child, which `strays` already spares; its test has a row for it. doctor's `desktop` line is a warning, like the block it is in, so an image without the `xdotool` link (before C2-9) does not fail doctor.
- Deviation: README gets only the doctor line here; "Take over the desktop" comes with the screen in C2-8.

### Task C2-8: The Desktop screen and noVNC in the app

**Files:**
- Create: `crates/zephyr/assets/web/novnc/` (noVNC 1.7.0's `core/` and `vendor/pako`, unchanged, with its `LICENSE.txt`)
- Modify: `crates/zephyr/src/web/mod.rs` (`FILES`: generated from the folder by a `const` list, one row per file), `crates/zephyr/assets/web/{index.html,app.js,app.css}`, `README.md` ("Take over the desktop" replaces the hand-started x11vnc), `DESIGN.md` §4.3.3

**Interfaces:**
- Consumes: `zephyr.web_challenge` op `desktop`, `zephyr.desktop`, `/api/vnc`, `Snapshot.desktop`.
- The drawer shows Desktop while `snapshot.desktop` is not null. The screen has Take over (signed), Hand back, a keyboard button whose text field sends keys with `rfb.sendKey`, the canvas scaled to fit, and under the button: "The screen is the agent's: it sees what is shown and can read what you type there. Use the vault to sign in without giving it a password."
- noVNC is imported as a module from `/novnc/core/rfb.js`; `new RFB(el, url, {wsProtocols: ["zephyr-vnc", "zt." + ticket]})`.

- [x] **Step 1: Failing tests** in `web/mod.rs`: the existing file tests (no `innerHTML`, `eval(`, inline script) now walk every `FILES` row; `every_novnc_file_is_served_as_javascript`; `the_novnc_tree_is_the_pinned_one` (the file count and a SHA-256 over the sorted paths and bytes, written into the test from the v1.7.0 tag).
- [x] **Step 2: Run** `cargo test -p zephyr --lib web::` → FAIL.
- [x] **Step 3: Implement.** Fetch the tag with `gh api repos/novnc/noVNC/tarball/v1.7.0`.
- [x] **Step 4: Run** → PASS; `cargo test -p zephyr`.
- [x] **Step 5: Commit** `feat(web): add the Desktop screen, with noVNC 1.7.0 served by the app itself`.
- Deviation: v1.7.0's `core/` and `vendor/pako` pass the app's file tests as they are (no `innerHTML`, `eval(`, `new Function`), so nothing is exempted. But they do not work whole under the unchanged CSP, which the spec missed: `img-src 'self'` refuses the `data:` URLs noVNC draws Tight's JPEG tiles, TightPNG and the pointer's shape through. Neither the CSP nor noVNC's files change; `app.js` wraps `RFB.messages.clientEncodings` and drops those encodings (JPEG, TightPNG, the quality levels, both cursor encodings), so the server sends lossless Tight and draws the pointer itself. Test `the_app_asks_novnc_for_no_data_images` pins this to v1.7.0's files. Costs bandwidth on photos; a CSP `img-src 'self' data:` for `/` would be the other way, and is the owner's call.
- Deviation: the tree is 57 files, not 56: the tag's `LICENSE.txt` is copied with it, as the task says. The two licences are served as `text/plain`; pako's `README.md` is in the folder and not served.
- Deviation: `FILES` keeps its rows, and noVNC's scripts are a second const list, `NOVNC`, written through a macro (one row per file, the path once); `embedded` looks in both.
- Deviation: noVNC is loaded with a dynamic `import()` when the Desktop screen is first used, since `app.js` is a classic script and 400 KB should not load with every chat.
- Not verified: no browser ran this. The Rust side (ticket, socket, x11vnc's argv and socketpair) is tested with a fake x11vnc; that noVNC completes its handshake with a real x11vnc through `/api/vnc`, the look of the screen, the phone keyboard field (Hangul composition, Backspace on Android) and the absence of CSP errors are for C2-10's side container and the README's manual check.

## Image

### Task C2-9: The `voice` stage, `VOICE` and `KO_VOICE`, the `xdotool` link

**Files:**
- Modify: `deploy/Dockerfile`, `crates/zephyr/src/host/` (pass `VOICE`/`KO_VOICE` through `zephyr up --build` only if build args are already passed there; else README names the `docker build --build-arg`), `crates/zephyr/src/main.rs` (`browser_wrapper` also checks the `xdotool` link), `README.md`, `DESIGN.md` §4.4, `CLAUDE.md`
- Possibly: `crates/zephyr/src/voice.rs` and the fakes, if the real binaries' argv or output differ from what C2-2 assumed (one commit, both sides)

**Interfaces:**
- Consumes: the layout in Global Constraints.
- Stages: `voice-1` builds whisper.cpp v1.9.5 (`GGML_VULKAN=ON`; `glslc`, `libvulkan-dev` in the stage only), ffmpeg n7.1 static with the spec's demuxers, decoders, `libopus` and `aac` encoders, ogg and ipod muxers, file and pipe protocols; the piper-tts 1.8.0 venv on the final image's python; the models and the English voice, each checked against a pinned sha256. `KO_VOICE=1` adds `ko_KR-kss-medium` (CC BY-NC-SA 4.0, named in a comment and in README). `FROM scratch AS voice-0`; `FROM voice-${VOICE} AS voice-out`; the final stage `COPY --from=voice-out /opt/zephyr/voice/ /opt/zephyr/voice/` (an empty tree with `VOICE=0`).
- `ln -s /usr/local/bin/zephyr /opt/zephyr/gate/bin/xdotool`.

- [x] **Step 1: Failing test:** `every_copy_in_the_dockerfile_is_in_the_build_context` must still pass; add `the_gate_bin_holds_both_wrappers` (the Dockerfile text links `agent-browser` and `xdotool` into `/opt/zephyr/gate/bin`) beside it.
- [x] **Step 2: Run** → FAIL.
- [x] **Step 3: Implement**, then `docker build -t zephyr-dev:c2-<sha> -f deploy/Dockerfile .` and a second build with `--build-arg VOICE=0` of the final stage only as far as it shows the stage swap works.
- [x] **Step 4: Check in the image** (`docker run --rm --entrypoint`, no container kept): whisper-cli and ffmpeg run as uid 65534 with `--cap-drop ALL`; `python -m piper` speaks English; pin whisper's language line and Piper's argv in `voice.rs` and the fakes.
- [x] **Step 5: Commit** `build(image): add whisper, a cut-down ffmpeg and Piper behind VOICE and KO_VOICE, and the xdotool wrapper`.
- Deviation: the final stage copies `--from=voice-out /opt/zephyr/ /opt/zephyr/`, and `voice-0` is `FROM scratch` with `WORKDIR /opt/zephyr`, so `VOICE=0` leaves no `/opt/zephyr/voice` at all (not an empty one): `voice::present` may test the folder.
- Deviation: done in the image part only (Dockerfile, its test, docs). `main.rs` (`browser_wrapper` checking the `xdotool` link) and the pins in `voice.rs` and the fakes are left to the takeover and voice parts; the shapes they need are below.
- Deviation: whisper prints the detected language only without `-np`, on stderr: `whisper_full_with_state: auto-detected language: ko (p = 0.995411)` (absent with `-l LANG`, and for silence). The spec's argv with `-np` loses it, so `voice.rs` drops `-np` and reads that line. stdout with `-nt` is `\n` then ` TEXT` with no final newline, the same with or without `-np`; empty, exit 0, for silence (VAD finds 0 segments); exit 11 for unreadable audio, 2 for a missing file. `-f /proc/self/fd/3` works.
- Deviation: Vulkan needs a shader disk cache. With the cleared environment the NVIDIA driver compiles every pipeline on each run, and a 9.3 s clip took 11.6 to 12.7 s on the RTX 3090, slower than the CPU (7.8 to 8.4 s, `-ng -t 8`). With `__GL_SHADER_DISK_CACHE_PATH` set to a folder nobody can write, the second run took 0.92 s. `voice.rs` passes one, or uses `-ng`.
- Deviation: Piper's argv is `python -m piper -m VOICE.onnx --output-raw` (text on stdin, raw s16le mono 22050 Hz on stdout, the rate from the voice's `.onnx.json`); onnxruntime writes a telemetry warning to stderr on every run, exit 0. ffmpeg: `-f s16le -ar 22050 -ac 1 -i pipe:0` (or `/proc/self/fd/3`); WAV `-ar 16000 -ac 1 -c:a pcm_s16le -f wav pipe:1`; Ogg Opus `-c:a libopus -b:a 32k -f ogg pipe:1`; the ipod muxer refuses a pipe, so M4A is `-c:a aac -movflags +faststart -f ipod -y /proc/self/fd/4` on a seekable fd, or `-movflags frag_keyframe+empty_moov` to a pipe. The raw demuxer is `pcm_s16le`; the aac, flac, mpegaudio, opus and vorbis parsers are built too.
- Deviation: whisper-cli links libgomp and libvulkan dynamically (both in the final image); its build stage also needs `spirv-headers`.
- Deviation: no `tests/fixtures/voice/ko.ogg`. The only CC0 Korean phrase found (Lingua Libre, `보스니아 헤르체고비나`) transcribed as `오스니아 헤드체곱이다`; the sentence that transcribed exactly is CC BY-SA 4.0.
- Deviation: `zephyr up --build` passes no build args, so README names `docker build --build-arg`.
- Note: until the takeover part's `argv[0] == "xdotool"` dispatch is merged, the link runs the zephyr CLI (`xdotool key a` exits 1 reading `zephyr.toml`), so this commit must not ship without it.

### Task C2-10: Measure and check in a side container

- [x] **Step 1:** Build `zephyr-dev:c2-<sha>` from the integrated branch. Record `docker image inspect` sizes against the same tree built with `VOICE=0`, and against `ghcr.io/yansigit/zephyr:0.8.0` if it is local.
- [x] **Step 2:** In the image: English (Piper's own speech) and Korean (a Korean sample; the Korean voice fetched into a scratch folder for this check only, never into the image) transcribe to their words. Record wall time on CPU and whether Vulkan found the GPU.
- [x] **Step 3:** Run the ignored tests in the image where they apply (`the_images_binaries_hear_what_piper_says`).
- [x] **Step 4:** Side container `zephyr-test-c2` (scratch HOME, ports 20790 and 20793, bridge network):
  - a voice-note file through the inbox path (a stub file server on the bridge, a Discord-shaped inbound over the test hook or the adapter's own code path) is transcribed, and the decoders' uid is 65534;
  - during a takeover, as the agent's uid: no x11vnc listening socket in `/proc/net/tcp` or `/proc/net/unix`, connects to `127.0.0.1:5900-5999` refused, the web port on the container's address refused;
  - Take over (a software passkey over the attach protocol on `/api/ws`, as `tests/web.rs` does) → `agent-browser snapshot` as the agent waits → Hand back → it exits 1 with the hand-back text; `xdotool key a` the same.
- [x] **Step 5:** `docker rm -f zephyr-test-c2`; keep the image. Record the results below.
- [x] **Step 6: Commit** `docs(plans): record the voice and takeover lane's measurements and deviations`.

## Results and deviations

Run on 2026-11-10 on the integrated branch (image `zephyr-dev:c2-150d1c9`, kept; side container `zephyr-test-c2`, scratch HOME, attach 20790, web 20793, its own bridge; container, network and HOME removed afterwards). The live `zephyr` container, `~/.zephyr*` and the host's tailscale were not touched.

**How it was built.** The three parts ran in parallel in their own worktrees (voice C2-1 to C2-5, takeover C2-6 to C2-8, image C2-9) and were cherry-picked onto this branch in the order takeover, image, voice. The one conflict was `Config::keep_hidden` (both sides kept).

**Tests.** `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean; `cargo test`: 1211 passed, 0 failed, 20 ignored.

**Image size** (`docker image inspect`, bytes):

| image | size |
|---|---|
| `zephyr-dev:c2-150d1c9` (`VOICE=1`, `KO_VOICE=0`) | 6,226,179,903 |
| the same tree with `VOICE=0` (`c2-img-4cb4119-voice0`) | 5,391,497,964 |
| `ghcr.io/yansigit/zephyr:0.8.0` | 5,404,300,917 |

Growth: **+835 MB** (+15.5%) for voice, against the spec's estimate of about 520 MB. `/opt/zephyr/voice` is 470 MB on disk (models 183, Piper's venv 186, the English voice 61, whisper-cli 39, ffmpeg 5); the image layer is 493 MB, and this Docker's inspect size counts more than the layer. noVNC adds 0.6 MB to the binary.

**In the image, with the real binaries** (as root with the daemon's capabilities, no network):
- `the_images_binaries_hear_what_piper_says` passes: Piper's English speech, encoded as Ogg Opus and as M4A, is transcribed to its words as `en`, and the decoders' uid is 65534.
- With a Korean sample beside it (Piper's `ko_KR-kss-medium` fetched to a scratch folder, never in the image or the repo), the same test hears it as `ko` with its words.
- `a_real_voice_note_is_heard_and_a_planted_one_is_not` passes: through `inbox::append`, the transcript is of the fetched note and the file planted under its name is left as it was.
- whisper small-q5_1 on the CPU: 7.8 to 8.4 s for a 9.3 s clip (8 threads).

**In the side container:**
- `doctor`: `voice: whisper small on CPU`, the models and the English voice are there, `the decoders run as uid 65534`, `a silent sample is transcribed`, `desktop: the web app can take it over`. `state.db` is at `user_version` 19.
- The inbox test above passes there too, under the floor. As uid 65534, a direct connection and one through the proxy at 127.0.0.1:7792 both fail; the rule set holds `meta skuid 65534 reject`.
- Take over, driven by a software passkey over `/api/ws` (a Python stand-in for `tests/web.rs`'s `Authenticator`): an unsigned take is `-32602 approve with your passkey`; the signed one answers a ticket; `/api/vnc` without a ticket and with the used one is 401; with it, the real x11vnc answers `RFB 003.008` through the socket.
- During the takeover, as the agent's uid: x11vnc runs as the agent with `-inetd` and no listening socket (`/proc/net/tcp` lists only the proxy, Docker's DNS and the daemon's two ports on the container's address; `/proc/net/unix` names no VNC socket); 127.0.0.1:5900-5999 has nothing open; the container's address on 5900, 20793 and 20790 is refused. `desktop-held` is root's, 0644.
- `agent-browser snapshot` and `xdotool key a` as the agent wait; no real binary starts. At the hand back both exit 1 with `zephyr: the owner used the desktop from 05:32 to 05:32; this command did not run. The page may have changed: take a fresh snapshot before acting.`, the VNC socket closes, x11vnc is gone and the flag is removed. Afterwards `xdotool getmouselocation` and `agent-browser --version` run as before.

**Deviations of the lane as a whole** (each task's own are under the task):
- Deviation: whisper runs on the CPU only (`-ng`). As `nobody` with a cleared environment the NVIDIA driver recompiles its shaders on every run, which made Vulkan slower than the CPU (12 s against 8 s for a 9 s clip; 0.9 s once a shader cache folder is given). `voice::gpu` and the "on GPU" doctor line are not built; the spec's "on the GPU when there is one" waits for a cache folder that only `nobody` can write.
- Deviation: the spec's whisper argv is wrong for 1.9.5: `-np` also hides the detected language, so it is not passed and the language is read from stderr.
- Deviation: a bug found in review after integration, fixed with its test in `150d1c9`: Piper's sample rate was given to ffmpeg as an output option, so a spoken reply would have played at twice its speed.
- Deviation: the voice outbox row has no inbox id (`outbox.inbox_id` is UNIQUE), where the spec asks for the same one.
- Deviation: the fakes are reached through `Config::voice_dir`, not PATH (decision 1), against the task brief's "fakes on PATH".
- Deviation: the "voice note through the inbox path" check in the side container is the ignored test run inside it, not a message through a live Discord or iMessage adapter: the side container has no channel account.
- Deviation: no Korean fixture is committed (no clean CC0 clip was found), so the ignored test's Korean half runs only when `tests/fixtures/voice/ko.ogg` is put there.
- Deviation: noVNC 1.7.0 does not work whole under the unchanged CSP (`img-src 'self'` blocks its `data:` images), so `app.js` drops the encodings that need them.

**Not verified:**
- The Desktop screen in a real browser: noVNC drawing the screen, the pointer, the phone keyboard field (Hangul, Backspace on Android), and a console free of CSP errors. Only the daemon side and the RFB banner were driven. README's manual check on iOS Safari and Android Chrome stands before release.
- A voice note through a real Discord or iMessage message, and the spoken reply arriving as a voice message there.
- The take and hand-back notes reaching a real rpi run: the side container had no signed-in account, so main never ran. The tests cover it against `fake-rpi.py`.
- The GPU path, arm64, and `KO_VOICE=1` in a full image (the stage alone was built with it).
- `tests/net_floor.rs` was not extended or run for uid 65534 or `/api/vnc`; the same facts were checked by hand above.
