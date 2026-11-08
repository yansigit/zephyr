# Approvals gate: messages, posts and money wait for the owner, as far as the owner wants

Status: design approved in conversation 2026-11-07; spec awaiting the owner's review.
Sub-project **A1** of the Muse-parity roadmap (the table is in
`2026-11-07-settings-registry-design.md` §1). Depends on **S**, the settings registry: every
`approvals.*` setting and the private facts are registry entries, changed through its one write
path and shown in its TUI panel.

## 1. Why

Zephyr runs every tool call without asking (DESIGN §1); outbound commands are only listed to the
owner after the turn (§4.4 item 3). The Muse research (`reports/Muse agent audit and research.md`)
found that every consent failure in Muse's record happened at the moment of acting: an "Allow
Always" grant that sent a seller's home address to strangers, an injected résumé stopped only by
the approval prompt, a "scheduled" email sent at once. "Approve What, Exactly?" (arXiv 2610.21063)
found half the runtimes it tested let an action's arguments change after approval, and that
flooding the owner with requests still worked 45–60% of the time. Zephyr has no moment of acting
for the owner to stand in today.

The owner's decisions, from the design conversation:

- Gate **(a) messages, posts and money**: actions that reach other people or spend money.
  Everything else stays autonomous and listed after the turn; general egress is A2's job.
- The owner controls the behaviour fully, from manual through semi-automatic to full automation,
  per class, with presets; the two hard limits (private facts, a money ceiling) are switchable too.
- **Approach A**: the daemon runs exactly what the owner approved, rather than letting the agent
  retry it.
