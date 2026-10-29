# Zephyr: an always-on agent on upstream pi-rust

Status: approved 2026-10-28. Research inputs: `/home/user/Projects/.always-on/research/{openclaw,hermes-and-peers,practices-and-channels,pi-rust-integration}.md`.

Decisions made: iMessage through a Mac running BlueBubbles; one `main` session shared by all channels; Zephyr runs in one Docker container (§4.4); repo `yansigit/zephyr`, private. Wherever this file says `rpi-gateway`, `gateway` or `gw-*`, read `zephyr`.

## 1. Goals and constraints

- One personal agent that runs 24/7 on this Linux machine. You can reach it from a terminal UI, Discord and iMessage.
- It runs tools on its own: no per-call human approval. Safety comes from hard limits instead: a sandbox, a blocklist, budgets and a kill switch.
- **Upstream stays untouched.** The gateway lives in its own repo. It uses pi-rust only through the released `rpi` binary and published crates. Syncing upstream means bumping one version pin, with no merge.
- It survives crashes, reboots, provider errors and token expiry without losing a message or running a job twice silently.

Out of scope for v1: other chat channels, multiple users, a web UI, vector memory, provider pools.

## 2. What the research says (short)

| Lesson | Source | What we take |
|---|---|---|
| One gateway process owns all state, under a lock, bound to loopback | OpenClaw, Hermes, nanobot | Same. A single daemon with a lock on its state dir, local socket only. |
| The gateway was the attack surface: one-click takeover (CVE-2026-25253), malicious skills, sandbox off by default | OpenClaw | No web UI or HTTP listener in v1. Sandbox **on** by default. No third-party skills installed automatically. |
| Deny-by-default access with sender allowlists and pairing codes | Hermes, OpenClaw | Owner-only allowlist per channel. Unknown senders are dropped and logged. |
| A hardline command blocklist that even full autonomy can't override | Hermes | A blocklist in a tool-call policy plugin, checked inside the sandbox too. |
| Scheduled jobs run in a fresh session, with a run ledger. Interrupted runs become `unknown` and are never retried automatically | Hermes, nanobot | Same. |
| Memory as plain files, kept in git, loaded as a frozen snapshot per session | Hermes, nanobot | Same. Files under `~/agent/memory`, committed after each change. |
| iMessage needs a Mac. No supported pure-Linux route exists | all three reports | A BlueBubbles server on a Mac, reached over Tailscale. This is the only part that needs extra hardware. |
| Durable messaging uses an inbox deduplicated by platform message id, an outbox with client ids, and at-least-once delivery | practices | An SQLite inbox and outbox (WAL mode, `synchronous=FULL`). |
| systemd user service with `Type=notify`, a watchdog, `Restart=on-failure`, and linger | practices | Same. Linger is already on. The "degraded" user manager is only the desktop portal and notification units, so it's harmless. |

## 3. Integration choice: the daemon spawns `rpi --mode rpc` children

Upstream analysis (`research/pi-rust-integration.md` §7) compared three options:

1. **Spawn `rpi --mode rpc`, one child process per session. Chosen.** The contract is the versioned JSONL protocol: 29 commands, a `ready` frame listing supported commands, `agent_settled`, `session_entry_committed`, and `resume_pending`.
   - rpi keeps owning session files, compaction and crash repair.
   - A crashed child takes down only its own session.
   - An upgrade only replaces the binary.
2. Embed the crates as a library. This gives the most control, but every 0.x bump means migrating to a changed Rust API, and matching the CLI's behaviour needs the unstable `rpi_cli::session::build`. It stays a fallback.
3. An rpi plugin as the daemon. A plugin lives inside one rpi process, so this isn't viable. It is used only as a **sidecar** (§5.4).

**How syncing works:** `gateway.toml` pins `rpi = "0.12.x"`. To upgrade, run `cargo install rpi-cli@<new>` into a versioned path. Then run `rpi-gateway doctor`, which spawns one test child and checks `ready.supportedCommands` against the commands we use. Then switch the symlink. A failed doctor run leaves the old binary in place.

**Not the fork.** The gateway runs on upstream `rpi`, never on `yansigit/pi-rust`.

## 4. Architecture

