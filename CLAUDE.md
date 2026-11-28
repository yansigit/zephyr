# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Zephyr is an always-on personal agent gateway over upstream `rpi --mode rpc`, running in one Docker
container, reachable from a terminal chat, the stock rpi TUI, Discord and iMessage. DESIGN.md is the
spec (section numbers like "§4.9" in code comments refer to it); README.md is the user-facing
command reference. Keep both in step with behaviour changes.

## Commands

```sh
cargo build                                   # workspace: crates/zephyr, crates/zephyr-policy
cargo test                                    # all non-ignored tests; needs python3 (tests/fake-rpi.py)
cargo test -p zephyr --test router            # one integration test file
cargo test -p zephyr --test router some_name  # one test by name filter
cargo clippy --all-targets && cargo fmt
cargo deny check advisories bans               # what CI's deny job runs

# Ignored tests need the pinned real binary at /home/user/Projects/.always-on/rpi-0.31.0/bin/rpi
# (or RPI_BIN=<path>; deploy/bump-rpi.sh rewrites that default path)
cargo test -p zephyr --test real_rpi -- --ignored
cargo test -p zephyr-policy --test rpi_load -- --ignored
cargo test -p zephyr --test attach -- --ignored   # drives real `rpi --connect` via tests/drive-connect.py
# Needs Docker and a side container (never the live one); the file's head says how to make it
ZEPHYR_CONTAINER=zephyr-test cargo test -p zephyr --test net_floor -- --ignored

deploy/install.sh [--from-image|--from-source] [--no-init]   # install the CLI on a fresh machine, then zephyr init
cargo install --path crates/zephyr && zephyr up --build   # build deploy/Dockerfile and run it locally
deploy/release.sh 0.2.1 | --cli                           # release from a clean main (README "Releasing")
deploy/runner/start.sh up [N]|down                        # the owner's zephyr-local CI runners (README "CI")
cargo nextest run --workspace --profile ci   # CI's test job, as .github/ci.sh runs it
deploy/bump-rpi.sh [0.22.0]                               # move every rpi pin; .github/workflows/rpi-bump.yml runs it daily
```

`ZEPHYR_CONTAINER` / `ZEPHYR_IMAGE` point the host CLI at another container or image, to test beside a
live install; `ZEPHYR_REPO` moves the repository it pulls `<repo>:<version>` from (default
`ghcr.io/yansigit/zephyr`; at build time `ZEPHYR_BUILD_REPO`). `deploy/release.sh` unsets all of them. Pushing a `v*` tag publishes `ghcr.io/yansigit/zephyr:<version>` and `:latest` (`.github/workflows/image.yml`).
The Dockerfile's `voice-*` stages fill `/opt/zephyr/voice` (whisper-cli, ffmpeg, Piper's venv from
`deploy/voice/uv.lock`, models and voices, the layout `voice.rs` reads); build arg `VOICE=0` leaves
it out, `KO_VOICE=1` adds the CC BY-NC-SA Korean voice. `--target voice-out` builds only that tree.

## Architecture

**One binary, two roles.** `crates/zephyr/src/main.rs` checks `host::in_container()` (true when
`ZEPHYR_IN_CONTAINER` or `ZEPHYR_CONFIG` is set, or `/.dockerenv` exists together with the
daemon state dir). On the host it runs the
setup/ops CLI in `src/host/` (`init`, `up`, `login`, `accounts`, `model`, `config`, ...), which mostly
shells into the container. In the container it runs the daemon (`run`) and the commands beside it
(`status`, `doctor`, `chat`, `pause`/`resume`/`reload`, which talk to the daemon over its chat socket).
`tests/cli.rs` sets `ZEPHYR_CONFIG` to exercise the container role; `tests/host.rs` covers the host role.

**Daemon data flow** (DESIGN §4.1–4.2):
- Channels (`chat`, `discord`, `imessage`, behind the `Channel` trait / `AnyChannel` in `channel.rs`)
  write inbound messages to the SQLite inbox (`store.rs`, deduplicated by platform id) before acking.
