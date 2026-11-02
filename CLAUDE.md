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

# Ignored tests need the pinned real binary at /home/user/Projects/.always-on/rpi-0.15.0/bin/rpi
cargo test -p zephyr --test real_rpi -- --ignored
cargo test -p zephyr-policy --test rpi_load -- --ignored
cargo test -p zephyr --test attach -- --ignored   # drives real `rpi --connect` via tests/drive-connect.py

cargo install --path crates/zephyr && zephyr up --build   # build deploy/Dockerfile and run it locally
deploy/release.sh 0.2.1 | --cli                           # release from a clean main (README "Releasing")
```

`ZEPHYR_CONTAINER` / `ZEPHYR_IMAGE` point the host CLI at another container or image, to test beside a
live install. Pushing a `v*` tag publishes `ghcr.io/yansigit/zephyr:<version>` (`.github/workflows/image.yml`).

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
  rpi's `ready.supportedCommands`; `doctor` checks it. rpi is pinned to 0.15.x (`RPI_PIN` in
  main.rs, `deploy/Dockerfile`, and `rpi-plugin-sdk = "=0.15.0"` in zephyr-policy).
- `pool.rs`: `[[account]]` failover. Each account gets its own `RPI_CODING_AGENT_DIR` under
  `<home>/accounts/<name>/`; rate-limit detection reads `agent_settled.error`; cooldowns live in `state.db`.
- `auth_keeper.rs`: refreshes expiring OAuth tokens via `rpi auth refresh` subprocess (not the
  `oauth` RPC). A new token only binds after a child restart, done when idle.
- `reload.rs`: live settings. `diff` returns the least disruptive `Plan`: read-in-place, `set_model`,
  restart the child (same session), or re-exec the daemon. Config edits go through `toml_edit` to keep comments.
- `scheduler.rs` (cron jobs, run ledger: interrupted runs become `unknown`, never auto-retried;
  `execute` is the unattended-run path jobs and tasks share), `tasks.rs` (background tasks the agent
  starts with its `task` tool, DESIGN §4.12: at most 2 at once, and the report goes back to `main` as
  an inbox row), `procs.rs` (main's `exec`/`process` tools, DESIGN §4.13: commands run as the agent
  and move to the background when a message for `main` arrives; main's child runs with
  `--exclude-tools bash`; a re-exec re-adopts them by pidfd and kernel start time, never a reused
  pid; after a daemon crash, `supervise.rs` (the image's `CMD`, between tini and `zephyr run`)
  starts the daemon again and exit codes come from `.procs/<id>.exit`). `retention.rs` deletes job/task
  sessions and process logs older than 30 days. Tasks interrupted by a crash or re-exec resume in their session (`scheduler::resume`, at most
  3 times), and runs with no rpi event for `stall_limit` are aborted. Mid-run messages are steered in (`queue_message` `deliverAs: steer`,
  DESIGN §4.1),
  `memory.rs` (git-committed memory files), `checkpoint.rs` (DESIGN §4.14: snapshots of the
  projects folder, `$ZEPHYR_PROJECTS`, into the agent's shadow-git store `<home>/.checkpoints`
  before each prompt/continue, task and job, at most 10 s; `/rollback` lists, diffs and restores;
  pruned to 50 per project by `retention.rs`), `attach.rs` (rpi remote JSON-RPC shim so stock
  `rpi --connect` attaches to `main`).
- `inbox.rs`: files the owner sends on Discord or iMessage (DESIGN §4.3). The adapter saves them as
  the agent into `<home>/inbox/` before the row goes to the router and appends a
  `[attached image: <path> ...]` line per file to its text; owners come from the router's live
  config (`Channel::live`), so a stranger's files are never fetched. 10 files, 25 MB each.
- `drafts.rs` (DESIGN §4.6): skills and connectors the agent drafts under `skills/drafts/` and
  `connectors/drafts/`, shown to the owner once after each run (`scan`) and approved with
  `/skill ok` / `/mcp ok` only if unchanged since shown. Approved skills are a root-owned git repo in
  `<state>/skills`; connectors are `[[mcp]]` in `zephyr.toml`. `drafts::prepare` (called by
  `spawn_as` in place of `memory::prepare`, which it runs first) publishes them as the agent to
  `<home>/.approved/` (`mcp.json` with `${NAME}` secrets filled in) and adds `## Skills` and
  `## Connectors` to AGENTS.md. zephyr-policy's `check_mcp` lets MCP calls use only that file
  (`ZEPHYR_MCP_CONFIG`), byte-identical to what it read at load.
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
vetoes hardline commands (`policy.rs` `check`) and appends audit lines to `$ZEPHYR_AUDIT_LOG`. It fails
closed: any non-`Allow`, malformed event or panic blocks the call.

**Tests** (`crates/zephyr/tests/`) run the daemon against `fake-rpi.py`, a scripted stand-in for rpi
0.15.0 frames. Its behaviour is driven by marker files in the zephyr home (`hang`, `slow`,
`stats.json`, `oauth_fail`, ...) and it logs every command to `log.jsonl`; read its docstring before
adding a test. When rpi's frame shapes matter, update fake-rpi.py to match the real binary.

## Conventions

- Commits: conventional prefix with scope, then a plain-English sentence about the behaviour, e.g.
  `fix(model): check a model with the target account's rpi --list-models`. Authored as the user, no trailers.
- Never read tokens into structs or log them; status/doctor report only whether a credential is present.
- Providers come only from the image (`/opt/zephyr/ext/`, loaded via `extensions`); don't install rpi
  packages into `~/.zephyr`.
