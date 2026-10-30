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

- **`main`** is one long-lived session shared by all three channels. The owner's `/new` starts a fresh one, `main-<unix secs>`, kept in `state.db` so restarts reopen it; the old file stays. Attack success rises with session length (ClawSafety Table 3: 50% at 10 turns, 77.5% at 64). It is your assistant, so the context continues whichever channel you use. Each inbound message is prefixed with its channel, e.g. `[discord] …`. The reply goes back to the channel the message came from.
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
   - A send that takes over 10 s counts as failed, and that channel is skipped for 30 s, so one stuck channel (a chat client that stopped reading, a hung HTTP call) cannot hold up the router or the other channels. The chat channel also drops a connection that does not take a line within 2 s.
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
  - `get_state` reports the model the agent is on now (the active account's, or `model`), so the client's footer follows `/model` and failover.
  - What the router writes itself for `tui` (command replies, notices such as the outbound list and memory diffs, job output) has no agent event, so it is sent to attached clients as an assistant message. With no client attached, notices and job output wait in the outbox for the next attach.
  - Unverified: these details come from reading the code, not from a test run. Fallback: a ~150-line `rpi-gateway chat` line client over the same socket.
  - A follow-up upstream PR could add `--connect --session <id>`.
  - For stock clients, a user turn is drawn as an assistant block `**<channel>** · <text>` (the `[channel #id]` tag removed), and `get_state` reports the live account's model (`Snapshot.model`) and `thinkingLevel` (`Snapshot.thinking`).

#### 4.3.1 Attach protocol: `zephyr.*` extensions

Zephyr's own TUI uses the same port, token and framing (one JSON-RPC 2.0 object per line; events are `{"jsonrpc":"2.0","method":"event","params":{"subscriptionId":1,"event":<event>}}`). Everything here is additive: the stock client never calls these methods, and it drops the `zephyr_*` event types (its `apply_line` fails to decode them as a `RemoteEvent`). Both methods need `authenticate` first (else error `-32001`).

**`zephyr.hello {}`** → `result` is the current `Snapshot`:

```jsonc
{
  "version": "0.4.0",                // zephyr's crate version
  "session": "main",
  "account": "work",                 // string | null: the pooled account the agent runs on; null without [[account]]
  "model": "anthropic/claude-x",     // string | null: that account's model, else `model`; null = rpi's default
  "thinking": "medium",              // string | null: rpi's get_state thinkingLevel, read when the child starts
  "paused": "user",                  // string | null: "user" or "budget:<YYYY-MM-DD>"; null = not paused
  "channels": [                      // the adapters this daemon runs, then the known ones it doesn't
    {"name": "chat",    "state": "connected"},
    {"name": "discord", "state": "connected"},
    {"name": "imessage","state": "off"}
  ],                                 // state: "connected" | "off" | "reconnecting" (today: running adapter = connected, not configured = off)
  "spend": {
    "tokens": 1234,                  // integer: today's (UTC) tokens, all accounts and jobs
    "cost": 0.0123,                  // number: today's cost in USD
    "daily_tokens_limit": 500000,    // integer | null: [budget] daily_tokens
    "daily_cost_limit": 5.0          // number | null: [budget] daily_cost_usd
  },
  "queue": [                         // inbox rows waiting (status new), oldest first
    {"channel": "discord", "preview": "first 60 chars of the text…", "since_unix": 1793318400}
  ],                                 // since_unix: integer, when the row arrived (seconds); age = now - since_unix
  "failed_replies": 0,               // integer: outbox rows that failed for good
  "run": {
    "busy": true,                    // between agent_start and agent_settled
    "since_unix_ms": 1793318400123,  // integer | null: when agent_start arrived
    "origin": {"channel": "discord", "id": 42, "sender": "1234"},  // object | null: the user message being served (inbox row id)
    "current_tool": "bash",          // string | null: between tool_execution_start and _end
    "tools_done": 3                  // integer: tool_execution_end count this run
  },
  "accounts": [                      // the [[account]] pool in order; [] without one
    {"name": "work", "model": "anthropic/claude-x",
     "state": "cooling",             // "ready" | "cooling" | "needs login"
     "until_unix": 1793322000}       // integer | null: when a cooldown ends
  ],
  "jobs": [                          // the 5 soonest scheduled runs, soonest first
    {"id": 3, "name": "renew domain", "next_unix": 1793350800}
  ]
}
```

When idle, `run` is `{"busy": false, "since_unix_ms": null, "origin": null, "current_tool": null, "tools_done": 0}`. `preview` is the row's text with whitespace runs folded to one space, cut to 60 chars, with `…` appended when cut.

**`zephyr.subscribe {}`** → `result` is `{"subscriptionId": 1, "sessionId": "main", "status": "subscribed", "messages": [...]}`. `messages` is rpi's raw `get_messages` list (user, assistant with `toolCall` blocks, `toolResult`, ...), unchanged except that every user message gets two more fields:

```jsonc
{"kind": "user", "role": "user", "content": "[discord #42] hi", "timestamp": 0,
 "origin": {"channel": "discord", "id": 42},  // object | null: from the [channel #id] tag; null when untagged
 "text": "hi"}                                 // the message text without the tag
```

Then events follow, in order:
1. `{"type": "ready"}`
2. `{"type": "zephyr_status", "snapshot": <Snapshot>}` at once, then again each time the snapshot changes (never twice with the same value). It is a `watch`: each event carries the latest whole snapshot, and quick intermediate values (a tool that starts and ends between two reads) may be skipped. Its order relative to the child events in 3 is not fixed.
3. Every live child event (the `--mode rpc` line: `agent_start`, `message_update`, `tool_execution_start`, `agent_settled`, ...), except `response` lines. User messages in `message_start`/`message_end` get `origin` and `text` as above. Unlike `subscribe`, nothing is quoted or hidden: the client's own turns come back too.
4. `{"type": "zephyr_reply", "text": "agent: account work on ...", "command": "/status"}`: a reply to the `tui` channel that the agent's stream does not show: answers to commands (`/status`, `/model`, `/reload`, `/stop`, `/pause`, `/resume`, `/memory ...`, `/job ...`; `command` is the command text), notices and job output (`command` null), and `[zephyr] ...` fallbacks for a run that ended without a reply (`command` null). An ordinary agent reply is not repeated. Stock `subscribe` connections get this event too and ignore it.

**`zephyr.models {}`** → `{"accounts": [{"name": "work", "model": "anthropic/claude-x", "models": ["anthropic/claude-x", "anthropic/claude-y"]}]}`: each pooled account, its model, and the models `rpi --list-models` lists on that account's provider (run with the account's agent dir). Without a pool there is one entry with `name` null, the top-level `model`, and everything rpi lists. When rpi cannot list, `models` holds just the account's model. It runs rpi once per account, so a client asks only when it shows the list (the TUI's model picker).

