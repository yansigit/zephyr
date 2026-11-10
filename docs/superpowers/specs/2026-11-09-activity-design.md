# Activity: the daemon's own record of what ran, what left, what waited and what was refused

Status: design drafted 2026-11-09; awaiting the owner's review.
Sub-project **A4** of the Muse-parity roadmap (the table is in
`2026-11-07-settings-registry-design.md` §1). Depends on **A1** (the gate's audit rows, held
actions, `known_targets`) and **S** (`settings_log`). The later web app (W) reads it through the
attach method in §4.7.

## 1. Why

The research (`reports/Muse agent audit.md`, "A4 must be the daemon's account, with revoke on
each entry") draws two lessons from Muse. First, the Aten case: the user's audit was the model's
own account of its access, and that account was wrong. Muse even suggests asking the chat "Show
all URLs you have visited in the last week". Second, the Robb case: one "Allow Always" tap
stayed in force for 24 hours before he learned what it had done, and the way to revoke it was
somewhere else, under Settings. The report's recommendation is that `/activity` be a query the
daemon renders over its own tables, never text the model writes. It should include what was
refused, and it should put a revoke next to every grant that keeps working.

Zephyr already records nearly all of this in root's `state.db`, which the agent can neither
read nor write, but its views show only current state (`/held`, `/job list`, `/task list`,
`/process list`, `zephyr status` counts). Three gaps:

- The after-turn outbound list (`rpc::Tally.outbound`, rpc.rs:472; `alert()`, rpc.rs:598) lives
  only in memory: gone once the notice is sent, lost before that if the daemon crashes.
- `Store::prune` (store.rs:1761) deletes `audit` and `settings_log` at 90 days, but `runs`,
  `tasks` and `procs` grow without limit.
- Nothing takes back a remembered target: `remember_targets` (store.rs:1668) adds to
  `known_targets` and no command removes one, which leaves `ask-new` an open-ended grant.

## 2. Requirements

- `/activity [today|Nh|Nd] [KIND]` on any owner channel, answered by the router outside the
  queue as `/held` is: the model never sees the request or writes the answer.
- The agent reads it with `gateway "/activity …"`, except the grants view (§4.6).
- `zephyr.activity`, an attach method paged by cursor, so W can scroll back 90 days; the TUI
  settings panel gains **Activity** and **Grants** tabs.
- One query (`Store::activity`) over `audit`, `settings_log`, command proposals, `runs`,
  `tasks` and `procs`; the grants view over `known_targets`, `setting_reverts` and turned-on
  jobs.
- Each outbound command is an audit row from the moment rpi starts the call, labelled as the
  after-turn notice labels it.
- Every text shown is scrubbed at read time with the vault and the facts as they are now,
  failing closed: no vault or fact value ever appears.
- Every standing grant is shown with the command that takes it back; known targets get the
  revoke they lack (`/approvals forget N`).
- `runs`, `tasks` and `procs` are pruned at 90 days, never letting an id be reused or a
  one-shot job fire again (§4.9).

### Non-goals (decided, with the reason)

- **No hash chain on `audit`.** The report suggests an HMAC chain keyed by a root-only secret.
  But `state.db` is root's (0600, in the 0700 state folder): whoever can edit it is root and
  holds the key. The weak point is upstream: the stream-derived rows can be forged by a hostile
  agent writing frames into rpi's stdout (DESIGN §4.4 item 3), and a chain over forged input
  proves nothing. Worth building once it can be anchored off the box, with W's passkey receipts.
- **No commitment notices** (an allowed action that agreed to something): that needs reading
  what a message means. The gate's cards, `fact-warn` and the outbound list are what the
  payload can say.
- **No Upcoming view**: `/held` shows due delays, `/job list` the schedule; W draws the tab.
- **Not listed as grants**: one-time passes (30 minutes, consumed on use) and vault grants
  (`/vault` lists them, `/vault revoke` takes one back).
- **Not in the record**: inbox and outbox rows; the session is the conversation's record.
- **No live push** on `zephyr.subscribe` (the tabs reload on opening and after a revoke), no
  host `zephyr activity` (`zephyr chat` reaches `/activity`), and no way for the agent to
  forget a target.
- **No schema change**: everything fits the v16 tables (§4.2, §4.4), so A4 takes no schema number (B takes v17)
  (open question 1).

## 3. What changes in DESIGN.md

- §4.4 items 3 and 6: each command `policy::outbound` lists is also an `audit` row
  (`tool = 'outbound'`, its label as args) from the call's start; the notice is unchanged.
  `runs`, `tasks` and `procs` are kept 90 days, the newest of each always (§4.9).
- §4.3.1 `zephyr.activity`; §4.3.2 the Activity and Grants tabs; §4.11 `gateway` reads
  `/activity …` (not `grants`); §4.16 `/approvals forget N`, logged in `settings_log`.
- New **§4.19 Activity**, describing this spec once built. README gains `/activity` and
  `/approvals forget`; CLAUDE.md's architecture list gains `activity.rs`.

## 4. Design

### 4.1 Sources and kinds

A new module, `crates/zephyr/src/activity.rs`, holds the argument parser, the rendering and the
grants view. The SQL lives in `store.rs`. Each record has exactly one **kind**:

| Kind | From | Rows |
|---|---|---|
| `out` | `audit`, `tool = 'outbound'` | a command that sent data off the machine (§4.5) |
| `gate` | `audit`, any other outcome | the gate's `auto`, `known`, `held`, `fact-hold`, `fact-warn`, `pass` (gate.rs:610, :794); the owner's and the tick's `approved`, `once`, `due`, `dropped`, `cancelled`, `expired`, `stopped`, `failed`, `re-asked` (actions.rs:301) |
| `refused` | `audit`, outcome `refused` or `denied` | the policy's veto (scheduler.rs:566), the gate's denial |
| `tool` | `audit`, outcome `started` or `error` | every other tool call: the full lineage, shown only on request |
| `setting` | `settings_log` | every change, revert, refused loosening and forgotten target |
| `proposal` | `proposals`, `kind = 'command'` | the agent's proposed commands and settings, and how each ended |
| `job` | `runs` joined to `jobs` | each run of a job |
| `task` | `tasks` | each background task |
| `process` | `procs` | each `exec` command |

Held actions (`proposals` of kind `run`/`pass`/`wait`) are not a separate source, because their
`held` row and the decision row on it are already `gate` rows. A bare `/activity` shows every
kind except `tool`, which would bury the rest (every `read` call is a row).

Rows of kind `out`, `tool` and the policy's `refused` come from rpi's event stream. Every other
row is the daemon's own state. `zephyr.activity` says which is which (`stream`, §4.7): following §4.4 item 3, the
stream's rows can be forged by a hostile agent; the others cannot.

### 4.2 The query

```rust
pub struct Record { pub at: i64, pub src: String, pub id: i64, pub kind: String,
                    pub subject: String, pub status: String, pub text: String }

impl Store {
    /// Newest first, `at >= since`, of `kinds` (empty: all but `tool`), strictly before `cursor`.
    pub fn activity(&self, since: i64, kinds: &[&str], before: Option<&Cursor>, limit: u32)
        -> rusqlite::Result<Vec<Record>>;
}
```

One `UNION ALL` in a CTE, filtered and ordered once (only `audit` is large, and `audit_ts`
indexes it):

```sql
WITH a(at, src, id, kind, subject, status, text) AS (
  SELECT ts, 'audit', id,
         CASE WHEN tool = 'outbound' THEN 'out'
              WHEN outcome IN ('refused', 'denied') THEN 'refused'
              WHEN outcome IN ('started', 'error') THEN 'tool' ELSE 'gate' END,
         session, outcome, iif(tool = 'outbound', args, tool || ' ' || args)
    FROM audit WHERE ts >= ?1
  UNION ALL SELECT at, 'settings_log', id, 'setting',
         actor || coalesce(' on ' || channel, '') || coalesce(' (#' || proposal || ')', ''),
         actor, key || ' ' || coalesce(old, 'none') || ' → ' || coalesce(new, 'none')
    FROM settings_log WHERE at >= ?1
  UNION ALL SELECT coalesce(settled_at, created_at), 'proposals', id, 'proposal', '#' || id,
         iif(status = 'pending' AND created_at <= unixepoch() - 86400, 'expired', status), command
    FROM proposals WHERE kind = 'command' AND coalesce(settled_at, created_at) >= ?1
  UNION ALL SELECT coalesce(r.ended_at, r.started_at, r.due_at), 'runs', r.id, 'job',
         '#' || r.job_id || ' run ' || r.id, r.status, coalesce(j.name, '(removed)')
    FROM runs r LEFT JOIN jobs j ON j.id = r.job_id
   WHERE coalesce(r.ended_at, r.started_at, r.due_at) >= ?1
  UNION ALL SELECT coalesce(ended_at, started_at, created_at), 'tasks', id, 'task', '#' || id,
         status, goal FROM tasks WHERE coalesce(ended_at, started_at, created_at) >= ?1
  UNION ALL SELECT coalesce(ended_at, created_at), 'procs', id, 'process', '#' || id,
         status || coalesce(' ' || exit_code, ''), command
    FROM procs WHERE coalesce(ended_at, created_at) >= ?1)
SELECT * FROM a WHERE instr(?2, ',' || kind || ',') AND (at, src, id) < (?3, ?4, ?5)
ORDER BY at DESC, src DESC, id DESC LIMIT ?6
```

`?2` is the comma-wrapped kinds; with no cursor `?3..?5` are `(i64::MAX, '', 0)`. A record
appears once, at its latest change (a task moves up when it ends). Keyset pages on
`(at, src, id)` stay correct when two rows share a second. One change outside the query:
`settle_proposal` (store.rs:1363) also sets `settled_at = unixepoch()` (v16's column, which the
action paths already set), so a command proposal is dated by its decision.

### 4.3 `/activity` and its lines

Up to two words, either order: a time, `today` (from local midnight in `[timezone]`,
`scheduler::zone`) or `Nh`/`Nd` from 1 h to 90 d (`settings::duration`, settings.rs:663, made
`pub(crate)`; default 24 h), and a kind, one of §4.1's, singular or plural, or `grants` (§4.4).
Anything else: `[zephyr] usage: /activity [today|6h|7d] [out|gate|refused|setting|proposal|job|task|process|tool|grants]`.
Wired in `Router::command` (router.rs:2000) as `("/activity", arg)` beside `/held`; at most 30
records (31 are asked for; a 31st means more).

```
[zephyr] activity in the last 24 h, newest first:
14:05 gate main approved: exec #31 email to bob@x.org
14:02 out main: curl POST api.example.com
14:02 gate main held: exec {"command":"himalaya message send -H 'To: bob@x.org' …"}
14:01 refused task-12 by the policy: bash {"command":"rm -rf /"}
13:58 setting agent on tui: tasks.parallel 4 → 2
13:40 task #12 completed: summarise the inbox
13:00 job #3 run 88 completed: morning-brief
12:10 process #7 exited 0: cargo build --release
11-08 22:15 proposal #29 done: /vault grant token GH_TOKEN gh
(only the newest 30; /activity KIND or a shorter time narrows it)
```

- A line is `TIME KIND SUBJECT[ STATUS]: TEXT`; `TIME` is `HH:MM` today in the owner's zone
  (`settings::clock`, settings.rs:984), `MM-DD HH:MM` for earlier days. `STATUS` is left out
  when `started` and for `setting` (the actor is the subject); for `refused` it reads
  `by the policy` (`refused`) or `by the gate` (`denied`).
- The header names the window (`since 00:00 today`, `in the last 6 h`) and the kind if one was
  given (`tasks in the last 24 h`); an empty window is `[zephyr] nothing recorded in the last 24 h`.
- Every line goes through `gate::Scrub` (gate.rs:460, made `pub(crate)`, loaded once per
  answer: `vault::scrub`, then `facts::scrub`), then `settings::one_line(…, 200)`
  (settings.rs:1027). A fact saved after the row was written is still scrubbed (as R70 does for
  summaries); an unreadable vault or facts file withholds the text (`[output withheld: …]`)
  while the time, kind and subject still show.

### 4.4 Grants, each with its revoke

`/activity grants` lists what still lets something happen without asking. Each line ends with
the command that takes it back:

```
[zephyr] standing grants, each with what takes it back:
messages bob@x.org, known since 11-02 · /approvals forget 4
posts yansigit/zephyr, known since 10-30 · /approvals forget 2
budget.daily_cost_usd until 18:00, then 5 · /set budget.daily_cost_usd 5
job #3 morning-brief, 0 7 * * *, to discord · /job rm 3
```

- **Known targets** (`SELECT rowid, class, target, added_at FROM known_targets`). They get a
  new revoke, `/approvals forget N`: one more arm in `Router::approvals` (router.rs:2550), and a
  new `Store::forget_target(n)`: `DELETE FROM known_targets WHERE rowid = ?1 RETURNING class,
  target`. Any owner channel may use it, since it only makes the gate stricter. It logs
  `log_setting("approvals.known.<class>", Some(target), None, "owner", channel, None)`
  (store.rs:1285), so the forgetting is an activity row, and replies `[zephyr] forgot bob@x.org
  for messages; under ask-new a message to it waits for you again` (unknown `N`: `[zephyr] no
  remembered target #N; /activity grants lists them`). `APPROVALS_USAGE` (router.rs:510) gains
  the form. `N` is the implicit rowid, which only a backup restore could renumber; forgetting
  the wrong row then only makes the gate ask again.
- **Time limits** (`setting_reverts`): `/set KEY VALUE` with the value the revert would restore
  (`none` when unset), which drops the pending revert (DESIGN §4.17); the registry still decides
  whether the channel may make it.
- **Turned-on jobs** (`jobs WHERE enabled = 1`): `/job rm N`. The view is current state, not paged.

### 4.5 Outbound commands as audit rows

The recording goes in `scheduler::audit` (scheduler.rs:566), which `Router::tapped`
(router.rs:721) and `scheduler::follow` already call for main, tasks and jobs.

- In its `tool_execution_start` branch, for `bash` or `exec`, it writes one more row per label
  `zephyr_policy::policy::outbound(command)` returns: `store.audit(session, "outbound", label,
  outcome)`. The command comes from the raw event args, as `Tally::add` reads it (rpc.rs:533);
  `outcome` is the call's own (`started`, or `refused` when `policy::check` denies it); the label
  is put through `vault::scrubbed` and `facts::scrubbed` first, as the call's row is
  (scheduler.rs:587).
- Same function as the notice, so record and notice agree; written at the start, so a run the
  daemon dies in still leaves it. `Tally.outbound` and `alert()` are unchanged. Whether a held
  send then ran is the gate's and the decision's rows to say.

### 4.6 The agent's read, through `gateway`

`zephyr_policy::tools::gateway_kind` (tools.rs:140) treats `/activity` with up to two
printable ASCII words, 40 bytes in all, as a `Gateway::Read`, unless one of the words is
`grants`. The grants view stays out of the agent's reach because it lists which recipients
pass without asking, and that is exactly what an injected run would aim at.

- `Router::gateway_reply` (router.rs:983) answers with the owner's text, the daemon's words;
  `tests/fake-rpi.py`'s `gateway_kind` (line 130) changes in the same commit.
- AGENTS.md's fixed `## Tools` text (`memory::agents_md`) gains one sentence: *When the owner
  asks what you did, sent or were refused, read `gateway "/activity …"` and give zephyr's
  record; do not answer from memory.*

### 4.7 `zephyr.activity`

The method sits behind `authenticate`, like the others. It is dispatched in attach.rs next to
`zephyr.proposals` (attach.rs:349), through a new `AttachRequest::Activity { params, reply }`
answered in the router.

Request (every field optional):
`{"range": "today" | "6h" | "7d", "kind": "out", "before": "<cursor>", "limit": 50}`.
`range` defaults to `24h`. `kind` is one of §4.1's or `grants`. `limit` is 1 to 200, default 50.

Response:

```json
{"since": 1762646400,
 "entries": [{"at": 1762693320, "kind": "out", "subject": "main", "status": "started",
              "text": "curl POST api.example.com",
              "line": "14:02 out main: curl POST api.example.com",
              "stream": true}],
 "next": "1762693320.audit.5512"}
```

- `text` and `line` are scrubbed as in §4.3; `line` is the chat line, so neither the TUI nor W
  formats anything. `stream` is true for `out`, `tool` and the policy's `refused` rows (§4.1).
- `next` is the last entry's cursor, `null` when the window is done: `AT.SRC.ID`, parsed
  strictly (an integer, one of the six table names, an integer).
- With `kind: "grants"`, each entry has `kind: "grant"`, its `subject` (`messages`, the key,
  `#3`), `at` (when it was granted), `line` and `revoke` (the command), and `next` is `null`.
- Errors: a bad `range`, `kind`, `before` or `limit` is `-32602`, with the usage text. A
  shutting-down daemon is `-32000`, as for the other methods.

### 4.8 The TUI tabs

In `tui/settings_panel.rs`, `Tab` (line 141) gains `Activity` and `Grants`, which `tabs()`
(line 541) pushes after `Waiting`.

- `conn.rs` sends `zephyr.activity` as it sends `zephyr.proposals` (conn.rs:539), answered as a
  new `Msg::Activity`.
- **Activity** loads on opening and shows each `line` (`tool` rows muted); moving onto the last
  row asks for the next page with `before: next`. **Grants** shows each `line`; `r` sends the
  row's `revoke` as a typed command (the TUI is the owner on `tui`) and reloads. Help (`?`) and
  the footer gain `r` ("Grants: take it back").
- A daemon without the method (`-32601`) gives an empty tab, `this zephyr keeps no activity
  record; zephyr upgrade`, as an old daemon's proposals default to none.

### 4.9 Pruning runs, tasks and procs

`Store::prune` (store.rs:1761) gains three statements, run in the same sweep and counted in its
return value:

```sql
DELETE FROM runs WHERE status NOT IN ('claimed', 'running')
   AND coalesce(ended_at, due_at) < ?1
   AND due_at < (SELECT max(due_at) FROM runs r WHERE r.job_id = runs.job_id)
   AND id < (SELECT max(id) FROM runs);
DELETE FROM tasks WHERE status NOT IN ('queued', 'running')
   AND coalesce(ended_at, created_at) < ?1 AND id < (SELECT max(id) FROM tasks);
DELETE FROM procs WHERE status != 'running'
   AND coalesce(ended_at, created_at) < ?1 AND id < (SELECT max(id) FROM procs);
```

A plain age cut would break two things; the guards prevent both:

1. **A one-shot job would fire again.** `claim_due` (store.rs:1843) dates a job's next time
   from `max(due_at)` of its runs, else `jobs.created_at`, and a one-shot RFC3339 job stays
   turned on after its run (`next_after`, scheduler.rs:131). With its only run deleted, its time
   is again after `created_at` and before now. Keeping each job's latest run (a yearly cron's
   too) leaves `claim_due` unchanged.