```
            ┌──────────────────────── rpi-gateway (systemd --user, Type=notify) ─────────────────────────┐
 TUI ───────┤ attach server (unix socket / 127.0.0.1)                                                   │
 Discord ───┤ discord adapter (serenity, DM only)      ┌──────────┐     ┌──────────────────────────┐     │
 iMessage ──┤ imessage adapter (BlueBubbles webhook+REST)│  Router  │────▶│ SessionManager           │     │
            │                                          │ +policy  │     │  child per session:      │     │
            │ Scheduler (cron, ledger) ───────────────▶│ +budget  │     │  bwrap → rpi --mode rpc  │     │
            │ AuthKeeper (refresh timer) ─────────────▶└──────────┘     │   --extension gw-policy  │     │
            │                                               ▲           └────────────┬─────────────┘     │
            │ Store: SQLite (inbox, outbox, jobs, runs, audit, budget)   events ◀──────┘                   │
            └───────────────────────────────────────────────────────────────────────────────────────────┘
 Files: ~/agent/{gateway.toml, state.db, memory/ (git), workspace/, sessions/ (rpi JSONL)}
```

### 4.1 Sessions

- **`main`** is one long-lived session shared by all three channels. It is your assistant, so the context continues whichever channel you use. Each inbound message is prefixed with its channel, e.g. `[discord] …`. The reply goes back to the channel the message came from.
- **`job-<id>-<run>`** is a fresh session for each scheduled run, as Hermes and nanobot do. It is deleted after 30 days.
- **Ordering:** one prompt at a time per child (`maxConcurrentPrompts: 1`).
  - Messages that arrive while the agent is busy go through RPC `queue_message`. This is rpi's own follow-up queue, so we don't build our own.
  - A `/stop` from any channel sends `abort`.

### 4.2 Message flow and durability