A slash command goes in as `send {"sessionId": "main", "command": {"type": "prompt", "content": "/status"}}`, as with the stock client.

#### 4.3.2 The TUI client (`zephyr tui`)

`zephyr attach` runs `zephyr tui` in the container: a client of the methods above, built from rpi-tui components (`crates/zephyr/src/tui/`).

- **Layout**, top to bottom:
  - **Header**: the mascot, a small cloud puffing the west wind, beside `zephyr · session · account › model` (with the pause badge), then the channel dots and today's spend (failed replies too).
    - The cloud's face is the agent's state: eyes ahead when idle, looking downwind with an animated trail while a run goes, closed with a `z` when paused or connecting, `× ×` when the link is down, a reply failed or the budget is spent.
    - Below 60 columns the header is one row without it, and spend moves to the footer.
  - **Transcript**:
    - Your messages are full-width shaded bands (`user_bg`) with a bar in the channel's colour down the left edge (`>` without UTF-8) and the channel and local time at the right of the first row, dropped when it would take over half the row.
    - `≋` marks assistant text. rpi-tui's markdown wraps by character, so Zephyr draws each reply once unwrapped and wraps its prose at words, with list items hanging under their text. Code, rules and tables keep upstream's layout.
    - A turn's tool calls fold into one line (`▸ 3 tools  bash, read ×2 … ✓ 4.1s`) with any failure under it; Ctrl+O opens them as a tree.
    - Replies come as cards: a dim title, then the lines on a dim `│` rule, no box. Notices are dim lines.
    - One blank row between blocks; none at the top. Marks (`▌`, `≋`) sit in the gutter; text, tool rows and cards line up two columns in.
  - Then the run line, the queue strip, slash autocomplete, the input box, and a footer of key hints.
  - Everything sits in a one-column gutter.
  - Times are in `timezone` from zephyr.toml, else the host's zone, which `zephyr attach` passes in.