- `router.rs` owns everything after that: owner allowlist, dispatching rows into the one shared `main`
  session as `prompt`/`queue_message` with a `[channel #id]` tag, reading `agent_settled`, writing
  the outbox (client id = Discord nonce / BlueBubbles tempGuid), budgets, `/stop` `/pause` `/model`
  `/reload`, and crash reconcile (re-send rows whose text isn't in `get_messages`).
- `rpc.rs` `Child` wraps one `rpi --mode rpc` child (JSONL). `USED_COMMANDS` must stay a subset of
  rpi's `ready.supportedCommands`; `doctor` checks it. rpi is pinned to 0.31.x (`RPI_PIN` in
  main.rs, `deploy/Dockerfile`, `rpi-plugin-sdk = "=0.31.0"` in zephyr-policy and zephyr-cursor,
  and `rpi-tui = "=0.31.0"` in zephyr); `deploy/bump-rpi.sh` moves them all. `rpi-bump.yml` proposes
  each bump as a PR (edits only, no build) and starts ci.yml on it, with its `real-rpi` check; a
  sweep fast-forwards main to that exact commit once `ci-ok` and `real-rpi` pass and the diff is a
  pure pin move (`.github/bump-check.py`). Anything else waits for a person, and a branch with a
  commit the bot did not both write and commit is never replaced or closed.
- `pool.rs`: `[[account]]` failover. Each account gets its own `RPI_CODING_AGENT_DIR` under
  `<home>/accounts/<name>/`; rate-limit detection reads `agent_settled.error`; cooldowns live in `state.db`.
- `auth_keeper.rs`: refreshes expiring OAuth tokens via `rpi auth refresh` subprocess (not the
  `oauth` RPC). A new token only binds after a child restart, done when idle.
- `reload.rs`: live settings. `diff` returns the least disruptive `Plan`: read-in-place, `set_model`,
  restart the child (same session), or re-exec the daemon. Config edits go through `toml_edit` to keep comments.
- `settings.rs` (DESIGN §4.17): the registry of settings (`SETTINGS`: kind, the agent's `Access`, `strong`,
  how a change is stricter), pure, with the texts the owner reads. The router's `set_setting` is the
  one write path for `/set`, `/model`, `/task`, `/subagent`, `/rule`, the attach methods, the panel
  and the agent's `gateway` `/set`; a loosening of a `strong` setting needs a channel in
  `[settings] strong_channels` (default `tui`, `chat`); changes land in `settings_log`, time limits
  in `setting_reverts` and are undone on the tick. A new setting is one row in the table.