1. **Inbound.** The adapter writes `inbox(platform, platform_msg_id UNIQUE, sender, text, status='new')` and commits before acknowledging the platform. A duplicate platform id is ignored.
2. **Policy check.** The router rejects any sender not on the owner allowlist; the message is logged and gets no reply. Otherwise it marks the row `dispatched` and sends `prompt` (or `queue_message`) with a client tag stored in the row.
3. **Reply.** On `agent_settled`, the router takes the final assistant text of that turn. It writes `outbox(inbox_id, channel, text, client_id, status='pending')` first, then sends.
   - The client id is Discord's `nonce` with `enforce_nonce`, or the BlueBubbles `tempGuid`, so the platform drops a retried duplicate.
   - Sends are retried only while the platform still dedupes the client id: 5 min on Discord, 1 h elsewhere. Then the row becomes `failed` and the failure is surfaced on the TUI. Client ids are at most 20 chars (Discord's nonce limit is 25).
4. **After a crash:**
   - **Gateway crash.** On start, `dispatched` rows whose text is already in the session (checked with `get_messages`) are not sent again. Rows whose text is missing are re-sent once.
   - **Child crash.** Respawn it with `--session-id`. If `get_messages` reports `pendingResume` (rpi 0.12 does not put it in `get_state`), call `resume_pending`; rpi allows at most 3 attempts. After that, the gateway tells you in the originating channel.
   - **Power loss.** rpi doesn't fsync its session log, so the last few entries can be lost. The inbox survives, because SQLite does fsync, and step 4's check re-sends anything missing. Optional upstream PR: fsync on session appends.

### 4.3 Channels

- **TUI.** The goal is that stock `rpi --connect 127.0.0.1:<port>` attaches with no upstream change.
  - The gateway implements rpi's small remote JSON-RPC: `authenticate`, `start_session`, `subscribe`, `send`, `stop_session`.
  - `start_session` returns `main`.
  - `subscribe` replays history from `get_messages`, then streams live events.
  - The client's `stop_session` only detaches.
  - Unverified: these details come from reading the code, not from a test run. Fallback: a ~150-line `rpi-gateway chat` line client over the same socket.
  - A follow-up upstream PR could add `--connect --session <id>`.
- **Discord.** serenity, DMs from your user id only, so the privileged message-content intent may be unnecessary. Messages from guilds are ignored in v1.
- **iMessage.**
  - Needs an always-on Mac (any Apple Silicon or recent Intel) signed in to your Apple ID, running BlueBubbles Server, joined to the same Tailscale network.
  - Inbound arrives as a BlueBubbles webhook to the gateway's tailnet-only listener, with a shared secret, and is checked against your handle.
  - Outbound uses the BlueBubbles REST send endpoint.
  - The BlueBubbles API shape was read from source and still needs checking against a running server. Without a Mac, this adapter stays off and the rest works.

### 4.4 Autonomy and safety (tools run without asking)

There are no per-call prompts. These hard limits hold every time:

1. **One Docker container, `zephyr`** (`zephyr up`, set up by `zephyr init`). The daemon and every child run inside it, as your uid, so files it creates are yours and no ACLs are needed. The agent can work anywhere it is mounted:
   - Read-write: `~/.zephyr` (seen as `/zephyr`), which holds its state, memory, sessions and its own rpi agent dir with its own credentials.
   - Read-write: `$PROJECTS` at the same path as on the host. Secrets inside it (`.parity-env.sh`, `.parity-lanes/archive`, every `.env*` outside `node_modules`) are covered with empty mounts.
   - Nothing else of your home: `~/.ssh`, `~/.rpi`, `~/.pi` and browser profiles are not mounted. No docker socket.
   - Network: host networking, so the attach port on 127.0.0.1 and the Tailscale webhook address work unchanged.
   - Providers come only from the image: `deploy/Dockerfile` builds `librpi_providers.so`, `rpi-cursor-lite/` and `rpi-claude-agent/` into `/opt/zephyr/ext/`, and `zephyr.toml` loads them with `extensions`. Do not `rpi install rpi-providers` into `~/.zephyr`. A package installed there loads twice (`zephyr doctor` warns).
   - `--restart unless-stopped` replaces the systemd unit; `--init` reaps children and forwards SIGTERM.

   This replaced an earlier dedicated Linux account: ACLs over 2.2M files were slow to apply and, on eCryptfs, unreliable to remove.
2. **Policy plugin `gw-policy`** (cdylib, published `rpi-plugin-sdk`, loaded with `--extension`). Its `ToolCall` veto blocks:
   - destructive patterns, as in Hermes's hardline list: `rm -rf /`, `mkfs`, `dd of=/dev/`, fork bombs, `curl | sh`
   - writes outside the granted roots
   - `git push --force`
   - package installs outside the workspace venv/node dirs

   It logs every call to the audit table.
3. **Untrusted content.** Text from iMessage, Discord, web fetches and files is wrapped as data in the prompt (`<untrusted source=…>`). The policy plugin also blocks outbound sends to any address other than the owner's channels, which limits what a prompt injection can exfiltrate.
4. **Budgets.** Daily token and cost limits, plus a per-run cap of 60 turns and 30 minutes, taken from `get_stats`. When a limit is hit, the gateway aborts the run and notifies you.
5. **Kill switch.**
   - `/stop` from any owner channel aborts the run.
   - `/pause` stops all dispatch and scheduling until `/resume`.
   - `rpi-gateway pause` does the same from a shell.
   - `docker stop zephyr` is the hard stop.
6. **Audit.** Every tool call, policy veto, inbound and outbound message goes to SQLite. A daily digest is optional.

### 4.5 Scheduler and proactivity

- Jobs live in `state.db` (cron expression or one-shot time, prompt, granted dirs, delivery channel). Create them with `/job add …` or let the agent use a `schedule` tool from `gw-policy`. Prompts are checked by the same policy.
- A tick every 30 s claims due runs in a transaction:
  - `runs(claimed → running → completed|failed|unknown)`
  - On restart, `running` becomes `unknown` and is reported, never retried automatically.
  - Missed runs are skipped, except one catch-up run per job after downtime.
- A run's output is delivered to its channel, unless the reply is exactly `[SILENT]`.
- A heartbeat is just a job: for example "every 30 min: check inbox/todo, reply `[SILENT]` if nothing to do". It is off by default.

### 4.6 Memory

- `~/agent/memory/{USER.md, MEMORY.md, notes/YYYY-MM-DD.md}`, kept in a git repo.
- These are loaded as context files when a session starts, using rpi's existing context-file loading. This is the frozen snapshot, which keeps the prompt cache warm. The agent edits them with normal file tools inside the sandbox.
- The gateway commits after each run that changed them. `/memory undo` runs `git revert`. It never reverts the first (seed) commit. rpi skips `AGENTS.md` for a cwd that `trust.json` marks `false`, so `zephyr doctor` checks for that.

### 4.7 Auth and providers

- **Until upstream PR #144 is merged**, the AuthKeeper calls `get_auth_status` on the child every `[auth] refresh_minutes` (default 10; `enabled`, default true). For each OAuth provider with a stored credential that expires within 15 minutes (or has expired), it runs `rpi auth refresh --provider <id>` as a subprocess, with the child's `RPI_CODING_AGENT_DIR` and cwd. It does not use the `oauth` RPC: upstream `rpi-cli/src/auth.rs` `run_refresh` passes the stored `{access, refresh, expires_at}` to the plugin, while the RPC handler (`modes.rs` `rpc_oauth`) forwards only the request's `input`, so it would not give the plugin the refresh token. Expiry is read from `<agent_dir>/auth.json`, field `expires` only, because `get_auth_status` has no expiry. Tokens are never read into a struct or logged.
  - Then the child must restart to bind the new token. Upstream builds each provider's request auth once, at session start (`rpi-runtime/src/provider/binding.rs:6`, `RequestAuth`). `set_model` only swaps the model descriptor (`rpi-harness/src/agent_harness.rs:5146`), so it keeps the old token and is not used. The child is restarted when idle; if a run is in progress, the restart waits for its settle.
  - Once #144 is released, this timer is removed.
- Failover: an ordered model list in `gateway.toml`. After repeated `retry_scheduled` errors or an auth error, the gateway calls `set_model` with the next model and tells you once.

### 4.8 Operations

- systemd user unit:
  - `Type=notify`, `WatchdogSec=60`, `Restart=on-failure`, `RestartSec=5`
  - `StartLimitBurst=5/10min`
  - `KillMode=mixed` with a 30 s stop timeout, so the children can drain
- On SIGTERM: stop intake, abort or finish runs, flush the outbox, then exit. rpi drains its session on `stop`.
- `rpi-gateway status` shows children, queue depth, budgets, last errors and outbox backlog. `doctor` checks the binary version, the protocol commands, bwrap, Tailscale reachability of the Mac, and credentials. It reports whether each credential is present, never its value.
- Logs go to journald. No secrets in logs, since credentials stay in rpi's agent dir.

## 5. Repo layout

New repo `yansigit/zephyr` (private), local at `/home/user/Projects/zephyr`, a Rust workspace:

```
crates/zephyr/          daemon: router, session manager, store, scheduler, auth keeper, adapters, attach server
crates/zephyr-policy/   cdylib rpi plugin: tool-call veto, audit, schedule tool, untrusted-content tagging
deploy/                 Dockerfile, zephyr.toml.example (host commands: crates/zephyr/src/host/)
```

Dependencies: tokio, serde_json, rusqlite (bundled), serenity, reqwest, sd-notify, croner, and `rpi-plugin-sdk` (crates.io, same minor as the pinned `rpi`). Reusing `rpi-extcore`, which already spawns RPC children, gets evaluated in M0. We use it only if it fits without patches.

## 6. Milestones

| M | Scope | Done when |
|---|---|---|
| M0 | Daemon skeleton: store, session manager (spawn, respawn, resume), systemd unit, `status`/`doctor`, `chat` line client | `kill -9` of the child or gateway mid-run loses no message, and the run resumes or is reported |
| M1 | TUI attach shim for stock `rpi --connect` | You attach, see history, chat, detach, and the agent keeps running |
| M2 | Container setup plus `zephyr-policy` (veto, audit, budgets, kill switch) | Each blocklist case is vetoed in tests. In the container, `/home/user/.ssh` and masked secrets are unreadable even through `bash` |
| M3 | Discord adapter, outbox | DM round trip. Duplicate delivery is dropped by nonce. Non-owners are ignored |
| M4 | Scheduler, memory, AuthKeeper | Jobs survive restart under the ledger rules. An OAuth token is refreshed with no failed request |
| M5 | iMessage via BlueBubbles | Round trip from your phone (needs the Mac) |

Each milestone gets a failing test first and runs in its own worktree. Lanes with independent work (M2 policy, M3 Discord, M5 iMessage) run in parallel on Haiku. I review and integrate them. Every commit is authored as you, with no trailers.

## 7. Open items

- The name collides with the Zephyr RTOS (a Linux Foundation project). That doesn't matter for a private repo, but check it before using Zephyr as a public brand.
- The container runs as your uid, so it can read anything mounted that you can. Only the listed secret paths are masked.
