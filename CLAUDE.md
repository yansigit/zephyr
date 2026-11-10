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

# Ignored tests need the pinned real binary at /home/user/Projects/.always-on/rpi-0.21.2/bin/rpi
# (or RPI_BIN=<path>; deploy/bump-rpi.sh rewrites that default path)
cargo test -p zephyr --test real_rpi -- --ignored
cargo test -p zephyr-policy --test rpi_load -- --ignored
cargo test -p zephyr --test attach -- --ignored   # drives real `rpi --connect` via tests/drive-connect.py
# Needs Docker and a side container (never the live one); the file's head says how to make it
ZEPHYR_CONTAINER=zephyr-test cargo test -p zephyr --test net_floor -- --ignored

cargo install --path crates/zephyr && zephyr up --build   # build deploy/Dockerfile and run it locally
deploy/release.sh 0.2.1 | --cli                           # release from a clean main (README "Releasing")
deploy/runner/start.sh up|down                            # self-hosted fallback runner (README "Releasing")
deploy/bump-rpi.sh [0.22.0]                               # move every rpi pin; .github/workflows/rpi-bump.yml runs it daily
```

`ZEPHYR_CONTAINER` / `ZEPHYR_IMAGE` point the host CLI at another container or image, to test beside a
live install. Pushing a `v*` tag publishes `ghcr.io/yansigit/zephyr:<version>` and `:latest` (`.github/workflows/image.yml`).

## Architecture

**One binary, two roles.** `crates/zephyr/src/main.rs` checks `host::in_container()` (true when
`ZEPHYR_IN_CONTAINER` or `ZEPHYR_CONFIG` is set, or `/.dockerenv` exists). On the host it runs the
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
  rpi's `ready.supportedCommands`; `doctor` checks it. rpi is pinned to 0.21.x (`RPI_PIN` in
  main.rs, `deploy/Dockerfile`, `rpi-plugin-sdk = "=0.21.2"` in zephyr-policy and zephyr-cursor,
  and `rpi-tui = "=0.21.2"` in zephyr); `deploy/bump-rpi.sh` moves them all, daily via
  `.github/workflows/rpi-bump.yml`, which merges the bump when the tests and real-rpi suite pass.
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
- `facts.rs` (DESIGN §4.18): private facts in `<state>/facts.toml` (root, 0600, read fresh).
  `found`/`scrub` match a value however it is written (NFKC, case, spacing, invisible characters,
  digit runs, percent and base64 forms; at least 6 digits or 4 characters); a fact's `allow` is
  compared with recipient targets only. A value is never in `Debug`, a log, an audit row, a notice
  or an error (`[fact:NAME]`); only the TUI, `zephyr facts` and the agent's `/facts add` take one.
- `activity.rs` (DESIGN §4.19): `/activity`, the record and grants views, rendered by the daemon
  from `Store::activity` (one `UNION ALL` over audit, settings_log, command proposals, runs, tasks
  and procs, keyset-paged by `store::Cursor`), every text scrubbed at read (`gate::Scrub`, then
  `settings::one_line`). The model never writes it.
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
  aes128gcm, the VAPID token) are pure functions on `ring`, tested against the RFCs' vectors.
  Two channels: `web` is the session cookie alone and weak; `passkey` is a signed decision and in
  the default `strong_channels`.
- `inbox.rs`: files the owner sends on Discord or iMessage (DESIGN §4.3). The adapter saves them as
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
  byte-identical to what it read at load.
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
reorder (a task's report may be steered into main's running turn). CI (`.github/workflows/ci.yml`)
runs fmt, clippy and the tests on Rust 1.95 and `cargo deny check advisories bans`;
`ZEPHYR_REAL_RPI=1` makes the ignored real-rpi tests fail instead of skip when the binary is missing.

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