- `scheduler.rs` (cron jobs, run ledger: interrupted runs become `unknown`, never auto-retried;
  `execute` is the unattended-run path jobs and tasks share), `tasks.rs` (background tasks the agent
  starts with its `task` tool, DESIGN §4.12: at most `[tasks] parallel` at once, default 2, each
  on a model it may name, or as one of the owner's `[[subagent]]`s (its prompt stored with the task;
  the model resolved in the router: the call's, the subagent's, `[tasks] model`, the account's), and
  the report goes back to `main` as an inbox row), `procs.rs` (main's `exec`/`process` tools, DESIGN §4.13: commands run as the agent
  and move to the background when a message for `main` arrives; main's child runs with
  `--exclude-tools bash`; a re-exec re-adopts them by pidfd and kernel start time, never a reused
  pid; after a daemon crash, `supervise.rs` (the image's `CMD`, between tini and `zephyr run`)
  starts the daemon again and exit codes come from `.procs/<id>.exit`; it also restarts a daemon
  whose `<state>/heartbeat`, written each router tick, is 300 s old). `/stop` and `/pause` also kill
  the agent's processes that escaped their group (`Procs::kill_strays`). `retention.rs` deletes job/task
  sessions and process logs older than 30 days, prunes state.db at 90 days, takes the daily backup
  (`<state>/backups/`, 7 kept) and expires replies pending 7 days. `host/alert.rs` is the host-side
  outage alert (`zephyr alert`, a systemd user timer posting to ntfy). Tasks interrupted by a crash or re-exec resume in their session (`scheduler::resume`, at most
  3 times), and runs with no rpi event for `stall_limit` are aborted. Mid-run messages are steered in (`queue_message` `deliverAs: steer`,
  DESIGN §4.1),
  `review.rs` (the built-in daily review, DESIGN §4.5: a `builtin = 'review'` job row kept in step
  with `review.at` by `sync`; its run sends the daemon's block, then the model's check-in only when
  there are goals), `memory.rs` (git-committed memory files; a change waits for `/memory ok` unless the router saw
  its run was the owner's alone, DESIGN §4.6: `session_origin` taint per conversation and its
  saved file's record, only `inbox.owner` rows, `Tally::main`'s clean list, `Store::writers_since`
  and strict strays (asked under the memory lock), `memory::untouched`'s allowlist of plain files
  and git setup, `clean_base`; every check fails closed; each commit's origin in `memory_commits`), `checkpoint.rs` (DESIGN §4.14: snapshots of the
  projects folder, `$ZEPHYR_PROJECTS`, into the agent's shadow-git store `<home>/.checkpoints`
  before each prompt/continue, task and job, at most 10 s; `/rollback` lists, diffs and restores;
  pruned to 50 per project by `retention.rs`), `attach.rs` (rpi remote JSON-RPC shim so stock
  `rpi --connect` attaches to `main`).
- `vault.rs` (DESIGN §4.15): owner-saved logins and tokens in `<state>/vault.toml` (root, 0600,
  read fresh on each use). Main's `vault` tool lists, requests and fills (the real `agent-browser`,
  only on the login's own origin); `exec`'s `secrets` sets tokens per command, its output scrubbed to
  `[vault:NAME]`. Grants come only from the owner (`/vault grant`, or `/ok N` on a proposal).
- The approvals gate (DESIGN §4.16): messages, posts and payments wait for the owner as far as
  `[approvals]` says. `approvals.rs` is pure: `Mode`/`FactsMode`/`Preset`, `effective` (hand-set
  class, else the preset's; an unparseable value reads as the strictest), `preset_edits` (a preset
  never loosens a hand-set class), `snapshot`/`restore_edits` for a timed preset; strictness is
  `stricter` alone, never a derived order. `gate.rs` `decide` is the one decision (pass, facts,
  unreadable control, mode, then the flood cap of 3 per run and 20 a day on calls that would wait),
  fails closed, audits each outcome and builds the card from the payload (`card`; agent and page
  text quoted, cut, scrubbed of vault and fact values). It is reached from the router for main's
  `exec` and over `<state>-public/gate.sock` (`serve`: the agent's uid only, 32 connections, 10 s
  to send) for the plugin and the browser wrapper; a hold's note is steered into the asking run
  before the plugin hears `hold`. The `ZEPHYR_SESSION` a request names is the agent's to forge, so
  no verdict may depend on it. `actions.rs` carries a held action out (`proposals` rows of
  kind `run`/`pass`/`wait`, schema v16, one id space with proposed commands): `/ok N [once]` takes
  it once in one statement, then runs the stored payload through `procs` (report `action-<id>` to
  main), stores a 30-minute one-time pass, or answers the waiting wrapper (`Waits`); `/no`,
  `/cancel`, `/held`, and the tick's due delays and expiry (24 h, a browser wait 10 min). A delay
  never goes late: pause, `/resume`, `/stop` and start put it back to `pending` with its card.
- `cards.rs` (DESIGN §4.25): virtual cards. Main's `card` tool asks for one (`request`); the gate
  reads it as a payment of kind `card` (`gate::card_request`: an https origin, at most
  `cards.max`, in `approvals.currency`) and `cards::issue` takes a `gate::Checked`, so no card is
  issued without the gate. `Issuer` is the provider trait (`AnyIssuer`: `Manual`, the owner's own
  card typed in with `zephyr card put N`; `Link`, `cards_link.rs`, Stripe's Link Agent Wallet,
  whose own approval replaces zephyr's `/ok`; `Fake` for tests, behind the hidden
  `Config::cards_fake`). A number lives only in `cards::live` (memory, keyed by the state folder)
  and `vault::scrub` turns it into `[card:#N]` in everything the daemon scrubs; it reaches the
  daemon in a chat line's `card` field (`chat::take_card` → `cards::enter`), never an inbox row.
  `cards::fill` types it only on the card's origin, through `vault::type_into`: the value goes
  to agent-browser on stdin (`batch`), never argv, in a script that checks the top document's
  origin itself; a page that moved closes the card. A field the top document lacks is looked
  for in a Stripe or Braintree card frame directly in it (`cards_frame.rs`: the daemon's own
  DevTools session from `get cdp-url`, the compiled-in `listed` origins, `judge`, and a typing
  script in an isolated world that checks the frame's origin, its parent and
  `ancestorOrigins` in the same evaluation); any other frame is never filled. While
  a number is kept, the wrapper refuses read-backs of that origin (`cards-filled`,
  `browser_gate::reads_back`). An open card covers exactly one Pay click (`Gate::card_for`,
  `Store::use_card`: `open` → `charging`). `cards::tick` runs on the router's store; issuers
  are asked off the loop and their answers (`Word`) recorded on the next tick; a card's row is
  changed only by conditional, per-column updates. `/stop`, `/pause` and a start close what is
  open and stop a fill in hand. **The payment window** (`gate::window_over`,
  `[approvals.window]`, the `payments` table): past N payments or a known amount in S seconds,
  a payment is held `strong`; one with no readable amount is held for an ordinary `/ok`. The
  look and the row are one `BEGIN IMMEDIATE` transaction (`Store::pay_within`).
- `facts.rs` (DESIGN §4.18): private facts in `<state>/facts.toml` (root, 0600, read fresh).
  `found`/`scrub` match a value however it is written (NFKC, case, spacing, invisible characters,
  digit runs, percent and base64 forms; at least 6 digits or 4 characters); a fact's `allow` is
  compared with recipient targets only. A value is never in `Debug`, a log, an audit row, a notice
  or an error (`[fact:NAME]`); only the TUI, `zephyr facts` and the agent's `/facts add` take one.
- `activity.rs` (DESIGN §4.19): `/activity`, the record and grants views, rendered by the daemon
  from `Store::activity` (one `UNION ALL` over audit, settings_log, command proposals, runs, tasks
  and procs, keyset-paged by `store::Cursor`), every text scrubbed at read (`gate::Scrub`, then
  `settings::one_line`). The model never writes it.
- `broker.rs`, `ca.rs` (DESIGN §4.21): the token broker. The agent holds a surrogate (`zsur_` +
  40 hex, HMAC-derived from `<state>/broker.key`) for each vault token and connector secret:
  `vault::exec_env` and `drafts::prepare` hand it out under the floor (`broker::on`), never the
  value in its place. `broker.rs` is pure: `parse_host` (exact names, no wildcard), `Map`
  (surrogate to value and hosts), `swap` (only credential headers: `Authorization` and the ones the owner listed; Basic;
  the target only with `url`; never a body), `scan` (a surrogate where nothing is swapped). `egress.rs` decrypts a `CONNECT` only
  to a bound `host:port` (`serve_tls`, `Decrypted`): a leaf from `ca::Authority`, the upstream
  verified against `ca::public_roots` (webpki-roots compiled in; nothing the agent can plant),
  one `Host`, HTTP/1.1 only, hop-by-hop headers, `Range` and upgrades stripped (a 101, a 206 and an
  encoded reply are refused); the reply's headers and body get each value bound to that host turned
  back into its surrogate (`Map::unswapper`, `Replacer`; `Accept-Encoding: identity` is forced
  and an encoded reply refused), and `procs::spawn` sends a command's output through
  `zephyr __scrublog` so its agent-readable log holds no vault value. `ca.rs` keeps the container-only CA in `<state>/ca/`, made and
  renewed only by `supervise` at a container start (`ca::start`), and installs it for the
  agent's tools (system bundle, `agent::CA_ENV`, the agent's NSS store, Camoufox's policies).
  Bindings come only from the owner on a strong channel (`/vault bind`, the strong rows
  `mcp.<name>.hosts` and `.secrets`; never from a connector draft, and `/mcp ok` of a draft that carries a
  `${SECRET}` needs a strong channel). `plain` is never implied (`broker::plain`): a
  `url` connector with a secret is not published until the owner settles it (rpi's own MCP
  client cannot reach a decrypted host), and the upgrade writes `secrets = "plain"` for
  those that were there (`Router::settle_remote_connectors`). A surrogate, a token and a key never reach a log, `Debug`, an audit row or a
  notice.
- `egress.rs` (DESIGN §4.20): the network floor. In bridge mode (`ZEPHYR_NETWORK=bridge`, set by
  `zephyr up` unless host.toml says `network = "host"`) the agent's uid has no route but its own
  loopback and the proxy on `127.0.0.1:7792` (`serve`, in the router's `select!`). `public` (IPv4
  denylist, IPv6 allowlist, so no IPv4-embedding form passes) and `decide` (ports 80 and 443, or
  an exact `network.allow` entry; every resolved address checked, one bad one refuses the
  request) are pure; the proxy connects only to the `SocketAddr`s `decide` returned. Every daemon
  TCP listener binds `listen_addr(..)` and is a row of `LISTENERS`, which is what `zephyr up`
  publishes on host loopback (`published`; the host reads the same keys with `published_from`). `ruleset`/`load_floor` are run by `supervise` before the first
  daemon (NET_ADMIN and SETPCAP then dropped in the daemon's `pre_exec`), and `supervise` keeps
  the direct routes of `network.allow` (`keep_allow`: an nft set of `address . port` pairs and its
  own lines of `/etc/hosts`, refreshed on a change and hourly); `verify` and the hidden
  `zephyr __netcheck` (run as the agent, before the config is read) prove it at start and in
  `doctor`. Agent processes get `agent::PROXY_ENV` through `agent::cmd`. Audit rows are session
  `net`. Tests never set `ZEPHYR_NETWORK`; `tests/net_floor.rs` (ignored) checks a real side
  container.
- `browser_gate.rs`: `agent-browser` on the agent's PATH is the zephyr binary (`argv[0]`,
  dispatched in main.rs before any config is read), linked from root's `/opt/zephyr/gate/bin`, first
  on `agent::AGENT_PATH`; the real 0.41 binary is `/opt/zephyr/libexec/agent-browser`
  (`ZEPHYR_AGENT_BROWSER`, `Config::agent_browser` for the daemon's own fill). It reads argv as
  agent-browser 0.41 does (keep `split`, `parse_key_chord` and the batch splitting in step with the
  pin), asks the real binary about the page and the control, sends clicks and Enter on send, post
  and pay controls (and controls it cannot read) to the gate, waits on the open connection, reads
  the page again after `allow` and tells the gate whether it ran (`{"ran": …}`); typing goes to the
  gate for the facts only. `doctor` checks the layout.
- `web/` (DESIGN §4.3.3): the web app. `mod.rs` is a hyper listener on 127.0.0.1 (`[web]`, port
  7793) behind `tailscale serve`, beside `attach::serve` in the router's `select!` with the same
  `attach::Handles`; `admit` checks Host, the Tailscale header (which only ever refuses, and is
  never logged) and Origin before any route, and the files of `assets/web/` are embedded (no
  inline code: a test reads them). `passkey.rs` (WebAuthn ES256) and `push.rs` (RFC 8291
  aes128gcm, the VAPID token) are pure functions on `ring`, tested against the RFCs' vectors;
  `signin.rs` holds enrolment (a one-time code from a strong channel), sign-in, the cookie, the
  lockout and `/web`. A decision made in the app is signed over the proposal's digest and its
  card's hash (`zephyr.web_challenge`), and the router compares both again on its loop before
  `/ok`. `push::spawn` posts new cards (`Router::push_cards`) and, while no web socket is open,
  replies, to the allowlisted push services only; `/api/push/decide` (a notification button)
  approves only what `web` may. The app starts only under the network floor (`web::floored`).
  A command that approves, grants, adds or undoes takes `may_approve` (false on channel `web`)
  and refuses its own verbs with `tools::NEEDS_PASSKEY`: give a new such verb the same check.
  Two channels: `web` is the session cookie alone and weak; `passkey` is a signed decision and in
  the default `strong_channels`.
- `voice.rs` (DESIGN §4.3): an owner's voice note is transcribed by whisper.cpp and a reply to it
  spoken by Piper, both from the image's root-owned `/opt/zephyr/voice` (`Config::voice_dir` in
  tests, where `tests/fixtures/voice` holds the fakes), as uid 65534 on sealed memfds, one note at
  a time, on the CPU. `inbox::append` hands it only the bytes it fetched in that call, never a file
  saved in `inbox/`, and marks the row `derived = "voice"` (schema v19), which taints main's
  conversation at dispatch; the transcript line starts with `[`, so speech is never a command. The
  real binaries' argv and output shapes are in `decode_args`, `whisper_args`, `detected`,
  `piper_args`, `encode_args`, `server_args`, `PIPER_LINES` and `ask_ear` (the multipart request
  to whisper-server and the reading of its reply) alone: change them with the fakes. A local
  phone call's warm whisper-server and Piper (`voice::Call`, DESIGN §4.26) run as uid 65533
  (`zephyr-call`, `CALL_UID`) with a turn and a sweep of their own, so a voice note never waits
  on a call.
- Desktop takeover (DESIGN §4.3.3, §4.11): `desktop::Control` is the one lock the owner's takeover
  and the vault's fill share; a take writes root's `<state>-public/desktop-held`, and the
  `agent-browser` and `xdotool` wrappers (`browser_gate::held`, `xdotool` dispatched on `argv[0]`
  like `agent-browser`) then wait on the gate socket (`{"desktop": "wait"}`) and exit 1 without
  running. The hold the daemon publishes is the truth, the file only the hint to ask. A wrapper
  runs the real binary as its child (never `exec`) under a shared `flock` on root's
  `<state>-public/desktop.lock` (`browser_gate::place`); `Control::drain` takes the exclusive
  side before a ticket is issued (hidden `Config::desktop_drain`, 15 s, then it kills the
  agent's processes that hold the lock) and keeps it until the hand back. `web/vnc.rs`: the passkey-signed `zephyr.desktop` take answers a one-use ticket,
  `/api/vnc` joins a WebSocket to an `x11vnc -inetd` on an unnamed socketpair (it listens
  nowhere, and runs as the agent), and `watchdog` gives the desktop back after 10 min idle, 60 min
  or 60 s after the socket closed (hidden `Config::desktop_*`). `Router::desktop_changed` steers
  the take and hand-back notes into main, which do not taint.
- `calls/` (DESIGN §4.26): phone calls. `mod.rs` is pure: `check` (Canadian local numbers only,
  toll-free only with `calls.toll_free`, the CRTC's hours in the recipient's zones, `en`/`fr`/`ko`,
  no emergency or healthcare call (`fold` first), the owner's contact details), the daemon's `opening`/`closing` lines, `said` (a reply's
  sentences, `[press 1]`, `[hang up]`) and the energy `Endpointer`. `telnyx.rs` is the one
  `Telephony`: Call Control from the daemon's own client (the key read per request from
  `vault.toml`'s `[[service]]`, never in a child's env or `mcp.json`) and `verify` (ed25519 over
  `timestamp|stream_url` or `timestamp|body`, 300 s, each signature once); what Telnyx's docs
  leave open is marked `ASSUMPTION(live)`. `live.rs` is the listener (`calls.port`: only
  `/calls/<token>` and `/calls/events`, only under the floor) and one task per call: the
  `--no-tools` child `call-<id>`, the opening line at `call.answered`, speech by `calls.speech`
  (`provider`, the default: Telnyx's `transcription_start` and `speak`, text only; `local`:
  whisper in and Piper out through `voice::Call` (one warm whisper/Piper pair per call, a one-shot run if it dies) over the media socket), each
  sentence checked with `facts::found` as it will be spoken (`voice::speakable` first), the cap
  absolute, the call's session file removed at its end, `call_lines`, and the report row `call-<id>` with `derived = 'call'`. The `call` tool is
  main's alone and always held (`gate::Call::phone`, `ActionKind::Call`, schema v21); `/ok` starts
  it through `actions::Ctx::calls`. `tests/fake-telnyx.py` is Telnyx and the funnel: keep its
  frames and signatures in step with `telnyx.rs`.
- `inbox.rs`: files the owner sends on Discord, iMessage, Telegram or Signal (DESIGN §4.3). The adapter saves them as
  the agent into `<home>/inbox/` before the row goes to the router and appends a
  `[attached image: <path> ...]` line per file to its text; owners come from the router's live
  config (`Channel::live`), so a stranger's files are never fetched. 10 files, 25 MB each.
- `drafts.rs` (DESIGN §4.6): skills and connectors the agent drafts under `skills/drafts/` and
  `connectors/drafts/`, shown to the owner once after each run (`scan`) and approved with
  `/skill ok` / `/mcp ok` only if unchanged since shown. Approved skills are a root-owned git repo in
  `<state>/skills`; connectors are `[[mcp]]` in `zephyr.toml`. `drafts::prepare` (called by
  `spawn_as` in place of `memory::prepare`, which it runs first) publishes the skills as the agent
  to `<home>/.approved/skills`, and as root `mcp.json` (`${NAME}` secrets filled in, never
  `ZEPHYR_*` or a channel's) to `<state>-public/` and AGENTS.md (with `## Skills` and
  `## Connectors`) to root's `/AGENTS.md` in the container, both readable but not writable by the
  agent. zephyr-policy's `check_mcp` lets MCP calls use only that file (`ZEPHYR_MCP_CONFIG`),
  byte-identical to what it read at load. `drafts::seed` (DESIGN §4.23), once per start, commits
  the image's starter skills (`deploy/skills/` → `/opt/zephyr/skills`, `Config::starter_skills`)
  into `<state>/skills`, guided by `<state>/skills-seeded.json`, never again once the owner removed one.
- `catalog.rs` (DESIGN §4.22, pure): the connector catalog, `deploy/connectors/NAME.toml` →
  `/opt/zephyr/connectors` (`Config::catalog_dir`), each an `[[mcp]]` table with its secrets, hosts
  and classes preset; `zephyr mcp add --catalog` (host, reads the image's copy with `docker exec cat`)
  and `/mcp catalog add` (strong channel, `drafts::command`) write it. Pin each entry's package
  version; never let an entry set `secrets`.
- The agent's tools (DESIGN §4.11): the browser, Camoufox and the X desktop are CLIs in the image,
  run through `bash`. `desktop.rs` keeps Xvfb up when `ZEPHYR_DESKTOP` is set. `send_file`,
  `schedule`, `gateway`, `task` and `recall` are registered by zephyr-policy (`tools.rs`). The first
  two are collected from the event stream in `rpc::Tally` and carried out at settle by `tools.rs`
  here; `gateway` and `task` are answered mid-turn through a file, and `recall` reads the session
  files itself (`ZEPHYR_SESSIONS_DIR`).

Child restarts never happen mid-turn: auth refresh, failover and reload all wait for the settle.
`/undo`, `/retry` and `/compact` (router, DESIGN §4.1) need the child idle too: `navigate` moves the
session leaf, `compact` is a run of its own that settles `compacted`, and undone rows get inbox status
`undone`. `rpc::Tally` also runs the repeated-failure guard (DESIGN §4.4) for main (`cap_run`) and
for tasks and jobs (`scheduler::follow`).

**`crates/zephyr-policy`** is a cdylib rpi plugin loaded with `--extension`. Its `ToolCall` handler
vetoes hardline commands (`policy.rs` `check`). It fails closed: any non-`Allow`, malformed event or
panic blocks the call. `policy::gated` (messages, posts, payments and their targets) and
`browser_class` (the English and Korean button lexicon) are shared with the daemon; for a gated or
`outbound` `bash` command and every `mcp_call` the handler asks the gate socket (`gate_client`: 5 s,
a socket served by root, or by the program's owner outside the image) and runs the call only on
`allow`. The audit of tool calls is state.db's `audit` table, written by the daemon from the event
stream (`Router::tapped`, `scheduler::follow`); `$ZEPHYR_AUDIT_LOG` is the plugin's optional log,
off in production.

**Tests** (`crates/zephyr/tests/`) run the daemon against `fake-rpi.py`, a scripted stand-in for rpi
0.18.0 frames. Its behaviour is driven by marker files in the zephyr home (`hang`, `slow`, `wedge`,
`refresh_hang`, `tool_sleep`, `stray`, `stats.json`, `oauth_fail`, ...) and it logs every command to
`log.jsonl`; read its docstring before adding a test. It writes rpi 0.18's real session layout
(`--<cwd>--/<stamp>_<id>.jsonl` with `.lock` and `.progress.jsonl`; read sessions in tests through
`session_file` / `session_messages`), refuses a second run while one is active, and gives tool calls
ids unique across children. It also plays the plugin's gate client: with `$ZEPHYR_GATE_SOCKET` set,
every `bash` and `mcp_call` sends the gate its request line and waits up to 5 s (checking the
socket's uid as the plugin does), answering what needs no run meanwhile, and runs the call only on
`allow`; `[mcp SERVER TOOL {json}]` in a prompt makes one `mcp_call` with those arguments. It
mirrors `gateway_kind` and the gate's protocol: change them in the same commit as the Rust. When
rpi's frame shapes matter, update fake-rpi.py to match the real binary. `Config::tick` (the
router's 5 s housekeeping tick) is a hidden field like `stall_limit`; tests that wait on it set
200 ms. Wait on events, not fixed sleeps, and don't assume the order of replies a slow machine can
reorder (a task's report may be steered into main's running turn). CI (`.github/workflows/ci.yml`,
README "CI") runs two checks on Rust 1.95 (and `real-rpi`, below), each compiling once: `lint`
(shellcheck, fmt, `cargo deny check advisories bans`, clippy) and `test`, the whole suite in one
`cargo nextest` run (each
test in its own process, so no test may rely on another's process-wide state, such as a port
counter). `.github/ci.sh plan` places them by repo variable `ZEPHYR_CI` (`both`|`local`|`hosted`;
under `both` everything runs on the owner's `zephyr-local` runners but the nightly run's `test`,
on GitHub's; once GitHub refuses a hosted job, nothing goes there for the rest of that UTC month,
until `gh workflow run ci.yml -f hosted=true` starts one);
`ci.sh skip` runs none for Markdown outside `.github/`, `crates/` and `deploy/`, or for main's
merge of a PR whose head passed `ci-ok` on the same tree. A job on `zephyr-local` copies main's
last build from the read-only volume `zephyr-ci-cache`, which only `deploy/runner/start.sh
warm` on the host writes, from main's commit. `rescue` reruns on `zephyr-local` what GitHub
refused to start, and `ci-ok` is the one check to gate merges on;
`ZEPHYR_REAL_RPI=1` makes the ignored real-rpi tests fail instead of skip when the binary is missing.
A third check, `real-rpi`, runs those ignored tests against the pinned rpi release on zephyr-local
(x86_64) when `ci.sh` finds the pin is what is tested: the nightly run, an `rpi-bump/*` branch, a
commit that moves `RPI_VERSION`, or `gh workflow run ci.yml -f real_rpi=true`. `rescue` never reruns
it past a failure.
`tests/host.rs` runs rpi-bump.yml's own steps, read from the workflow's text (`workflow_steps`,
`run_job`), against a bare repository, a fake `gh` over JSON fixtures and a fake release
(`rpi_bump_proposes_one_commit_and_merges_only_a_tested_pin_move`): keep the steps in the layout
it reads, and give a new rule of `propose` or `merge` a case there.
`tests/cards_link.rs` runs the Link issuer against a stub of Link's API; its ignored
`a_real_link_card_for_one_dollar` is the owner's spike (`ZEPHYR_LINK_LIVE=1`, after `zephyr card link`).

## Conventions

- Commits: conventional prefix with scope, then a plain-English sentence about the behaviour, e.g.
  `fix(model): check a model with the target account's rpi --list-models`. Authored as the user, no trailers.
- Never read tokens into structs or log them; status/doctor report only whether a credential is present.
- Root in the container never runs anything the agent could have placed: no agent-writable directory
  on root's PATH (agent processes get `agent::AGENT_PATH` through `agent::cmd`), root's own binaries
  by full path (`/usr/bin/git`, `/usr/local/bin/zephyr`), and root's git always with
  `core.hooksPath=/dev/null`, `core.fsmonitor=false` and no system or global config. Files root
  publishes for the agent go in a root-owned 0755 folder, never into the agent's home.
- Providers come only from the image (`/opt/zephyr/ext/`, loaded via `extensions`); don't install rpi
  packages into `~/.zephyr`.