2. **Reports would be dropped.** Without `AUTOINCREMENT`, deleting the highest rowid lets SQLite
   reuse it, and reports enter the inbox keyed `task-<id>` / `proc-<id>` with `INSERT OR IGNORE`
   (store.rs:2051, :2137, :2249) against inbox rows that are never deleted. Keeping each
   table's highest id rules reuse out (run ids feed `job-J-R` sessions and `j<id>` likewise).

`zephyr status` counts (store.rs:2317) then cover 90 days; the README's `status` line says so.

### 4.10 Failure handling

- A store error: `[zephyr] reading the activity failed: {e}`, as `/held` answers
  (actions.rs:393); on attach `-32000` with the same words. An unreadable vault or facts file
  withholds the text (§4.3), never shows it raw.
- An outbound row that fails to write is logged (`zephyr: writing the audit failed`), as every
  audit write is; the call and the notice are unaffected.
- `/approvals forget` on a row just gone gives the unknown-`N` reply; a delete whose log row
  fails still forgets, logs the failure and says so.
- A failing prune statement is logged and the sweep goes on (retention.rs:25), as today.
- After an upgrade there are no outbound rows from before it; everything else is in the tables.

## 5. Testing

- **Unit** (`activity.rs`): parsing (`today`, `6h`, `90d` accepted; `91d`, `0h`, three words,
  an unknown kind refused; plurals; either order); one golden line per kind and status rule
  (`by the policy`, `by the gate`); `HH:MM` against `MM-DD HH:MM` across midnight in a non-UTC
  zone; the cursor round-trips and a malformed one is refused.
