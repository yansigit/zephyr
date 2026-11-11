# Channels and voice: Telegram, Signal, local voice, desktop takeover

Status: design drafted 2026-11-10; awaiting the owner's review.
Sub-project **C** of the Muse-parity roadmap (Wave 2 in `~/.claude/plans/i-want-you-to-jaunty-fountain.md`).
It needs W (the web app, DESIGN §4.3.3, shipped in 0.8.0) for the desktop takeover, and A2 (the
network floor, §4.20) for the claim that the agent cannot reach what it adds. Schema v19 (A3 needs none).
The owner chose Telegram, Signal and local voice. Phone calls and hosted speech services are out.

## 1. Why

Muse reaches its owner on WhatsApp and by voice, and lets the owner take the browser over while the
agent waits (`reports/Muse agent audit.md`, part C). Zephyr has Discord, iMessage (which needs a Mac),
the TUI and the web app. A voice note is saved as a file and nothing transcribes it (§4.3). Today the
only way to see the desktop is a hand-started x11vnc that does not stop the agent (README, "Watching it").

The audit sets the rules this spec follows:
- Identify the owner by a platform's stable user id, never a display name (Agents of Chaos, arXiv 2602.20021).
- Telegram and Signal are weak channels, like Discord.
- A spoken "yes" approves nothing. It is the chat-confirm mode that arXiv 2610.21063 found every
  scripted attack beats.
- Speech settings belong to root's config.
- Owner and agent never steer one screen at once.

## 2. Requirements

- **Telegram:** a bot in private chats only, over Bot API long polling with `reqwest`; no new crate.
- **Signal:** signal-cli in the image, run by the daemon, on a number of zephyr's own.
- **Owners:** on both platforms, the owner is learned by a one-time code issued on a strong channel.
  A stranger who messages the bot gets no reply, no fetched file and no transcription.
- **Voice in:** voice notes from an owner on any channel are transcribed locally with whisper.cpp,
  in Korean or English, on the GPU when there is one. The text is appended to the row.
- **Voice out:** Piper speaks a reply as an audio file through the existing attachment path,
  when the owner wrote in voice or the setting asks.
- **Desktop takeover:** noVNC in the web app, behind a passkey-signed "Take over". While the owner
  holds the desktop, the agent's `agent-browser` and `xdotool` calls wait. "Hand back" resumes them.
- **Access:** nothing added here is reachable by the agent's uid. That covers signal-cli's keys,
  the transcriber, the voice settings and x11vnc.
- **Build args** leave voice or Signal out of the image.

### Non-goals

- Telegram groups, channels, inline mode, webhooks (they need a public HTTPS endpoint) and
  Telegram formatting. Replies go as plain text.
- Signal groups, stories, reactions and editing our own messages.
- Live voice or calls, wake words, and voice approval of anything.
- A view-only desktop mode, the desktop from the TUI, and clipboard sync beyond typing text in.
- Signal on arm64. signal-cli bundles libsignal's native library for x86_64 Linux only.

## 3. What changes in DESIGN.md

- **§4.3 Channels:** new Telegram and Signal bullets (§4.1–4.3 below).
  - The voice-note line ("nothing transcribes it") becomes §4.4's.
  - `inbox::File` gets a local source.
- **§4.3.1:** `Snapshot.desktop`; `zephyr.desktop` and the `desktop` op of `zephyr.web_challenge`.
- **§4.3.3:** the Desktop screen, `GET /api/vnc` and the embedded noVNC files.
- **§4.4 item 1:** signal-cli is a root child; the decoders run as `nobody`.
- **§4.6 Provenance, check 2:** a row whose text carries a transcript taints (`inbox.derived`, v19).
- **§4.11 Desktop:** takeover, the `xdotool` wrapper and the `desktop-held` flag.
- **§4.16:** Telegram and Signal are weak. Speech never approves.
- **§4.17:** the `voice.*` rows. `settings::CHANNELS` gains `telegram` and `signal`.
- **§4.20:**
  - Telegram's and signal-cli's connections are the daemon's own, made directly.
  - The rule set refuses uid 65534.
  - x11vnc listens nowhere.
- **README:**
  - `zephyr channel add telegram|signal` and `/pair`.
  - Voice.
  - "Take over the desktop" replaces the hand-started x11vnc as the way to sign the agent in.

## 4. Design

### 4.1 Telegram (`crates/zephyr/src/telegram.rs`, new; `AnyChannel::Telegram`)

```toml
[telegram]
token_env = "ZEPHYR_TELEGRAM_TOKEN"   # secrets.env, as discord.token_env
# progress = true                     # as [discord]
# progress_after_secs = 20

[owners]
telegram = ["123456789"]              # Telegram user ids (from.id), written by /pair
```

**The token** goes in `secrets.env` (root 0600) under `token_env`, the same as Discord's.
- Like every secrets.env variable, it is removed from children's environments.
- `drafts.rs` adds `telegram.token_env` to the secrets a connector may never name, so no
  connector's `${NAME}` can put it into `mcp.json`.