- **Keys**: Enter sends; Shift+Enter or Ctrl+J is a newline; Esc stops the run; Ctrl+L model picker; Ctrl+S status; `?` on an empty input, help; Ctrl+O tool output; Ctrl+T thinking; PgUp/PgDn scroll; Ctrl+C clears (twice to detach); Ctrl+D detaches.
- **Panels** open over the bottom of the screen, between header and footer. Esc closes; arrows and PgUp/PgDn move; typing filters the lists; the footer shows the panel's keys.
  - *Model picker* (Ctrl+L, `/model`): `zephyr.models` grouped by account, the model in use marked `●`. A pick sends `/model provider/model`; the reply shows as a notice.
  - *Status* (Ctrl+S, `/status`): agent and run, the account pool with cooldowns, channels, pause, spend against the limits, failed replies, the next jobs. All from the snapshot.
  - *Help* (`/help`, `?`): every command, grouped Chat, Agent, Memory, Schedule and Gateway. Enter puts the command in the input.
- **Autocomplete**: typing `/` lists the matching commands with one line each. Up/Down choose, Tab completes, Enter runs a command that needs no argument.
- `/help`, `/status` and `/model` with no argument open their panel and send nothing. A one-line `[zephyr] …` reply (`stopped`, `paused`, `now on …`) is a dim notice; a longer reply is a titled card.
- **Discord.** serenity, DMs from your user id only, so the privileged message-content intent may be unnecessary. Messages from guilds are ignored in v1.
  - While a run for you is going, the DM shows typing (serenity's `Typing`, refreshed every 7 s; Discord's typing lasts 10 s). If it is still going after `[discord] progress_after_secs` (default 20), one note is posted, `≋ working · 0:24 · bash, read ×3`, edited at most every 5 s and deleted on `Stop` or before the reply's first chunk. `progress = false` turns the note off; typing stays.
  - The note is not an outbox row: never retried or deduped. A failed post, edit or delete is logged once per run and ignored. The router drives this through `Channel::activity(to, Activity::{Start, Progress, Stop})`, a no-op for other channels.
- **iMessage.**
  - Needs an always-on Mac (any Apple Silicon or recent Intel) signed in to your Apple ID, running BlueBubbles Server, joined to the same Tailscale network.
  - Inbound arrives as a BlueBubbles webhook to the gateway's tailnet-only listener, with a shared secret, and is checked against your handle.
  - Outbound uses the BlueBubbles REST send endpoint.
  - The BlueBubbles API shape was read from source and still needs checking against a running server. Without a Mac, this adapter stays off and the rest works.

### 4.4 Autonomy and safety (tools run without asking)

There are no per-call prompts. These hard limits hold every time:

1. **One Docker container, `zephyr`** (`zephyr up`, set up by `zephyr init`). Two users inside it:
   - **The agent** (every rpi child, and every process zephyr starts for it: `rpi auth refresh`, `rpi --list-models`, memory's git) runs as your uid, so files it creates are yours and no ACLs are needed.
   - **The daemon** runs as root holding only SETUID, SETGID and KILL (`--cap-drop ALL`, `no-new-privileges`, `--pids-limit 4096`). Without DAC_OVERRIDE, root is bound by file permissions like anyone else. Its own files live in `~/.zephyr-daemon` (root, 0700), seen as `/var/lib/zephyr`: `zephyr.toml`, `secrets.env`, `tui.token`, `host.toml`, `state.db`, the chat socket and the lock. The agent can neither read nor change them, so it cannot learn the bot tokens, rewrite the settings (dropping the policy plugin or adding an owner), or speak as the owner on the chat socket. Whatever the daemon does in the agent's home it does with the agent's file ids (`agent::fs`, setfsuid), so a symlink the agent plants leads nowhere new; a missed call site fails closed, since root cannot even enter `/zephyr`. `zephyr up` moves an older layout over once and gives the dir to root in a throwaway container; the host CLI then reads and writes those files through `zephyr __file` in the container, as root.
   - Before a child starts, secrets.env's variables are removed from its environment (`[agent] env` lets named ones through), and the daemon is undumpable.
   - Left readable by the agent, by necessity: the provider credentials rpi itself reads (`accounts/*/auth.json`, the Claude config) and the conversation (`sessions/`).

   The agent can work anywhere it is mounted:
   - Read-write: `~/.zephyr` (seen as `/zephyr`), which holds its memory, sessions and its own rpi agent dir with its own credentials.
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
   - credential files (`auth.json`, `.credentials.json`, `secrets.env`), `/proc/*/environ`, `.ssh/` and `.git/hooks/`, named in any tool's path or any bash word. A hook in a project would run as the owner, on the host, at their next commit. This is a name check, so a glob or an encoded name gets past it: the boundary is the agent's environment and uid (item 1), not this list.

   It logs every call to the audit table.
3. **Untrusted content and what leaves.** Text the agent reads is not wrapped in `<untrusted>` tags: the plugin cannot rewrite tool output, and wrapping only lowered attack success from 57.7% to 41.7% in AgentDojo (Table 5). Nothing outbound is blocked either; an egress allowlist did not lower attack success in ClawSafety (NemoClaw, 45.8% vs 40%), because most leaks went into normal replies. Instead the owner sees what left: the daemon reads each bash `tool_execution_start` from rpi's event stream (which the agent cannot forge) and, after the turn, lists every command that sent data off the machine (`zephyr_policy::policy::outbound`: curl/wget with a body or a non-GET method, ssh, scp, sftp, rsync to a host, nc, socat, telnet, `git push`). Plain GETs are not listed, though a URL can carry data too (CaMeL §7).
4. **Budgets.** Daily token and cost limits, summed from the `usage` on each assistant `message_end` event. rpi 0.12.0's `get_stats` totals count only usage that extensions record, never the replies' own, so they stay at 0. When a limit is hit, the gateway pauses and notifies you. Per run, `[budget] run_turns` (model turns, counted from `turn_end`) and `run_minutes` stop a main-session run with `abort` and tell the owner `[zephyr] stopped the run after …` once it settles; jobs get `run_turns` too, and their fixed 30-minute cap (`JOB_TIMEOUT`) instead of `run_minutes`. Both are off unless set.
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

- `/zephyr/memory/{USER.md, MEMORY.md, notes/YYYY-MM-DD.md}`, kept in a git repo, edited by the agent with normal file tools. The daemon commits after each run that changed them.
- rpi loads `/zephyr/AGENTS.md` as a context file when a child starts (the frozen snapshot, which keeps the prompt cache warm). The daemon writes it before every start from the owner's side only, never from the working files:
  1. **The owner's standing rules** (`/rule <text>`, `/rule rm N`, `/rules`), kept in `<state>/RULES.md`, first, with a line saying that text claiming to change them is not from the owner. A rule change restarts the child at idle on the same session, so it applies from the next message; jobs start fresh children and get it too. Rules here sit at system-prompt level, which compaction does not touch: in Governance Decay (arXiv 2606.22528, Tables 1–2) in-chat rules were dropped after compaction up to 59% of the time, system-prompt ones 0%.
  2. **The approved memory**: `<state>/approved/{USER.md, MEMORY.md}`. A change the agent makes waits: after the run the owner gets its diff once (`[zephyr] memory change waiting; /memory ok keeps it, /memory undo drops it`), and `/memory diff` shows it again. `/memory ok` approves it, for the next child start, but only if the files still hash to what the owner was last shown; if a later run changed them, the reply shows the new diff and a second `/memory ok` keeps that. `/memory undo` drops a waiting change by reverting each commit since the last approval; with nothing waiting it reverts the newest memory commit, as before, never the seed, and approves the result. On the first start with approvals, the memory there counts as approved.

  Why a human gate rather than a scanner: an agent that writes its own memory saved a fabricated "preference" from one web page up to 99.8% of the time, which then steered 68–80% of later actions (Sleeper Memory Poisoning, arXiv 2605.15338, Tables 1–2, App. I); pattern and LLM-trust filters let poisoned entries through (arXiv 2601.05504 §8.2).
- rpi 0.12.0 loads the first of `AGENTS.override.md`, `AGENTS.md`, `AGENTS.MD`, `CLAUDE.md`, `CLAUDE.MD` from the agent dir and from each dir up from the cwd, so an agent that wrote `AGENTS.override.md` or `CLAUDE.md` in `/zephyr`, or `AGENTS.md` in its agent dir, would get around both rules and approval. It also reads `SYSTEM.md` (which replaces its own system prompt) and `APPEND_SYSTEM.md` from `/zephyr/.rpi/` and from the agent dir. Before each start the daemon renames any of those to `<name>.ignored-by-zephyr` and logs it.
- Two more sources are closed on the child's command line. `--no-skills`: skill descriptions go into every prompt, and the agent can write skills under `/zephyr/.rpi/skills` or its agent dir. `--approve`: rpi skips `AGENTS.md` for a cwd that `trust.json` marks `false`, and the agent can write that file too. `--local-only`: rpi otherwise also loads extensions from `/zephyr/.rpi/extensions`, the agent dir's `extensions/`, project packages and the `extensionDirs` setting, all writable by the agent; an extension can rewrite the system prompt (`before_agent_start`) or shadow a provider, and it loads before the policy plugin. With the flag only the `--extension` files zephyr passes (the policy plugin and the image's providers) load.
- `<state>` is root's in the container (§4.4), so the agent can change neither the rules nor the approved copies.

### 4.7 Auth and providers

- **Until upstream PR #144 is merged**, the AuthKeeper calls `get_auth_status` on the child every `[auth] refresh_minutes` (default 10; `enabled`, default true). For each OAuth provider with a stored credential that expires within 15 minutes (or has expired), it runs `rpi auth refresh --provider <id>` as a subprocess, with the child's `RPI_CODING_AGENT_DIR` and cwd. It does not use the `oauth` RPC: upstream `rpi-cli/src/auth.rs` `run_refresh` passes the stored `{access, refresh, expires_at}` to the plugin, while the RPC handler (`modes.rs` `rpc_oauth`) forwards only the request's `input`, so it would not give the plugin the refresh token. Expiry is read from `<agent_dir>/auth.json`, field `expires` only, because `get_auth_status` has no expiry. Tokens are never read into a struct or logged.
  - Then the child must restart to bind the new token. Upstream builds each provider's request auth once, at session start (`rpi-runtime/src/provider/binding.rs:6`, `RequestAuth`). `set_model` only swaps the model descriptor (`rpi-harness/src/agent_harness.rs:5146`), so it keeps the old token and is not used. The child is restarted when idle; if a run is in progress, the restart waits for its settle.
  - Once #144 is released, this timer is removed.
- Failover between accounts: §4.9. With `[[account]]` set, the AuthKeeper checks every account's `<home>/accounts/<name>/auth.json` for its model's provider, without asking the child, so a cooling account stays fresh. Only a refresh of the account the child runs on restarts it.

### 4.8 Operations

- systemd user unit:
  - `Type=notify`, `WatchdogSec=60`, `Restart=on-failure`, `RestartSec=5`
  - `StartLimitBurst=5/10min`
  - `KillMode=mixed` with a 30 s stop timeout, so the children can drain
- On SIGTERM: stop intake, abort or finish runs, flush the outbox, then exit. rpi drains its session on `stop`. A run that ends during the drain is settled as usual, so its spend and outbound list are booked; one cut off at the 20 s deadline is booked with what it did so far.
- `rpi-gateway status` shows children, queue depth, budgets, last errors and outbox backlog. `doctor` checks the binary version, the protocol commands, bwrap, Tailscale reachability of the Mac, and credentials. It reports whether each credential is present, never its value.
- Logs go to journald. No secrets in logs, since credentials stay in rpi's agent dir.

### 4.9 Accounts and failover

- `[[account]]` entries (`name`, `model`), in failover order, replace `model`. Without them nothing changes: one child on `agent_dir` and `model`. A name must be a plain path segment (letters, digits, `-`, `_`, `.`, not starting with `.`) and unique; a model must be `<provider>/<model>`.
- Each account's `RPI_CODING_AGENT_DIR` is `<home>/accounts/<name>/` (mode 0700, made absolute because rpi refuses a relative one). It holds only that account's `auth.json`. Zephyr links `models.json` and `settings.json` in it to the ones in `agent_dir`, when they exist. Other agent-dir files (`trust.json`, skills, a global `AGENTS.md`) are not linked; trust is forced on and skills are off anyway (§4.6).
- The child runs on the first account not cooling down. Cooldowns are in `state.db` (`account_cooldown(name, until, reason, level)`), so they survive restarts.
- **Detection.** rpi 0.12.0 puts the last assistant message's `errorMessage` into `agent_settled.error` (checked against the pinned binary), so the router reads the settle. It is a rate limit when the text contains the providers' marker `rate_limited;` (with an optional `resets_at=<unix>`), or matches `429|rate.?limit|usage limit|quota|too many requests`, case-insensitive. Without a reset time (or one already past) the cooldown backs off 15 min, 1 h, then 4 h. A completed turn on the account resets the backoff.
- **Failover.** The router marks the cooldown, puts the turn's unanswered rows back to `new`, and restarts the child the same way the AuthKeeper does (when idle), on the next ready account, with the same `--session-id main` and session dir. After an error turn rpi 0.12.0 keeps the user message and the error message, and `pendingResume` is false, so there is nothing to resume. The new child re-sends each row as a prompt with the same `[channel #id]` tag, and reconcile matches the reply to the later copy. The owner is told on the row's channel: `account a is rate-limited until <time>; switched to b`.
  - A failed account cools down until a time in the future, so it is not tried twice in one turn.
  - When every account is cooling down, no child runs. Messages wait in the inbox, the owner is told the earliest reset, and the child starts at that time and sends what waited.
  - Jobs (§4.5) start on the first ready account. A rate-limited attempt cools that account and retries once per account in a fresh session; when every account is cooling down the run fails at once.
- **rpi's own retries.** rpi 0.12.0 retries an error whose text says `429`, `rate limit` or `too many requests` (`rpi-harness` `is_retryable_assistant_error`) up to `retry.maxRetries` times, default 10, with a delay doubling from 2 s, bounded by `maxAgentDelayMs` (60 s) (`RetryPolicy`, `settings.json` key `retry`: `enabled`, `maxRetries`, `baseDelayMs`, `maxAgentDelayMs`). There is no per-error switch, and `enabled = false` would also drop retries of 5xx and network errors. So in pool mode Zephyr sets `retry.maxRetries` to 1 in the shared `settings.json` (it writes the file only if it is plain JSON; a file with comments is left alone and logged). Failover then starts after one 2 s retry. Quota texts (`insufficient_quota`, `quota exceeded`, `usage limit`) and the `rate_limited` marker are not retried by rpi at all.
- **Budget.** Daily totals are unchanged and count every account. Each settle's usage is also added to `account_spend(day, account)`, shown by `zephyr status`.
- **`zephyr status`** prints one line per account: `ready`, `cooling until <time> (<reason>)`, or `needs login` when the account's `auth.json` has no key for its provider (only the keys are read). An API key given by environment variable shows as `needs login`.

### 4.10 Live settings

- The router owns the config as an `Arc<Config>` and sends each new one to the scheduler on a `watch` channel. `Config::load` records the file it read.
- `home` is the agent's (sessions, memory, accounts); `state` (default `home`; `/var/lib/zephyr` in the container) holds `state.db`, `secrets.env`, the socket and the lock. Changing either re-execs.
- **Triggers.** `/reload` (owner-only, outside the queue, like `/pause`; `zephyr reload` sends it over the chat socket, and the host CLI runs it after every edit), and the router's 5 s tick when the mtime of `zephyr.toml` or `secrets.env` changed. A file that does not load or validate is reported and not applied.
- **Plan** (`reload::diff`), the least disruptive that covers the change:
  - `rpi`, `home`, `[discord]`, `[imessage]`, `[attach]`, or any `secrets.env` pair: **re-exec**. Once main is settled and no job runs, the router drains as on SIGTERM and `zephyr run` execs itself with the same args, minus the variables secrets.env set, so the new process reads them again. The lock and sockets are close-on-exec; under `--init` the PID stays.
  - `extensions`, `policy_plugin`, `agent_dir`, the active account changing (the pool would now pick another), a model on another provider, or a newer `auth.json` for the active account (a re-login): **restart the child** when idle, as the AuthKeeper does, on the same session.
  - The active account's model on the same provider: **`set_model`** when idle (before reconcile sends the next row). rpi checks it against its catalogue; the sign-in stays the same, so the bound credentials (§4.7) are still right. A refusal is told to the owner.
  - Anything else (budget, owners, time zone, `[auth]`, the pool order): read where it is used.
- **`/model PROVIDER/MODEL`** sets it on the active account if that is on the provider, else on the first account that is, moved to the front; checks that `rpi --list-models`, signed in as that account, lists it (the running child lists only its own sign-in's models); writes `zephyr.toml` (atomically, comments kept), then reloads. `/status` adds the agent's account and model to what `zephyr status` prints.

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
- The agent runs as your uid, so it can read anything mounted that you can. Only the listed secret paths are masked.
- The agent can read the provider credentials rpi itself needs (`accounts/*/auth.json`, the Claude config) and the conversation (`sessions/`); the policy plugin's name check is the only thing in the way, and it can be bypassed.
- A plain GET can carry data out in its URL; outbound alerts list only commands that send a body or use another method (§4.4 item 3).