- The agent may read and tighten approval settings at once and propose loosening them (S's rules).

## 2. Requirements

- Classes: **messages**, **posts**, **money**. Each has a mode; private facts have their own mode.
- Modes for messages and posts: `ask`, `ask-new`, `delay N` (minutes), `auto`. Money: `ask`,
  `delay N`, `auto` with an optional per-purchase cap. Private facts: `hold`, `warn`, `off`.
- Presets: `manual`, `semi`, `full`; any setting may be overridden after a preset. A preset never
  loosens a setting the owner set by hand; it may tighten one.
- Any change may be time-limited (S §4.5).
- What runs after `/ok` is exactly what the card showed.
- Held actions survive restarts; nothing runs late silently; nothing runs twice.
- The owner is asked on the channel of the run that asked, and in the TUI's Waiting tab; the
  card is the daemon's words, the agent's reason only as a quote.
- Loosening approvals, money or facts is accepted only from a strong channel (S §2). Money
  approvals stay allowed from any owner channel until W ships, configurable.
- Everything is recorded whatever the mode: the after-turn outbound list, the audit table, and
  S's change log.
- Existing installs keep today's behaviour after the upgrade; new installs start in `manual`.

### Non-goals

- Gating general data egress (curl POSTs, scp, rsync, `git push`): A2.
- Stopping a deliberately hostile agent that calls the real browser binary by path, posts from a
  Python script, or reaches a mail API with curl: A2 (network floor) and A3 (token broker). This
  gate stops a confused or prompt-injected agent using zephyr's own tools. Elaborate obfuscation
  of a send (encoded scripts, interpreters fed from files, a script written now and run later) is
  out of scope too (R27): the gate covers the shell forms it recognizes.
- Payments infrastructure (virtual cards): D5. A1 gates checkouts the agent drives in the browser
  and connector calls that pay.
- The web app's approval cards and push: W, built on the attach methods here.

## 3. What changes in DESIGN.md

- §1: "It runs tools on its own: no per-call human approval" becomes: tools run on their own,
  except actions in the gated classes, which follow the owner's approval modes (new §4.16).
- §4.4 item 2 (policy plugin): the plugin also asks the gate for gated calls, failing closed.
  Item 3: the outbound list stays; the private-facts check is added beside it.
- §4.11: `agent-browser` on the agent's PATH is the gate's wrapper; the real binary moves out of
  the PATH. `gateway` gains `/approvals` reads and `/set approvals.*` (S).
- §4.15 (vault): `exec`'s vault proposals share the new proposal storage; no behaviour change.
- New §4.16 Approvals (this spec, once built). README: the command reference and a section on
  approvals and presets.

## 4. Design

### 4.1 Classification: `zephyr_policy::policy::gated`

One function, shared by the plugin, the wrapper and the daemon, beside `outbound`:

```rust
pub struct Gated {
    pub class: Class,                 // Messages | Posts | Money
    pub targets: Vec<String>,         // normalized recipients, "owner/repo", origins, "server/tool"
    pub summary: String,              // one line, in the daemon's words
    pub amount: Option<Amount>,       // money only: value and currency, when it can be read
    pub replay: Replay,               // Run (shell) | Pass (connector) | Wait (browser)
}

pub fn gated(tool: &str, args: &Value) -> Option<Gated>
```

| Class | Recognized |
|---|---|
| Messages | `himalaya` sends (the same forms `himalaya_send` recognizes); recipients read from `To`/`Cc`/`Bcc` in the command's own text (a heredoc or `-H`), else the target is `unknown` (counts as new) |
| Messages | connector calls (`mcp_call`) whose tool name contains `send`, `reply`, `message`, `dm`, `email`, unless the connector's `classes` says otherwise |
| Messages | browser sends (§4.4) |
| Posts | `gh` writes (the forms `gh_write` recognizes, including `gh api` with a method other than GET, or a field or `--input`); target `owner/repo` from `-R`/`--repo`, the URL, or the API path, else `unknown` |
| Posts | connector calls whose tool name contains `post`, `publish`, `comment`, `tweet`, `share`, `create_issue`, `create_pull` |
| Posts | browser posts (§4.4) |
| Money | connector calls whose tool name contains `pay`, `purchase`, `order`, `checkout`, `transfer`, `refund`, `charge`, `subscribe`; amount from `amount`/`total`/`price` and `currency` in the arguments |
| Money | browser checkout and payment buttons (§4.4) |

As built (R22, R37, R38): a connector tool's name is normalized (`createIssue` is
`create_issue`) and matched by whole `_` words, and a name whose first word is a read verb
(`get`, `list`, `read`, `search`, `find`, `fetch`, `view`, `show`, `count`, `describe`) is not
gated. A connector message's or post's targets are `server/tool` plus the values of its
recipient-like arguments (`to`, `cc`, `bcc`, `recipient`, `recipients`, `email`, `channel`,
`channel_id`, `user`, `user_id`, `chat_id`, `thread`, `thread_id`, `phone`, `number`); with none
present the targets include `unknown`, so `ask-new` never lets such a send through on its own. A
payment's target is `server/tool`. A fact's `allow` list is compared with the recipient targets
only.

A command with several stages is classified per stage (`policy::stages`, nested `sh -c`/`eval`
as `outbound` does); the strictest class and every target count. `[[mcp]]` gains an optional
`classes = { tool_name = "messages" | "posts" | "money" | "none" }` table that overrides the verb
rule, set by the owner at `/mcp ok` or later (an S setting per connector, agent `Tighten`: it may
move a tool into a class, never out of one).

### 4.2 The decision: one place, three ways in

The daemon is the only thing that decides. It alone reads the modes, the remembered targets and
the private facts; none of these is published where the agent can read the values.

```
main's exec ──────────────────────────────▶ router (in process) ─┐
task/job bash, any session's mcp_call ──▶ plugin ──▶ gate socket ─┼──▶ gate::decide ──▶ allow | hold | wait
agent-browser click/press/Enter ─────▶ wrapper ──▶ gate socket ─┘
```

1. **Main's `exec`.** The router already runs these (§4.13). Before `procs.start`, after the
   veto and `vault::exec_env`, it calls `gate::decide`. `allow` starts the process; `hold`
   answers the call (§4.6) and starts nothing.