- It sits in every API URL (`/bot<token>/…`), so every error goes through `without_url()`, as
  `inbox::fetch` already does, and no URL is ever logged.
- A hidden `Config::telegram_api` (default `https://api.telegram.org`) points tests at a stub.

**Polling.** `run` calls `deleteWebhook` once. It then loops
`getUpdates {offset, timeout: 50, allowed_updates: ["message", "my_chat_member"]}` on a client with
a 60 s timeout.
- The offset advances past a batch only on the next call, after every row of the batch went to `tx`.
  Telegram keeps unconfirmed updates for 24 h, so a crash before then re-delivers them, and the
  inbox's platform-id dedupe drops what was already stored. That leaves the same small window as
  iMessage's queued webhook (§4.3).
- A 409 (another client is reading this bot: a second zephyr, a side container on the same token)
  is logged once and retried after 30 s. A side container needs a bot of its own.

**What is taken.**
- Only `message` with `chat.type == "private"` and `from.id == chat.id`, from a user who is not a bot.
- The row is `platform_msg_id = "<chat.id>-<message_id>"`, `sender = from.id`. A display name or
  `@username` is never the identity: it appears only in the pairing notice.
- A bare `/start`, which Telegram sends when a chat opens, is dropped.
- **Groups are refused twice.** `channel add` tells the owner to set `/setjoingroups` to Disable in
  @BotFather. If the bot is added anyway, a `my_chat_member` update for a group, supergroup or
  channel gets `leaveChat`. Group messages are never rows.
- **Edits and deletions are not seen.** `edited_message` is not subscribed, and the Bot API sends no
  delete events in private chats. A message already taken stays taken. README: send it again.

**Files** come in through `inbox::append`:
- `photo` (the largest size), `document`, `audio`, `voice`, `video` and `video_note`, each with its
  `file_size`.
- `getFile` resolves `file_path` only for an owner (a new `inbox::owner(&live, &inbound)`, the check
  `append` already makes), so a stranger's file costs no call. The download is
  `GET {api}/file/bot<token>/<file_path>`.
- The Bot API serves at most 20 MB, so larger files are listed `too large, not downloaded`.

**Sending.**
- **Chunks:** `sendMessage {chat_id, text}` with no `parse_mode`. Markdown shows as typed:
  `shortcut:` until a converter to Telegram entities is worth it. Text is cut by `discord::chunks`,
  moved to `channel::chunks` and shared, at 4096 UTF-16 units, which is Telegram's own count.
- **Files:** `send_file` goes as `sendDocument` (multipart). A file named `voice.ogg` goes as
  `sendVoice` (§4.5). Either comes after the text.
- **Retries:** Telegram has no client nonce, so §4.2's "retry while the platform dedupes" cannot
  hold. The adapter keeps, in memory, how many chunks of each `client_id` got a 200, and a retry
  starts after them.
  - The retry window is 5 minutes, as Discord's.
  - A timeout after the request went out may still send a chunk twice, and a restart resends the
    whole reply. That is a lost reply traded for a rare duplicate.
  - A 429 is retried after its `retry_after`.
- **Activity:** `sendChatAction typing` every 4 s (it lasts 5 s), and the progress note exactly as
  Discord's (`discord::Progress`, `progress_text`): posted after `progress_after_secs`, edited
  (`editMessageText`) at most every 5 s, deleted (`deleteMessage`) on `Stop` or before the reply.

### 4.2 Signal (`crates/zephyr/src/signal.rs`, new; `AnyChannel::Signal`)

```toml
[signal]
account = "+821012345678"   # zephyr's own number, never the owner's; not a secret

[owners]
signal = ["7f3c…-…"]        # the owner's ACI (Signal's account UUID), written by /pair
```

**Who runs it: root, as a child of the daemon.** The adapter's `run` spawns
`/opt/zephyr/signal/bin/signal-cli --data-dir /var/lib/zephyr/signal -a ACCOUNT jsonRpc`.
- It speaks JSON-RPC on stdin and stdout, one object per line, as an rpi child does, so **nothing
  listens**: no socket, no port.
- Messages arrive as `receive` notifications. The adapter sends `send`, `sendTyping` and, for the
  doctor, `listAccounts`.
- The child is in the daemon's process group, so it dies with the daemon. When it exits, it is
  restarted with a backoff from 5 s to 5 min, and the channel reads `reconnecting`.
- Its environment is cleared to `PATH`, `JAVA_HOME` and `JAVA_TOOL_OPTIONS=-Xmx256m -XX:+UseSerialGC`.
- **Why root:** the keys must be out of the agent's reach, and root's `<state>` is the one place in
  the container that already is (§4.4).
  - The agent cannot run signal-cli with them, and cannot read the conversation's keys or the
    account's password.
  - signal-cli's connections are the daemon's own, so they go directly under the floor (§4.20).
- **The cost:** a JVM that parses traffic from anyone who messages the number runs as root.
  - It holds the daemon's SETUID, SETGID and KILL, and it can read `state.db`, `secrets.env` and
    `vault.toml`.
  - A uid of its own would fence that off. Open question 4 asks whether to pay for it.