- **Store**: `activity` ordering across all six sources, the kind filter, keyset pages with two
  rows in one second (no overlap, no gap); a command proposal pending over 24 h reads `expired`;
  `settle_proposal` sets `settled_at`; `forget_target` returns and logs the row. Prune: after it,
  `claim_due` claims nothing for a one-shot job that already ran; the next task gets a new id and
  its report reaches the inbox; `running`, `queued` and `claimed` rows stay. Extend
  `the_sweep_prunes_state_db_past_ninety_days` (retention.rs:341).
- **`scheduler::audit`**: `curl -X POST https://api.example.com` in `bash` and in `exec` writes
  an `outbound` row `curl POST api.example.com`; a vault value in the URL is stored as
  `[vault:NAME]`; a vetoed one is `refused`. Tests listing the whole audit after an outbound
  command gain the row; those filtering by tool (router.rs:4492) are unaffected.
- **Router** (fake-rpi): `/activity` after a run with an outbound curl lists
  `out main: curl POST …`, with a task and a job run beside it; a fact saved after the call is
  scrubbed; the owner's `/activity` never reaches the child (`log.jsonl`); `/activity grants`
  after an `ask-new` `/ok` lists the target, and forgetting it logs a `setting` row and holds
  the next send to it again; `gateway "/activity today"` is answered, `gateway "/activity
  grants"` refused.
- **Attach**: `limit: 2` gives a `next` whose page neither overlaps nor skips; grants carry
  `revoke`; bad params are `-32602`.
- **`zephyr-policy`**: `gateway_kind` accepts `/activity`, `/activity today task`, `/activity 7d`
  and refuses `/activity grants`, three words and non-ASCII; fake-rpi.py checked against the
  same cases.
- **TUI screens** (`tests/screens/`): the Activity lines (`tool` muted) and the next page at the
  bottom; `r` on a Grants row sends its revoke and reloads; an old daemon's empty tab.

## Decided with the owner (2026-11-10)

1. A4 takes no schema number; B takes v17 and W v18.
2. The grants view leaves vault grants out; `/vault` lists them.
3. `/activity` (not `/activity grants`) stays readable by the agent through `gateway`.