2. **Tasks' and jobs' `bash`, and `mcp_call` in every session.** The plugin's `ToolCall`
   handler, for a `bash` call `gated` recognizes or `outbound` lists (the second only for the
   private-facts check), and for every `mcp_call` (as built: the owner's `classes` are the
   daemon's to apply), connects to the **gate socket**, sends the request and waits for the
   verdict (at most 5 s; browser waits never reach the plugin). `allow`
   continues; anything else, including no socket, a timeout or a malformed answer, aborts (the
   plugin already fails closed). A veto shows the model no reason, so the daemon steers in
   `held for the owner as #N (…); don't retry it; carry on or end your turn`, through the same
   path the repeated-failure note uses for main (`router::steered`) and for tasks and jobs
   (`scheduler::follow`). As built (R51, A11): the note goes into the asking run before the
   plugin hears `hold` (the gate waits at most 2 s for rpi to take it); rpi 0.18 accepts that
   `queue_message` while the plugin's `ToolCall` handler is blocked on the gate and delivers it
   right after the vetoed call's result. The call's own `tool_execution_end` carries only rpi's
   "extension … blocked tool …" line, so the note is what tells the agent why.
3. **Browser sends.** §4.4.

**The gate socket** is `/var/lib/zephyr-public/gate.sock`: a Unix socket in root's 0755 public
folder (the one that holds `mcp.json`), mode 0666 so the agent can connect but not replace it.
The daemon checks `SO_PEERCRED` (the agent's uid only), takes one JSON request per connection,
caps connections at 32 as the attach port does, and closes a connection idle for 10 s. The
request names its session (`ZEPHYR_SESSION`, which `rpc::spawn_as` sets in every child, inherited
by the wrapper). As built (A7): the session is supplied by the agent's process, so a forged one
can misroute a card, bypass the per-run flood cap (the 20-a-day cap still binds) and steer a held
or pass note into another run; no verdict depends on it. Cards for socket requests carry no agent
reason.

```jsonc
// request
{"v": 1, "session": "task-7", "call_id": "call_abc", "tool": "bash" | "mcp_call" | "browser",
 "args": { ... },                       // the tool call's own arguments
 "browser": {"action": "click", "url": "https://…", "origin": "https://…",
             "element": {"role": "button", "name": "Send"}, "typed": ["…"]}}   // wrapper only
// answer
{"verdict": "allow" | "hold" | "deny", "id": 7, "text": "held for the owner as #7 (…)"}
```

**`gate::decide`**, in order (as built, R35):

1. `gated` → `None`: allow (the private-facts check below still runs on every call `outbound`
   lists, every `mcp_call`, and on browser `fill`/`type` values, whether gated or not). A
   connector call with an unused one-time pass for exactly its arguments (§4.6) goes through
   first, once.
2. **Private facts** (§4.5): a match with facts `hold` → hold as a fact hold; `warn` → note it for
   the owner's notice and go on; `off` → go on. A facts file that cannot be read holds unless
   `off`.
3. A browser action on a control the wrapper could not read is held, whatever the mode.
4. **The class's mode** (§4.3).
5. **The flood cap**: at most 3 actions waiting from one run (main's run, a task, a job run) and
   20 held in the last 24 h that the owner did not approve with `/ok` (R62: waiting, dropped,
   cancelled, expired, and delays that went by themselves count; owner-approved ones don't). It
   counts only calls that would wait and is applied after the mode, so a call allowed by `auto`
   or by known targets still runs at the cap. Past it the call is denied at once with `not run: N
   actions already wait for the owner in this run` or `not run: N actions held for the owner in
   the last day are unanswered or were declined`, and the owner gets one summary line per run,
   not a card per call.

### 4.3 Modes, presets and their settings

Registry entries (S §4.1), all in `zephyr.toml` under `[approvals]`:

| Key | Values | Agent | Strong |
|---|---|---|---|
| `approvals.preset` | `manual`, `semi`, `full` | Tighten | yes |
| `approvals.messages` | `ask`, `ask-new`, `delay N` (1–60), `auto` | Tighten | yes |
| `approvals.posts` | `ask`, `ask-new`, `delay N`, `auto` | Tighten | yes |
| `approvals.money` | `ask`, `delay N`, `auto`, `auto N` (cap) | Tighten | yes |
| `approvals.currency` | ISO 4217 code, default `USD` | Propose | yes (as built, C2: another currency changes what an `auto N` cap lets through) |
| `approvals.facts` | `hold`, `warn`, `off` | Tighten | yes |
| `approvals.strong_only` | list of `loosen`, `money` | Tighten (add only) | yes |

Strictness, strictest first: `ask` < `ask-new` < `delay N` (larger N stricter) < `auto`; for money,
`auto N` is stricter than `auto`, and a lower cap is stricter; `hold` < `warn` < `off`.

| Preset | messages | posts | money | facts |
|---|---|---|---|---|
| `manual` (new installs) | ask | ask | ask | hold |
| `semi` | ask-new | delay 5 | ask | hold |
| `full` | auto | auto | auto | warn |

- The **effective value** of a class is the hand-set value when there is one, else the preset's.
- Setting a preset drops every hand-set value that is looser than the new preset's, and keeps
  every one that is stricter. So a preset never loosens what the owner set by hand. `/approvals`
  shows `semi (modified)` while a hand-set value differs from the preset.
- Commands (all callers of S's `set`): `/approvals` shows everything; `/approvals manual|semi|full
  [for D]`; `/approvals off [for D]` is `full`; `/approvals on` is `manual`; `/approvals messages
  MODE`, `/approvals posts MODE`, `/approvals money MODE`, `/approvals facts MODE`.

Per mode, for a gated call:

- `ask`: hold.
- `ask-new`: allow when every target is in `known_targets(class, target)`; a target `unknown`
  never is. Otherwise hold. Approving with `/ok N` adds the action's targets; `/ok N once` does
  not. As built (R46, A6): targets are remembered only when the class is `ask-new` at approval,
  the owner answered `/ok N` (not `once`, and never a delay running out), and the action started.
- `delay N`: a `Run` or `Pass` action is stored `scheduled` with `due_at = now + N min`, and the
  owner is told `sending at 14:05 unless /cancel 7`; a `Wait` action (browser) holds the page for
  `min(N, 10)` minutes, then allows unless cancelled. The agent is told `delayed as #7 (…): it
  goes by itself in N min unless the owner cancels it with /cancel 7`. As built (R63): when it
  comes due, its payload is checked again under the facts and the modes as they are then; if
  either would now hold it (or delay it longer), it goes back to `pending` and the owner gets its
  card again, with why.
- `auto`: allow. Money with a cap: allow when the amount is known, in `approvals.currency`, and at
  or under the cap; otherwise hold. An unknown amount counts as over the cap.

### 4.4 Browser sends: the wrapper

The image moves the real `agent-browser` to `/opt/zephyr/libexec/agent-browser` and puts a
root-owned `/opt/zephyr/gate/bin/agent-browser` (a symlink to `/usr/local/bin/zephyr`, which acts
as the wrapper when `argv[0]` is `agent-browser`) **first** on `agent::AGENT_PATH`, ahead of
`/zephyr/.local/bin`, so a copy the agent drops in its own bin does not shadow it. The image's
`ENV` that points agent-browser at the browser binary and profile is unchanged.

- Every command except `click`, `press`, `keyboard`, `fill`, `type` and `find … click` is passed
  through with `exec` (no wait, no change).
- `fill`/`type`: the value is checked for private facts (§4.5) through the gate socket, then
  passed through. These are never gated by class.
- `click`/`find … click`/`press Enter`/`keyboard press Enter`: the wrapper asks the real binary
  for the page URL (`get url`, as the vault's fill does) and for the target element's role and
  accessible name (from a `snapshot` scoped to the ref, or the focused element for a key press),
  and classifies:
  - **Money**: the name matches `buy`, `pay`, `place order`, `checkout`, `complete purchase`,
    `confirm order`, `subscribe`, `donate`, `transfer` (case-insensitive, whole words, in the
    page's language list shipped with the classifier: English first, more added as data). The
    amount is the page's labelled total (`total`, `order total`, `amount due`) when one is found.
  - **Posts**: the name matches `post`, `publish`, `tweet`, `share` or `comment`, or the target
    is `submit` on a form that has a textarea.
  - **Messages**: the name matches `send` or `reply`, or Enter is pressed in a
    `textbox`/`contenteditable` composer that is not a search box (`role=searchbox`,
    `type=search`, or a name/placeholder with the word "search", so "Send research summary" is
    still a send). A browser send's `ask-new` target is the page's origin in both classes.
  - Not gated: login forms (the form has a password field; the vault's fill handles those), and
    everything else (links, tabs, menus, navigation). As built (R58): a password field in a form
    excludes only messages and posts (a login form's "Send code"); a money label is gated whatever
    the form holds, since a checkout may carry an optional account password. Only Enter in a
    field reads the form's password field: a click never does, so a clicked "Send code" is held.
  - The snapshot fails or the ref matches nothing: held as "a click the gate could not read".
  - As built (A8): the wrapper reads commands as agent-browser 0.41 does (global flags anywhere,
    `batch` strings split like a shell, `press` key chords as its `parse_key_chord`). Clicks are
    `click`, `dblclick`, `tap` and `find … click`; Enter is any spelling (`press`, `key`,
    `keydown`, `keyup` with `Enter`, `Return` or `NumpadEnter`, with modifiers, `\n` or `\r`, a line
    break typed with `type` or `keyboard type`), and Space or Enter on a focused button is a click
    on it. A click on a child of a button counts as the button (R53); a scoped read is followed by
    a full snapshot so the agent's refs keep working (R54). A `batch` carrying any gated command or
    reading stdin, `mcp`, `chat` and `webmcp invoke` (R55) are refused. A Tab before a line break,
    or keys after the first Enter, hold as "a key press the gate could not read" (R57). After an
    `allow` the wrapper reads the page again and refuses `the page changed while the owner decided;
    not run` when it is not what the owner approved, and tells the gate whether it ran; the owner
    is told when an approved click did not run. An amount it cannot read counts as over the cap.
    The lexicon is English and Korean; an unrecognized label is not gated (a documented limit). The
    wrapper trusts only a gate socket run by root.
- A gated click goes to the gate socket as a `Wait` request. The wrapper blocks until the
  verdict: `allow` runs the real command; `hold`/`deny` exits non-zero with the daemon's text on
  stderr, which is what the agent reads. While it waits, nothing touches the page.
- The daemon's own browser calls (the vault's fill) use the real binary by full path.
- The plugin's veto refuses `/opt/zephyr/libexec/agent-browser` named in any tool's path or bash
  word, like a credential path. That is a name check: a copy of the binary gets past it (§2
  non-goals).

### 4.5 Private facts

- **Store**: `<state>/facts.toml`, root's, 0600, added to `DAEMON_FILES` (the host CLI reaches it
  through `zephyr __file`) and to the daily backup (§4.8). Read on every use; one in-process
  writer lock; edited with `toml_edit`.

  ```toml
  [[fact]]
  name = "home-address"            # [a-z0-9][a-z0-9_-]{0,63}
  kind = "text"                    # text | digits | email
  value = "…"
  allow = ["https://www.amazon.com"]   # targets it may go to without asking
  ```

- **Matching**: both sides NFKC-normalized, case-folded, whitespace runs collapsed. `digits`
  compares digit sequences only (`+1 (555) 010-2345` matches `5550102345`), at least 6 digits.
  `email` compares case-insensitively. `text` is a substring match, at least 4 characters. Each
  value is also matched in its URL-encoded and standard/URL-safe base64 forms at the three byte
  alignments, as `vault::scrubbed` does (shared code).
- **What is checked**: the call's own text, for every call `outbound` lists, every gated call,
  `mcp_call` arguments, and the wrapper's `fill`/`type` values. Not the contents of files a
  command sends (an `scp` of a file holding the address is not caught).
- **Modes**: `hold` holds the action, the card naming the fact (`contains your private fact
  home-address`) and never its value; `warn` runs it and tells the owner which fact went where;
  `off` checks nothing. A fact's `allow` targets skip the check for that target.
- **Commands**: `/facts` (names, kinds, allowed targets; never a value), `/facts rm NAME`, `/facts
  allow NAME TARGET`, `/facts disallow NAME TARGET`. A value is entered only in the TUI panel
  (a masked field, as the vault's) or with `zephyr facts add NAME --kind K` on the host (hidden
  prompt or stdin), never in a chat message. Attach methods `zephyr.facts_list|facts_put|
  facts_rm` mirror the vault's, never returning a value.
- **The agent** (through `gateway`): `/facts` reads names; `/facts add NAME KIND VALUE` applies at
  once (it only tightens) and tells the owner the name; removing a fact or adding an allowed
  target is a proposal, and a strong one.
- **Never logged**: `Debug` prints lengths only; audit arguments and every notice are scrubbed of
  fact values as they are of vault values.

### 4.6 Held actions: storage and lifecycle

The `proposals` table (schema v16) gains columns, so one id space covers slash-command proposals
(today's `gateway` and vault proposals, kind `command`) and actions, and `/ok N` serves both:

```sql
ALTER TABLE proposals ADD COLUMN kind TEXT NOT NULL DEFAULT 'command'; -- command|run|pass|wait
ALTER TABLE proposals ADD COLUMN class TEXT;          -- messages|posts|money
ALTER TABLE proposals ADD COLUMN targets TEXT;        -- JSON list
ALTER TABLE proposals ADD COLUMN summary TEXT;        -- the card's first lines, daemon's words
ALTER TABLE proposals ADD COLUMN reason TEXT;         -- the agent's words, quoted on the card
ALTER TABLE proposals ADD COLUMN payload TEXT;        -- exact command and its timeout, or tool + args JSON
ALTER TABLE proposals ADD COLUMN secrets TEXT;        -- vault token names (exec)
ALTER TABLE proposals ADD COLUMN origin TEXT;         -- JSON {channel, sender, session, run}
ALTER TABLE proposals ADD COLUMN due_at INTEGER;      -- delay
ALTER TABLE proposals ADD COLUMN settled_at INTEGER;
ALTER TABLE proposals ADD COLUMN result TEXT;
CREATE TABLE known_targets(class TEXT NOT NULL, target TEXT NOT NULL,
                           added_at INTEGER NOT NULL DEFAULT (unixepoch()),
                           PRIMARY KEY (class, target));
```

`status` gains `scheduled`, `running`, `cancelled`, `expired`, `failed` and `unknown` beside
`pending`, `done` and `dropped`.

- **`/ok N`** (and `/ok N once`; `zephyr.decide`):
  - `run`: the daemon runs `payload` word for word, as the agent, through `procs` (the same
    wrapper, log and limits as `exec`; cwd the agent's home; vault grants for `secrets` checked
    again now). It is reported back to main as an inbox row `action-<id>`:
    `[action #7 done; zephyr ran what the owner approved] …` with the output quoted and cut like
    a process report (§4.13). A run that was held from a task or job reports to the channel that
    started it, through main, as task reports do.
  - `pass`: a one-time pass bound to SHA-256 of `server`, `tool` and the canonical (sorted-key)
    `arguments`, good for 30 minutes, stored in the daemon. The session that asked is told to
    make the call once (a steer if it is still running, else an inbox row to main); the gate
    allows exactly that digest once and consumes the pass in the same transaction.
  - `wait`: the waiting wrapper's connection gets `allow`.
  - `ask-new`: the action's targets go into `known_targets` unless `once`.
- **`/no N`**: dropped; the agent gets a short inbox note (`the owner declined #7 (…); don't try
  it again`), a waiting wrapper gets `deny`.
- **`/cancel N`**: a `scheduled` action becomes `cancelled`, a delayed browser wait gets `deny`.
  It is for delayed actions only: a `pending` one is pointed at `/no N`.
- **`/held`**: lists `pending` and `scheduled` actions, newest first (also the TUI's Waiting tab).
- **Expiry**: `pending` actions expire after 24 h (the agent is told), browser waits after 10 min.
  The sweep runs on the router's tick.
- **Due delays** run on the router's tick through the same path as `/ok`.

### 4.7 The card and where it goes

```
[zephyr] approve #7: message (email)
to: alice@example.com (new) · subject: "Invoice March"
first lines: "Hi Alice, attached is the invoice for…"
the agent says: "the owner asked me to send the March invoice"
/ok 7 · /ok 7 once · /no 7 · expires in 24 h
```

- Money cards show the amount (or `amount: could not be read`), the merchant origin and the page
  title; delay cards say `sending at 14:05 unless /cancel 7`; fact holds name the fact; cards that
  need a strong channel say so.
- The summary is built by the daemon from the payload (recipients, subject and first lines from
  the message text; repo and title from `gh` arguments; tool and arguments from `mcp_call`; page,
  element name and typed values from the wrapper), never from the agent's text. The agent's
  reason is the run's latest assistant text, cut to 200 characters, one line, quoted.
- Destination: the channel and sender of the run that asked (`Router::origin`); for a task, the
  channel that started it; for a job, its delivery channel. Also the TUI's Waiting tab and badge
  (S §4.6), and later W's Approvals screen with push.
- **Strong channels for approval**: `approvals.strong_only` (default `["loosen"]`) lists what may
  be approved only from a strong channel. `loosen` is any proposal that relaxes an `approvals.*`
  setting or a fact. Adding `money` locks purchase approvals to the TUI (and W with a passkey);
  it is off by default until W ships so a purchase can be approved from a phone. As built (R48):
  a `delay N` mode is the owner's standing choice, and `strong_only` governs who may approve, so
  a delayed payment that runs out sends without a strong-channel approval. `loosen` is enforced by
  the `approvals.*` settings' `strong` flag and `Router::not_now` (facts), whether or not the
  list names it.
- The existing rules stand: agent lines starting `[zephyr]` go out as `[agent]` (§4.4 item 7), so
  no card can be forged in the agent's own text.

### 4.8 What the agent sees

- On a hold: `held for the owner as #7 (message to alice@example.com); don't retry it; carry on
  or end your turn` (exec's answer, the steered note for a vetoed call, or the wrapper's stderr).
  As built (C4): on a delay, `delayed as #7 (…): it goes by itself in 5 min unless the owner
  cancels it with /cancel 7; don't retry it; carry on or end your turn`.
- On a decision: the action's report row, or the decline or expiry note.
- AGENTS.md gains a fixed `## Approvals` section (compiled in, like `## Tools`): which actions are
  gated, that `gateway /approvals` reads the current modes, that held actions run without a retry
  once approved, and that routing around a hold (curl to a mail API, a Python script, another
  browser) breaks the owner's rule. The section does not list the modes' values, so a mode change
  needs no child restart.

### 4.9 Failure handling

- **No gate** (the daemon restarting, the socket missing): the plugin and the wrapper deny.
- **`/stop`**: kills running approved actions (they are `procs` processes) and denies every
  browser wait; `pending` actions stay pending. As built (R47): every `scheduled` action goes
  back to `pending` too, its card shown again, so nothing set to go by itself goes after a stop.
- **`/pause` and a budget pause**: `/ok` answers `paused; /resume first`. As built (I1): a
  delay that comes due during the pause is never run late by itself: at `/resume` (or the first
  tick after a budget pause lifts) it goes back to `pending` and the owner is asked again, with
  its card; one not yet due keeps counting down.
- **A child killed while its wrapper waits** (failover, re-exec, crash): the wait gets `deny`.
- **Restarts**: rows are durable. At start, a `scheduled` action whose `due_at` passed while the
  daemon was down goes back to `pending` and the owner is asked (never run late silently, the job
  ledger's rule); a `running` action becomes `unknown` unless `procs` adopts it or finds its exit
  file (§4.13), and it is never run again.
- **Settings and facts** are read on every decision, so a change applies to the next call.
- **Audit**: every decision is an `audit` row (`held`, `auto`, `known`, `approved`, `once`,
  `dropped`, `expired`, `cancelled`, `fact-hold`, `fact-warn`; as built also `pass`, `denied` and
  `due`), arguments scrubbed of vault and fact values and cut to 500 bytes, as today.
- **`doctor`**: fails when the gate socket is missing while the daemon is up, when
  `/opt/zephyr/gate/bin` is not root-owned or not first on `AGENT_PATH`, or when another
  `agent-browser` is reachable on `AGENT_PATH`.

### 4.10 Upgrade and onboarding

- **New installs** (no `state.db` before this version): `[approvals]` absent means `manual`.
  `zephyr init` asks which preset to start with (manual, semi, full) and whether to add private
  facts now (opens the hidden prompts).
- **Upgrades** (the v16 migration runs on an existing `state.db`): if `zephyr.toml` has no
  `[approvals]`, the daemon writes `approvals.preset = "full"` with a comment, so the install
  behaves as it did, and sends the owner one notice: `[zephyr] approvals are off, as before this
  version; /approvals manual or /settings turns them on`.

## 5. Testing

Each piece starts with a failing test (CLAUDE.md, DESIGN §6).

- **`zephyr-policy` unit tests**: `gated` over every `himalaya` send form, `gh` write and `gh
  api` method, connector verbs and `classes` overrides, multi-stage and nested commands; target
  extraction (`unknown` cases); the browser lexicon including search boxes and login forms; mode
  strictness; preset application (hand-set stricter kept, looser dropped); fact normalization
  and matching (digits, email, text, encoded forms, minimum lengths, `allow` targets);
  `gateway_kind` for `/approvals`, `/facts` and `/set approvals.*`.
- **Store tests**: v16 migration on a v15 database; proposal kinds sharing one id space; the
  flood cap; expiry; `known_targets`; the restart rules (overdue `scheduled` → `pending`,
  `running` → `unknown`); one-time pass consumed once.
- **Router integration tests** (`tests/router.rs`, fake-rpi): `fake-rpi.py` gains a second role:
  for `bash` and `mcp_call` calls it plays the plugin and asks the gate socket, as it already
  plays the plugin for `gateway`. Cases:
  - main `exec` of a `himalaya` send held; `/ok` runs the stored text exactly (seen in
    `log.jsonl` / the process log); the report row reaches main;
  - `ask-new`: a second send to the same recipient runs at once; `/ok N once` does not remember;
  - `delay 1` then `/cancel`; a delay that falls due while the daemon is down goes back to
    `pending`;
  - the flood cap's fourth call denied with one summary line;
  - a task's `bash` held through the socket, the steered note, `/ok` running it, the report
    going to the task's channel;
  - a connector pass consumed once, a second identical call held again;
  - facts `hold` and `warn`, and an `allow` target;
  - `/approvals full` from the `chat` channel refused (strong), from the TUI accepted; the agent
    tightening `approvals.messages` applied at once, loosening proposed;
  - the upgrade path writing `preset = "full"` and its notice; a fresh home starting in `manual`;
  - a browser wait through the wrapper with a fake `agent-browser` script that records what ran;
    `/stop` denying it; a timeout denying it.
- **TUI screen tests**: the Approvals section of the settings panel, preset rows marked
  `(modified)`, the Waiting tab with an action card, the facts editor's masked field.
- **Ignored real-rpi test** (`crates/zephyr-policy/tests/rpi_load.rs`, as built, not
  `tests/real_rpi.rs`): the plugin's gate-socket round trip under the pinned binary, a held `bash`
  call vetoed and the steered note delivered.