**Keys.** `/var/lib/zephyr/signal/` (root, 0700) holds the account's identity keys, password and
session store.
- It is not in the daily backup: restoring old session keys next to a live account desynchronizes
  it, and the cure for a lost folder is to register or link again.
- `zephyr channel remove signal` runs `unregister` for a registered account (`deleteLocalAccountData`
  for a linked one), then deletes the folder.

**Register or link: register a number of zephyr's own.** `zephyr channel add signal` runs
`docker exec -it` as root into `zephyr signal-setup`, which drives signal-cli interactively. It offers:
- **register** (recommended):
  - The owner gives a number zephyr will own: a second SIM or eSIM, or a VoIP or landline number
    with `--voice`.
  - It pastes the `signalcaptcha://` token from signalcaptchas.org, then types the SMS or call code
    (`register --captcha`, then `verify`).
  - Last comes `updateProfile --given-name zephyr`.
- **link:** for a number whose Signal account lives on a spare phone. It prints the
  `sgnl://linkdevice` URI as a terminal QR (`link -n zephyr`), which that phone scans.
- **Never the owner's own account.** A device linked to it receives every conversation the owner
  has, and its keys speak as the owner to all of their contacts.
  - The prompt says so in those words.
  - The adapter takes only `dataMessage` envelopes from another account and drops every
    `syncMessage`. So a link to the owner's own account yields a channel that hears nothing:
    a mistake fails closed, rather than leaking.

**What is taken.**
- A `dataMessage` with no `groupInfo` and no `groupV2`, from `sourceUuid`.
- The row is `platform_msg_id = "<sourceUuid>-<timestamp>"` (Signal's own message identity) and
  `sender = sourceUuid`. A phone number may be hidden and can change; the ACI is stable.
- Typing, receipts, stories, edits (`editMessage`) and remote deletes are ignored.
- No read receipts are sent. signal-cli's delivery receipts stay, so a stranger learns that the
  number is on Signal and nothing more.
- signal-cli downloads attachments itself, into its data dir. For those,
  `inbox::File::src = Src::Path(..)`: read as root, at most 25 MB, copied into `<home>/inbox` as the
  agent (`agent::fs`), then removed from the data dir.
- **Durability gap** (`shortcut:`): signal-cli acknowledges a message to the server when it reads
  it, not when zephyr commits it. A message in the pipe when the daemon dies is lost. The window is
  the row's file copy and transcription (§4.4), and README names it.

**Sending.**
- `send {recipient: [ACI], message, attachments: ["/var/lib/zephyr/files/<id>/<name>"]}` reads
  root's copy directly.
- Chunks are 2000 UTF-16 units, and retries follow Telegram's rule.
- Activity is `sendTyping` every 10 s, and `sendTyping {stop: true}` on `Stop`. There is no progress
  note (`shortcut:`, Signal's edit-a-message is possible later).
- **Safety numbers:** signal-cli keeps its default `on-first-use`. When an owner's identity key
  changes (a new phone), sends to them fail. The owner is told once on their other channels:
  `[zephyr] the Signal safety number of 7f3c… changed; compare it, then run zephyr channel signal trust 7f3c…`.

### 4.3 Owners: the pairing code (`crates/zephyr/src/pairing.rs`, new, pure)

The owner rarely knows their Telegram id or ACI, and a lookup bot run by someone else is the wrong
thing to trust. So the id comes from the message itself, on the strength of a code that only a
strong channel saw:

1. **The code.** `/pair telegram|signal` is taken only from a channel in `strong_channels` other
   than `passkey` (`tui` or `chat` by default), as `/web enrol` is.
   - `zephyr channel add` asks for it over the chat socket at its end and prints the instruction.
   - The reply: `[zephyr] send /pair ABCDE-FGHJK to the bot within 10 min; it works once`.
   - The code is 6 random bytes in base32, and only its SHA-256 is kept (`meta` key
     `pair_<channel>`, with the expiry). A new code replaces the old one.
   - `gateway` refuses `/pair`, and the reply goes only to the asking channel, so the agent never
     sees the code.
2. **The claim.** In `Router::inbound`, before the owner check drops a stranger's row, a row on
   `telegram` or `signal` whose whole text is `/pair CODE` is checked against the hash in constant
   time.
   - The row is stored `dropped` with its text cut to `/pair`, so the code is never in `state.db`.
   - On a match, the meta row is taken in one statement (`Store::take_meta`, as enrolment does), and
     the sender id is appended to `owners.<channel>` through `toml_edit`. The owner list is read
     live (`Channel::live`), so no restart is needed.
   - The new owner is answered `[zephyr] paired: you are an owner of this zephyr`.
   - Every owner channel is told
     `[zephyr] Telegram user 123456789 ("Aiden", @aiden) is now an owner; /unpair telegram 123456789 removes them`.
     That notice is how the owner learns if someone else used the code.
3. **Wrong codes** get no reply, so there is no oracle. Five in 10 minutes close pairing for
   15 minutes, and the owner is told once, as the web app's sign-in lockout does.
4. `/unpair CHANNEL ID` is taken from any owner channel, since it only takes away.

A stranger's other messages are dropped silently, as on Discord (§4.2 step 2).

### 4.4 Voice in (`crates/zephyr/src/voice.rs`, new)

**What is transcribed.** An owner's attached file whose served or declared type is `audio/*`. That
covers Telegram's `voice` and `audio`, Discord's voice messages (`audio/ogg`), iMessage's `.caf`,
and Signal's `audio/aac`. It must also be at most `voice.max_secs` long, while `voice.transcribe`
is on.
- `inbox::append` transcribes only **bytes it fetched itself in this call**. It never reads the
  saved file back: `<home>/inbox` is the agent's to write.
- When `save` finds the file already saved (a backfill after a crash), it fetches the file again for
  the transcript.
- A stranger's file is never fetched (DESIGN §4.3), so it is never transcribed.

**The line** goes under the file's own line, flattened to one line. Control and direction marks are
removed, and the text is cut at 4000 characters:

```
[attached audio: /zephyr/inbox/telegram-55-8812-1.ogg (voice note, audio/ogg, 0:12, 18 KB)]
[transcript of the voice note above, by zephyr (ko), may be wrong: "내일 아침 9시 회의 알려줘"]
```

- Other outcomes read: `no speech heard`, `not transcribed: longer than 5:00`,
  `transcription failed`, and `transcription timed out`.
- The row's text starts with `[` or with the owner's typed caption, never with the transcript. So
  **speech can never be read as a command**: a voice note saying "slash ok 3" approves nothing.
  Approvals stay typed (§4.16).

**Where it runs: as `nobody`, outside the agent's reach.**
- The pipeline runs from the adapter's task, by full path from the image's root-owned
  `/opt/zephyr/voice/`:
  1. The fetched bytes go in a sealed memfd.
  2. `ffmpeg -i /proc/self/fd/3 -ar 16000 -ac 1 -f wav` writes into a second memfd.
  3. `whisper-cli -m …/ggml-small-q5_1.bin --vad -vm …/ggml-silero-v5.1.2.bin -l LANG -nt -np -t N -f /proc/self/fd/3`
     produces the text on stdout.
- **Each process:**
  - runs as uid and gid 65534, with no groups. The `setuid` drops every capability.
  - has `cwd /`, a cleared environment and its own process group.
  - is killed at 3 minutes (ffmpeg at 30 s).
- **Not as root:** a decoder is a parser fed by files people send. As `nobody` an exploit reaches
  no file of root's or of the agent's.
- **Not as the agent:** there it could be ptraced, and its text rewritten before the daemon reads it.
- `egress::ruleset` gains `meta skuid 65534 reject` beside the agent's jump, so the decoders have no
  network under the floor. Under `network = "host"` the agent has the whole network anyway, so
  `nobody`'s reach adds nothing it lacks.
- **One transcription at a time** (a semaphore), with `N = min(8, nproc / 2)` threads at nice 10,
  so the browser and the agent stay responsive. Other rows of that channel queue behind it, which
  keeps a caption after its voice note.

**Models and languages.**
- whisper.cpp 1.9.5 (MIT), built with `GGML_VULKAN=ON`. The image already has `libvulkan1`, and
  `zephyr up` already passes the NVIDIA GPU with `graphics` capability, which carries Vulkan.
- The model is `ggml-small-q5_1.bin` (190 MB), multilingual. `tiny` and `base` are poor at Korean.
- Silero VAD drops silence, which is where whisper invents text ("시청해 주셔서 감사합니다").
- `voice.languages` (default `ko, en`): whisper detects the language. A detection outside the list
  is run again as the list's first entry, since short Korean notes are sometimes taken for Japanese.
  With one entry, detection is skipped.

**Cost on this host** (Xeon E5-2680 v4, 28 threads, RTX 3090), measured 2026-11-10 with whisper.cpp
1.9.5 on CPU, 8 threads, 55 s of English speech:

| model | wall | CPU | RSS |
|---|---|---|---|
| small-q5_1 | 10.5 s | 80 s | 508 MB |
| large-v3-turbo-q5_0 | 35.9 s | 282 s | 847 MB |

- So a one-minute voice note takes about 11 s on CPU with small. The Vulkan build was not measured
  here (no `glslc` on the host). The plan measures it in its first step and records it in DESIGN.
- **GPU detection** (`voice::gpu`, once per daemon start):
  - It looks for a GPU when `/dev/nvidia0` exists or a `/dev/dri/renderD*` is open to `nobody`.
  - The first run is taken as the test. If whisper names no Vulkan device, or names `llvmpipe`, or
    fails, the daemon uses `-ng` (CPU) from then on.
  - `status` and `doctor` print `voice: whisper small on GPU (NVIDIA GeForce RTX 3090, Vulkan)` or
    `on CPU`.

**Provenance: a transcript is file-derived, not the owner's text.**
- The platform authenticates the account that sent the audio, not the voice in it.
- A voice note can hold anyone's speech: a forwarded note, a room, a speaker playing an injection.
- Audio can also be crafted to transcribe as chosen text, and whisper makes text up on noise.
- That is the same reason a `read` of an `inbox/` image taints (§4.6 check 3). The difference is
  that the transcript is inline, so no tool call shows it.
- Hence, schema v19 (`ALTER TABLE inbox ADD COLUMN derived TEXT`):
  - `Inbound` gains `derived: Option<String>`, set to `voice` by `inbox::append` when it adds a
    transcript. `insert_inbox_as` stores it.
  - `inbox.owner` keeps its meaning: an owner sent the row on a channel. Routing, replies and
    `/retry` are unchanged.
  - `dispatch_new` taints main's conversation for a row with `derived` set, before sending it, with
    `took in a voice note's transcript at 14:02`, beside the `!row.owner` case (`router.rs:2190`).
- **The implications:**
  - Under `memory.auto owner`, a conversation that took a voice note no longer saves memory at once
    until `/new`. Its memory changes wait for `/memory ok`.
  - A typed message in the same conversation is still the owner's, but taint is per conversation
    and never cleared (§4.6 check 1).
  - Jobs and tasks are unaffected: they are never owner-origin.
  - Open question 1 offers the alternative.

### 4.5 Voice out (Piper)

- **When:**
  - `voice.reply` is `mirror` (default), `always` or `off`. With `mirror`, a reply to a row whose
    `derived` is `voice` gets a voice note. With `always`, every agent reply on `telegram`,
    `signal`, `discord` and `imessage` does.
  - The reply text is always sent too, so the voice is in addition. It is never sent for:
    - `[zephyr] …` notices, approval cards, job output or `/btw`. A card is read, never heard
      (audit: a voice channel cannot show a hidden Bcc).
    - A reply with a code block.
    - A reply over 1500 characters once markdown marks and URLs are removed (about two minutes of
      speech).
- **How:** after the text row is written at the settle, `voice::speak` runs on a blocking task of its
  own, so the router never waits.
  1. Piper (`piper-tts` 1.8.0, GPL-3.0, in its own root-owned venv `/opt/zephyr/voice/py`) reads the
     text on stdin and writes raw PCM to stdout.
  2. ffmpeg encodes it into a memfd. Telegram and Discord get Ogg Opus at 32 kbit/s (`voice.ogg`).
     Signal and iMessage get AAC in M4A (`voice.m4a`).
  3. Both processes run as `nobody`, as above, killed at 60 s.
  4. The file goes to `<state>/files/<id>/` and a **second outbox row** is written. It has the same
     inbox id and recipient, client id `<client id>-v`, empty text, and that `attachment` (schema
     v5's path).
- So the voice never delays or blocks the text. A crash between the two loses only the voice, and
  retention removes the copy as for `send_file`.
- **Voice choice:** `ko_KR-kss-medium` when Hangul is at least 30% of the letters, else
  `en_US-lessac-medium` (63 MB each). The Korean voice's licence is CC BY-NC-SA 4.0 (open question 3).
- **Size:** at most about 120 s, so about 0.5 MB, far under every channel's limit.

### 4.6 Desktop takeover

**In the app.** The drawer gets a Desktop screen while the daemon has a desktop (`Snapshot.desktop`
is `null`, or `{"owner": bool, "since_unix"}`).
- **Take over is signed with the passkey.** The desktop holds the agent's signed-in browser, which
  is worth more than a Discord session, the yardstick for the cookie (§4.3.3).
  - `zephyr.web_challenge {"op": "desktop", "verb": "take"}` has the preimage
    `["zephyr-web/1", "desktop", "take", <nonce>]`.
  - `zephyr.desktop {"verb": "take", challengeId, assertion}` answers `{"ticket"}`, or refuses:
    `there is no desktop`, `another device has the desktop`, or
    `the vault is signing in to SITE; try again in a moment`.
- **Hand back is unsigned:** `zephyr.desktop {"verb": "give"}`.
- `zephyr.desktop` joins `attach::WEB_METHODS`. The TUI's `Conn` does not get it.

**noVNC is served by the web app itself.**
- noVNC 1.7.0's `core/` and `vendor/pako` (MPL-2.0 and MIT, 56 files, 581 KB) are embedded under
  `assets/web/novnc/`, pinned by the tag's tree.
- They are served at `GET /novnc/…` like the app's other files, under the same CSP and the same file
  tests: no `innerHTML`, `eval(` or inline script, which v1.7.0's core passes as it is.
- The CSP does not change. On a desktop browser, noVNC's `data:` cursor is blocked and the plain
  arrow shows. A phone uses noVNC's canvas cursor (`core/util/cursor.js` `useFallback`).
- The screen scales to fit. A keyboard button focuses a text field whose input goes out as keys
  (`rfb.sendKey`).
- **The socket:** `GET /api/vnc` is a WebSocket upgrade that passes `web::admit` and needs the
  session cookie.
  - It offers `Sec-WebSocket-Protocol: zephyr-vnc, zt.<ticket>`. The ticket is 32 random bytes,
    lives 30 s, works once, and is bound to the session that asked.
  - There is one VNC socket at a time, outside the four chat sockets (`MAX_SOCKETS`).
  - Frames are binary only, at most 1 MiB, with none of `/api/ws`'s 60-a-minute cap, since a pointer
    drag sends hundreds.
  - It is closed by a text frame, the session ending (`Hub::revoke`), or the hand back.
- **Reach:**
  - The web listener binds the container's own address, which the agent's uid is refused (§4.20).
  - Under `network = "host"` the web app does not start, so there is no takeover.
  - So noVNC is never on a port the agent can reach.

**x11vnc listens nowhere.**
- For each takeover, `web::vnc` spawns `/usr/bin/x11vnc -inetd -once -norc -nocmds -noremote -nossl
  -quiet -display :99 -auth /zephyr/.zephyr-xauthority` and joins the socket's frames to its stdin
  and stdout.
- With `-inetd` there is no listening socket at all: no TCP port, no loopback, no unix socket. The
  agent has nothing to connect to, and the agent's loopback, which the floor leaves open to it,
  holds no VNC server.
- **It runs as the agent** (`agent::cmd`), not as root. x11vnc is an X client of Xvfb, which is the
  agent's process. Root speaking X to a server the agent controls would hand the agent a parser
  inside a root process.
- `Procs::stray` spares x11vnc as the daemon's child, as it does the desktop.
- **The owner should know:** the screen is the agent's. It sees what is shown and can read keys
  typed there. The vault (§4.15) is the way to sign in without giving the agent a password. The
  Desktop screen says this under the button.

**What "pause" means.**
1. **Taking control.** The daemon takes `desktop::Control`, a lock the vault's `fill` also takes,
   so a fill and a takeover never overlap. It writes `/var/lib/zephyr-public/desktop-held` (root's,
   0644, holding the start time). Then it starts x11vnc.
2. **Wrappers.** `agent-browser` (the existing wrapper, `browser_gate.rs`) and `xdotool` check that
   file first.
   - `xdotool` is a new wrapper: `/opt/zephyr/gate/bin/xdotool`, a link to zephyr dispatched on
     `argv[0]` in main.rs, with the real binary at `/usr/bin/xdotool`.
   - With the file absent they behave as today. With it present, they send the gate socket
     `{"desktop": "wait"}` and wait on the open connection.
   - `gate::serve` answers when control comes back, or after 2 minutes. That is well under the
     10-minute `stall_limit`.
   - **The command never runs.** The wrapper exits 1 with the gate's words. A click planned on the
     page as it was is not made on the page as the owner left it.
   - Every one of the agent's browser and desktop commands is held, whether it is from main, a
     task or a job. `import` screenshots are not held: they read and do not act.
3. **Not held, as the gate's documented non-goals (§4.16):** the real binaries run by full path,
   Camoufox scripts, and a program of the agent's own that speaks X or CDP. Pause stops the
   agent's tools from racing the owner. It is not a wall against a hostile agent, which could act
   on its own screen anyway.
4. **In flight:** a command that passed the check just before the file appeared finishes, within
   seconds.

**What the agent is told.**
- **At take:** if main is in a run, a note is steered in:
  `[zephyr] the owner took over the desktop at 14:04; browser and desktop commands wait until they hand it back. Do other work meanwhile.`
- **A held command after 2 minutes:** `zephyr: the owner has the desktop (since 14:04); this
  command did not run. Do other work, or try again later.`
- **A held command at the hand back:** `zephyr: the owner used the desktop from 14:04 to 14:08;
  this command did not run. The page may have changed: take a fresh snapshot before acting.`
- **After the hand back:** the same sentence goes to main as a steered note if a run is going, else
  in front of the next prompt.
- These are the daemon's own notes, so they do not taint (§4.6 check 2).

**Timeouts.** Control goes back by itself:
- after 10 minutes with no input frame from the owner;
- after 60 minutes in all (take over again to go on);
- 60 s after the VNC socket closes without a new take (a phone that switched apps);
- at any daemon start, which removes a `desktop-held` left by a crash. The held wrappers' sockets
  closed with the old daemon, so they have already exited 1.

On an automatic hand back, the owner is told `[zephyr] the desktop went back to the agent after 10 min without input`.
These are constants (`shortcut:`), with no registry rows until the owner asks for one.

### 4.7 Registry rows and schema v19

| key | kind | default | agent | strong |
|---|---|---|---|---|
| `voice.transcribe` | bool | `true` | propose | no |
| `voice.reply` | choice `off`, `mirror`, `always` | `mirror` | propose | no |
| `voice.languages` | list of `ko`, `en` and whisper's codes | `ko, en` | propose | no |
| `voice.max_secs` | int 10..900 | `300` | propose | no |

- The rows live in `[voice]` in `zephyr.toml`, which is root's, so the agent proposes and never
  writes (audit: speech settings belong to root's config).
- None are `strong`: none can send, spend or loosen a check.
- `settings::CHANNELS` gains `telegram` and `signal`, so the owner may list them in
  `strong_channels`. Neither is listed by default.
- `[telegram]` and `[signal]` are config tables like `[discord]`. A change to either re-execs the
  daemon (`reload::diff`).
- **Schema v19** is the one column `inbox.derived` (§4.4). The pairing codes use `meta`; takeover
  state is in memory.

### 4.8 The image (`deploy/Dockerfile`)

New stages, each pinned by sha256 like the others:
- **`voice`:**
  - whisper.cpp 1.9.5 with Vulkan (`glslc` and `libvulkan-dev` in the build stage only).
  - ffmpeg n7.1, built `--disable-everything` with only: the ogg, mov, matroska, mp3, wav, flac,
    aac, caf and amr demuxers; their decoders; the `libopus` and `aac` encoders; the ogg and ipod
    muxers; and the file and pipe protocols. Statically linked.
  - The piper-tts 1.8.0 venv, the two voices, and the two whisper models.
- **`signal`:**
  - signal-cli 0.14.10's JVM build. The GraalVM native build is 408 MB and experimental, and its
    `link` and `register` broke in recent releases (signal-cli #2150, #2154).
  - A JRE 25 made by `jlink` from Temurin with the modules `jdeps --print-module-deps` lists for
    its jars.

| part | size | how |
|---|---|---|
| whisper small-q5_1 + Silero VAD | 191 MB | Hugging Face listing |
| whisper.cpp with Vulkan | ~50 MB | estimate; CPU-only build measured 3 MB |
| ffmpeg, cut down | ~5 MB | estimate |
| piper-tts venv (onnxruntime, numpy, espeak-ng data) | ~150 MB | estimate from wheel sizes (34 + 24 MB packed) |
| Piper voices ko_KR-kss, en_US-lessac (medium) | 126 MB | Hugging Face listing |
| **voice** | **~520 MB** | |
| signal-cli 0.14.10 (JVM) | ~130 MB | 126 MB tarball of jars |
| jlinked JRE 25 | ~70 MB | estimate |
| **signal** | **~200 MB** | |
| noVNC core | 0.6 MB | in the binary |

**Growth:** about 720 MB, from 5.4 GB (0.8.0) to about 6.1 GB (+13%). Large-v3-turbo would add
574 MB (open question 2). The plan records the measured sizes.

**Build args** `VOICE=1` and `SIGNAL=1`:
- `0` swaps the stage for an empty one (`FROM scratch AS voice-0`; `FROM voice-${VOICE} AS voice-out`).
- On arm64, `SIGNAL` builds nothing.
- At runtime, a missing `/opt/zephyr/voice` or `/opt/zephyr/signal` turns the feature off.
  `doctor` reads `voice: not in this image (built with VOICE=0)`, and `zephyr channel add signal`
  refuses with the same words.

### 4.9 doctor and status

New lines:
- `telegram`: the token is present and `getMe` answers with the bot's name.
- `signal`:
  - the child is running, and `listAccounts` names `account`;
  - the data dir is root's 0700.
- `voice`:
  - the models and voices are there;
  - a one-second silent sample transcribes as `nobody` within 30 s, with GPU or CPU named;
  - the floor refuses uid 65534.
- `desktop`:
  - x11vnc is there;
  - `/var/lib/zephyr-public/desktop-held` is absent or matches a live takeover;
  - `/opt/zephyr/gate/bin/xdotool` is the root-owned link.

`Snapshot.channels` lists `telegram` and `signal`.

## Open questions for the owner

1. **Is a voice note's transcript your text?**
   - (a) **File-derived.** It taints the conversation, so `memory.auto owner` stops saving at once
     until `/new` (§4.4).
   - (b) **Your text** when the platform marks it a voice note recorded in the app and not
     forwarded. Voice then works with auto-saved memory. The cost is that audio played near your
     phone, or crafted audio, writes your memory.
   - *Recommended: (a).* The platform proves the account, not the speaker.
2. **Whisper model.**
   - (a) small-q5_1 only: 190 MB, about 11 s per minute on this CPU.
   - (b) Also large-v3-turbo-q5_0 (+574 MB), used only when a GPU is found. Better Korean, but
     36 s per minute on this CPU, so never used there.
   - *Recommended: (a) now.* Move to (b) if small's Korean disappoints in a week of use.
3. **The Korean voice** `ko_KR-kss` is CC BY-NC-SA 4.0: personal use only.
   - (a) Ship it in the private image.
   - (b) Korean replies stay text-only.
   - (c) Fetch it at build only when a `KO_VOICE=1` arg is set, with the licence named.
   - *Recommended: (c).* It costs one build arg, and E's self-hosting guide inherits the right default.
4. **signal-cli's uid.**
   - (a) Root, as specified.
   - (b) A uid of its own (`zephyr-signal`), with its data in a separate host folder that `zephyr up`
     creates and chowns in a throwaway container, as it does for the daemon dir. The daemon reads
     attachments through `setfsuid`.
   - *Recommended: (b), as a follow-up after (a) ships.* It keeps a signal-cli exploit away from
     `state.db`, `secrets.env` and the vault, for about a day of work.
5. **Which number will zephyr's Signal account use?**
   - (a) A second SIM or eSIM.
   - (b) A VoIP or landline number, verified by voice call.
   - (c) An account on a spare phone, linked.
   - *Recommended: (a) or (b) with register.* With (c) the spare phone is the account's primary
     device and has to be kept working.

## Tests

**Telegram** (`tests/telegram.rs`, a stub Bot API on `Config::telegram_api`, inbox stubs as in
`inbox::tests`):
- **A stranger messages the bot.** Their text, a photo and a voice note:
  - the row is `dropped`;
  - no `sendMessage`, `sendChatAction` or `getFile` reaches the stub;
  - no file is in `inbox/`, and the fake whisper is never started.
- A group message and `/start` are not rows. A `my_chat_member` for a group gets `leaveChat`.
- **Pairing:**
  - `/pair` from `chat` returns a code, and `/pair CODE` from the stranger adds `from.id` to
    `owners.telegram`;
  - the notice reaches the other owner channels;
  - the code works once and expires at 10 min, and the row's text holds no code;
  - five wrong codes close pairing, and `gateway "/pair telegram"` is refused;
  - a display name equal to the owner's is never an owner.
- Chunks fit 4096 UTF-16 units, and a retry after a failed second chunk resends only it. A 409
  backs off. The token never appears in a log line or error.
- **Offsets:** a batch's offset is confirmed only on the next call, and a restart re-reads
  unconfirmed updates once (dedupe).

**Signal** (`tests/signal.rs`, `tests/fake-signal-cli.py` speaking JSON-RPC on stdio, on a hidden
`Config::signal_cli`):
- A stranger is dropped with no `sendTyping`.
- `syncMessage` from the account itself is ignored. This is the "linked to the owner's own account"
  case.
- Group messages are ignored, and pairing works as for Telegram, keyed by `sourceUuid`.
- An attachment is copied from the data dir and removed there.
- The child restarts with backoff after an exit.
- The data dir is never under `<home>`, and `agent::cmd` cannot list it (as the agent's uid,
  `ls /var/lib/zephyr/signal` fails).

**Voice** (`voice.rs` unit tests, plus `tests/voice.rs` with fake ffmpeg, whisper and piper scripts
that log their uid, argv and an input hash):
- **A forged voice-note file.**
  - The agent writes `inbox/telegram-55-8812-1.ogg` before the owner's real voice note with that id
    arrives: the transcript is of the fetched bytes (the hash matches the stub's, not the agent's
    file).
  - A planted file found by `existing` is fetched again for the transcript.
  - A file the agent drops in `inbox/` with no message makes no row and no transcript.
- **A transcript is not a command.** One reading `/ok 3` or `/set approvals.preset off` leaves
  proposal 3 pending and the setting unchanged.
- **Taint:** a row with `derived = 'voice'` taints main with
  `took in a voice note's transcript at …`, and a later typed row in that conversation does not
  auto-save memory.
- The decoders run as uid 65534. The floor's rule set text holds `meta skuid 65534 reject`, and the
  ignored `tests/net_floor.rs` checks it live.
- The over-`max_secs` and timeout lines appear. Two voice notes transcribe one at a time and keep
  their order.
- **Voice out:**
  - `mirror` writes a second outbox row with `voice.ogg` on Telegram and `voice.m4a` on Signal;
  - nothing is spoken for notices, cards, a code block or a long reply;
  - a TTS failure leaves the text reply sent.
- Ignored real test: the image's binaries transcribe a recorded Korean and an English sample to the
  expected words, and on the GPU when present.

**Desktop takeover** (`web` tests on the listener, `browser_gate`, `desktop.rs`, and the ignored
`tests/net_floor.rs`):
- **The agent tries to reach x11vnc.** During a takeover, as the agent's uid:
  - no listening socket belongs to x11vnc (`/proc/net/tcp`, `/proc/net/unix`);
  - connects to `127.0.0.1:5900-5999` and to the container's address on 5900 are refused;
  - `/api/vnc` on the container's address is refused by the floor.
- **The ticket:**
  - `/api/vnc` without a ticket, with a used one, with one 31 s old, or with another session's is
    refused;
  - a second VNC socket is refused;
  - a text frame closes it.
- **Signing:** take without a passkey assertion is `-32602 approve with your passkey`, and give is
  taken unsigned.
- **Holding:**
  - with `desktop-held` present, the `agent-browser` and `xdotool` wrappers wait on the gate and
    exit 1 with the hand-back text without running the real binary (the fake logs nothing);
  - after 2 minutes (shortened in tests) they exit with the "still has the desktop" text;
  - with the file absent they run as before.
- **Vault:** `vault::fill` during a takeover is refused, and take during a fill is refused.
- **Timeouts:** an idle socket, the 60-minute cap and a closed socket each give control back.
- **Restart:** a daemon start removes a stale `desktop-held`.
- **Notes:** main gets the take and hand-back notes, steered or in front of the next prompt, and
  they do not taint.
- The noVNC files pass the app's file tests, and every method `app.js` calls, `zephyr.desktop`
  among them, is in `attach::WEB_METHODS`.
- Manual, in README's release check: take over from iOS Safari and Android Chrome, type into the
  browser, hand back.
