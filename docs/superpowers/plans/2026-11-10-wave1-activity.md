# Activity (A4) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `/activity`, the daemon's own record of what ran, what left the machine, what waited and what was refused, rendered from root's `state.db` and never written by the model, with a revoke beside every standing grant, outbound commands recorded from the call's start, and `runs`/`tasks`/`procs` kept 90 days.

**Architecture:** One `UNION ALL` query (`Store::activity`) over the v16 tables, paged by a keyset cursor. A new pure-ish `activity.rs` parses the arguments, scrubs and renders each record into the one line every client shows, and builds the grants view; the router answers `/activity` outside the queue (as `/held`), `gateway` reads it for the agent (not `grants`), `zephyr.activity` serves the same JSON to the TUI and later W. No schema change.

**Tech Stack:** Rust (MSRV 1.89, CI 1.95), rusqlite 0.41, jiff (zones), serde_json, ratatui TUI, Python `fake-rpi.py`.

**Spec:** `docs/superpowers/specs/2026-11-09-activity-design.md` (its "Decided with the owner (2026-11-10)" overrides the body). Wave index: `docs/superpowers/plans/2026-11-10-wave1.md`.

## Global Constraints

- The wave index's Global Constraints apply in full (commits, worktree per lane, failing test first, fmt/clippy `-D warnings`/tests before each commit, event waits, fake-rpi mirror, scrubbing, root rules, docs in the same task).
- Branch `claude/activity`, cut from `main` at `a7e3c9e` or later; task ids A4-1 … A4-8; **no schema change** (`SCHEMA_VERSION` stays 16).
- Spec values, verbatim: chat answer at most **30** records (**31** asked for); `zephyr.activity` `limit` **1–200**, default **50**; range `today` or `Nh`/`Nd` from **1 h to 90 d**, default **24h**; each text `settings::one_line(…, 200)`; agent `gateway "/activity …"` at most **two** printable ASCII words, **40** bytes for the whole command; prune at **90 days** (`retention::DB_AGE`); a command proposal pending over **86400 s** reads `expired`.
- Every shown text goes through `gate::Scrub` loaded once per answer (vault, then facts), failing closed (`[output withheld: …]`), then `one_line`. The agent-forgeable `audit.session` (gate socket requests name it) is scrubbed and cut too (`one_line(…, 40)`).
- Copy strings exactly: usage `[zephyr] usage: /activity [today|6h|7d] [out|gate|refused|setting|proposal|job|task|process|tool|grants]`; store error `[zephyr] reading the activity failed: {e}`; footer `(only the newest 30; /activity KIND or a shorter time narrows it)`; empty `[zephyr] nothing recorded in the last 24 h` (window words vary); grants header `[zephyr] standing grants, each with what takes it back:`; forget `[zephyr] forgot {target} for {class}; under ask-new {a message|a post|a payment} to it waits for you again`; unknown `[zephyr] no remembered target #{n}; /activity grants lists them`; old daemon tab `this zephyr keeps no activity record; zephyr upgrade`.

## Review Focus

1. **Pruning a database where a one-shot job's only run is older than 90 days** (wave index item 5): the job does not fire again, and a new task's (and process's) report is not dropped as a duplicate `task-<id>`. Test in A4-2.
2. **A timed preset in the grants view** (`/approvals semi for 2h`): its `setting_reverts.value` is an `approvals::snapshot` (`manual; messages=ask`), not a value, so a naive `/set approvals.preset manual; messages=ask` revoke is broken. The line shows the snapshot; the revoke is `/set approvals.preset manual` (the snapshot's first item, `default` → `none`) and works when sent. Test in A4-5.
3. **Agent-written text dressed as a daemon line**: a task goal, command or forged gate session holding `\n[zephyr] forgot …`, U+202E or a fact value. Each record stays one line, the fake line is not a line of its own, the value is `[fact:NAME]`. Test in A4-4.
4. **A row written between two `zephyr.activity` pages** (the agent keeps working while W scrolls back): page 2 neither repeats nor skips any row page 1's cursor was older than. Test in A4-1.
5. **A phone keyboard's capitals** (`/activity Today`, `/activity 7D Tasks`, `/activity GRANTS` through `gateway`): words are read case-insensitively by the daemon, and the agent's `grants` refusal is case-insensitive too. Tests in A4-4 and A4-6.

---

### Task A4-1: The query and its cursor

**Files:**
- Modify: `crates/zephyr/src/store.rs` (new types beside `Revert` `:504`; `activity` after `audit_rows` `:1746`; `settle_proposal` `:1363`)

**Interfaces:**
- Produces (W consumes these exactly):
  ```rust
  pub struct Record { pub at: i64, pub src: String, pub id: i64, pub kind: String,
                      pub subject: String, pub status: String, pub text: String }
  #[derive(Debug, Clone, PartialEq, Eq)]
  pub struct Cursor { pub at: i64, pub src: String, pub id: i64 }
  impl Cursor { pub fn parse(s: &str) -> Option<Cursor>; pub fn of(r: &Record) -> Cursor; }
  impl std::fmt::Display for Cursor // "AT.SRC.ID", e.g. "1762693320.audit.5512"
  pub const ACTIVITY_SOURCES: [&str; 6] = ["audit", "settings_log", "proposals", "runs", "tasks", "procs"];
  impl Store {
      /// Newest first, `at >= since`, of `kinds` (empty: all but `tool`), strictly before `before`.
      pub fn activity(&self, since: i64, kinds: &[&str], before: Option<&Cursor>, limit: u32)
          -> rusqlite::Result<Vec<Record>>;
  }
  ```
- `Cursor::parse`: exactly three `.`-separated parts, an `i64`, one of `ACTIVITY_SOURCES`, an `i64`; anything else `None`.

- [ ] **Step 1: Write the failing tests** in `store.rs` `mod tests` (seed rows with the store's own writers, then age them with `UPDATE … SET ts/at/created_at = ?`):
  - `activity_lists_every_source_newest_first`: one row of each source at distinct times → kinds in `at` order `["process","task","job","proposal","setting","gate","out"]` (newest first) and `tool` absent with `kinds = &[]`; a job run of a removed job reads text `(removed)`.
  - `activity_maps_audit_outcomes_to_kinds`: `outbound`/`started` → `out`; `bash`/`refused` and `exec`/`denied` → `refused`; `bash`/`error` → `tool`; `exec`/`held` → `gate` with text `exec {…}`; `outbound` text is the bare label.
  - `activity_filters_by_kind`: `kinds = &["task"]` returns only tasks; `&["tool"]` returns the `started` row.
  - `keyset_pages_neither_overlap_nor_skip_within_one_second`: five audit rows all at `ts = 100` plus a `settings_log` row at 100 → pages of 2 via `Cursor::of(last)` concatenate to the single 6-row answer, no duplicates.
  - `a_row_written_between_pages_changes_neither_page` (Review Focus 4): take page 1 (limit 2) of 5 rows, then `store.audit(…)` a new row now and a `settings_log` row at the cursor's own second; page 2 from the cursor equals rows 3–4 of the original list.
  - `a_command_proposal_pending_a_day_reads_expired`: `created_at = now - 86_401`, status `pending` → record status `expired`.
  - `settling_a_proposal_dates_it`: `settle_proposal(id, "done")` sets `settled_at` (non-null, ≥ `created_at`), and the record's `at` is it.
  - `cursors_round_trip_and_bad_ones_are_refused`: `parse(&c.to_string()) == Some(c)`; `None` for `""`, `"1.audit"`, `"1.audit.2.3"`, `"x.audit.1"`, `"1.inbox.1"`, `"1.audit.y"`.
- [ ] **Step 2:** `cargo test -p zephyr --lib store::tests::activity` (and the other names) → FAIL (no `activity`).
- [ ] **Step 3: Implement.** The SQL is spec §4.2's, verbatim; `?2` is the kinds comma-wrapped (`",out,gate,refused,setting,proposal,job,task,process,"` when `kinds` is empty), `?3..?5` are `(i64::MAX, "", 0)` without a cursor, `?6` is `limit`. `settle_proposal` adds `settled_at = unixepoch()` to its `UPDATE`.
- [ ] **Step 4:** Run the tests, then `cargo test -p zephyr --lib` → PASS.
- [ ] **Step 5:** Commit `feat(activity): read every record of what ran, sent, waited and was refused in one paged query`.

### Task A4-2: Keep runs, tasks and processes 90 days

**Files:**
- Modify: `crates/zephyr/src/store.rs` (`prune` `:1761`), `crates/zephyr/src/retention.rs` (test `:341`), `DESIGN.md` §4.4 item 6, `README.md` (`zephyr status` row `:53`)

**Interfaces:**
- Consumes: nothing new. Produces: `prune` also deletes from `runs`, `tasks`, `procs` (counted in its return).

- [ ] **Step 1: Write the failing tests.**
  - `store.rs` `pruning_never_refires_a_one_shot_job_or_reuses_an_id` (Review Focus 1): a job with schedule `"2025-01-01T00:00:00Z"`; `claim_due(now, |s, a| scheduler::next_after(s, a, &TimeZone::UTC))` claims one run; `start_next_run`, `finish_run(…, "completed", …)`; a cron job with two finished runs; two finished tasks and two exited procs; set every `ended_at`/`due_at`/`created_at` to `now - 91 days`; `prune(now - 90 days)`. Assert: the one-shot run and the cron job's newer run remain, its older run is gone; `claim_due` now returns `[]`; the lower task and proc ids are gone, the highest stay; `add_task` gives `max + 1`, and `finish_task` on it puts a `task-<new id>` row in the inbox (`SELECT count(*) FROM inbox WHERE platform_msg_id = ?` is 1 with the new report text); likewise `add_proc` + `finish_proc` → a new `proc-<id>` inbox row.
  - `store.rs` `pruning_keeps_what_still_runs`: aged `claimed` and `running` runs, a `queued` and a `running` task, a `running` proc all stay.
  - `retention.rs` `the_sweep_prunes_state_db_past_ninety_days`: also seed two finished tasks, age the first; after `sweep` only the second remains (`store.tasks(i64::MIN)` or a direct count).
- [ ] **Step 2:** `cargo test -p zephyr --lib prun` → FAIL.
- [ ] **Step 3: Implement** spec §4.9's three `DELETE`s verbatim, after the existing statements (a failing one still `?`-returns; earlier deletes have run).
- [ ] **Step 4:** `cargo test -p zephyr --lib` → PASS.
- [ ] **Step 5:** Docs: DESIGN §4.4 item 6 says `runs`, `tasks` and `procs` are kept 90 days, each job's latest run and each table's highest id always; README's `zephyr status` row says the counts cover 90 days.
- [ ] **Step 6:** Commit `feat(retention): keep job runs, tasks and processes 90 days without ever refiring a job or reusing an id`.

### Task A4-3: Outbound commands as audit rows

**Files:**
- Modify: `crates/zephyr/src/scheduler.rs` (`audit` `:566-599`, tests near `:982`), `DESIGN.md` §4.4 item 3
- Test: `crates/zephyr/tests/router.rs` (tests asserting the whole audit after an outbound command, if any)

**Interfaces:**
- Produces: audit rows `(session, "outbound", label, outcome)`, one per `zephyr_policy::policy::outbound(command)` label, written in the `tool_execution_start` branch for `bash` or `exec`, `outcome` the call's own (`started`/`refused`), label through `vault::scrubbed` then `facts::scrubbed`. `rpc::Tally` and `alert()` unchanged.

- [ ] **Step 1: Write the failing tests** in `scheduler.rs` `mod tests`:
  - `an_outbound_command_is_an_audit_row_from_its_start`: events `bash` and `exec` with `{"command": "curl -X POST https://api.example.com/v1"}` → rows include `("main", "outbound", "curl POST api.example.com", "started")` twice, each after its call's own row.
  - `an_outbound_label_is_scrubbed`: a vault entry whose secret is `s3cr3tvalue` (as the vault tests build one) and the command `curl -d x https://s3cr3tvalue.example.com` → the label is `curl POST [vault:NAME].example.com` (the label keeps only the host, so the value must sit there).
  - `a_vetoed_outbound_command_is_recorded_refused`: `curl -d x https://e.x; rm -rf /` → `("main", "outbound", "curl POST e.x", "refused")`.
  - `a_plain_get_writes_no_outbound_row`: `curl -s https://e.x` → no `outbound` row.
- [ ] **Step 2:** `cargo test -p zephyr --lib scheduler::tests::outbound` etc. → FAIL.
- [ ] **Step 3: Implement** in `audit`: take the command from `ev["args"]["command"]` as `Tally::add` does (rpc.rs:533); write the extra rows with the same 500-byte cut and error log.
- [ ] **Step 4:** `cargo test -p zephyr` → PASS; fix any router test that lists the whole audit after an outbound command by adding the row (those filtering by tool, e.g. tests/router.rs:4490, stay as they are).
- [ ] **Step 5:** Docs: DESIGN §4.4 item 3: each command `policy::outbound` lists is also an `audit` row (`tool = 'outbound'`, its label as args) from the call's start; the notice is unchanged.
- [ ] **Step 6:** Commit `feat(audit): record each command that sends data off the machine when it starts, as the notice labels it`.

### Task A4-4: `/activity` on every owner channel

**Files:**
- Create: `crates/zephyr/src/activity.rs`
- Modify: `crates/zephyr/src/lib.rs` (`pub mod activity;`), `crates/zephyr/src/gate.rs` (`Scrub` `:460`: `pub(crate)` struct, `load`, `text`), `crates/zephyr/src/settings.rs` (`duration` `:663` → `pub(crate)`), `crates/zephyr/src/router.rs` (`command` `:2001`, beside `("/held", "")`), `DESIGN.md` (new §4.19), `README.md` (`/activity` row beside `/held` `:70`), `CLAUDE.md` (architecture list gains `activity.rs`)
- Test: `crates/zephyr/src/activity.rs` tests, `crates/zephyr/tests/router.rs`

**Interfaces:**
- Consumes: A4-1 `Store::activity`, `Record`, `Cursor`; `gate::Scrub::{load, text}`; `settings::{duration, one_line, clock}`; `scheduler::zone(cfg).unwrap_or(TimeZone::UTC)` (as `Router::clock`, router.rs:2720).
- Produces (W consumes `Query`, `Entry` and `answer`; A4-5 to A4-7 extend this file):
  ```rust
  pub const KINDS: [&str; 9] = ["out", "gate", "refused", "setting", "proposal", "job", "task", "process", "tool"];
  pub const USAGE: &str; // the usage line in Global Constraints
  pub enum Window { Today, Last(i64 /* secs, 3600..=90*86400 */) }
  pub struct Query { pub window: Window, pub kind: Option<&'static str> /* a KINDS entry or "grants" */,
                     pub before: Option<crate::store::Cursor>, pub limit: u32 }
  impl Query { pub fn words(arg: &str) -> Option<Query>; } // chat form: limit 30, before None
  #[derive(serde::Serialize)]
  pub struct Entry { pub at: i64, pub kind: String, pub subject: String, pub status: String,
                     pub text: String, pub line: String, pub stream: bool }
  pub fn since(w: &Window, now: i64, tz: &jiff::tz::TimeZone) -> i64;
  pub fn entry(r: &Record, scrub: &crate::gate::Scrub, now: i64, tz: &jiff::tz::TimeZone) -> Entry;
  pub fn chat(cfg: &Config, store: &Store, arg: &str, now: i64) -> String; // the whole reply
  ```
- Words (`Query::words`): 0–2 words, either order, ASCII-lowercased first; a time is `today`, or `Nh`/`Nd` parsed by `settings::duration` and within 1 h–90 d (`m`/`s` units refused); a kind is a `KINDS` entry, it plus `s`, `processes`, or `grants`. Two times, two kinds, or a third word → `None`. A time beside `grants` is accepted and ignored (the view is current state).
- Line: `WHEN KIND SUBJECT[ STATUS]: TEXT`. `WHEN` is `settings::clock` (`HH:MM`) when `at` falls on `now`'s local date, else `MM-DD HH:MM`. `STATUS` is left out when `started` and for `setting`; for kind `refused` it is `by the policy` (`refused`) or `by the gate` (`denied`). `TEXT` is `one_line(scrub.text(text), 200)`; `SUBJECT` of an `audit` row is `one_line(scrub.text(session), 40)`, other subjects `one_line(subject, 40)`. `stream` is `kind == "out" || kind == "tool" || status == "refused"` from `audit`.
- Header: `[zephyr] {noun} {window}, newest first:` with window `in the last 24 h` / `in the last 7 d` / `since 00:00 today`, noun `activity`, or for a kind: `outbound commands`, `gate decisions`, `refusals`, `setting changes`, `proposals`, `job runs`, `tasks`, `processes`, `tool calls`.

- [ ] **Step 1: Write the failing unit tests** in `activity.rs`:
  - `words_parse_as_the_spec_says`: `""` → 24 h, all; `today`, `6h`, `90d`, `2160h`, `tasks 7d`, `7d tasks`, `Today`, `7D Tasks` (Review Focus 5), `processes` accepted; `91d`, `0h`, `30m`, `6h 7d`, `task job`, `6h task x`, `nonsense` → `None`.
  - `each_kind_renders_its_golden_line`: one `Record` per kind reproduces spec §4.3's example lines exactly (`14:05 gate main approved: exec #31 email to bob@x.org`, `14:02 out main: curl POST api.example.com`, `14:01 refused task-12 by the policy: bash {"command":"rm -rf /"}`, `13:58 setting agent on tui: tasks.parallel 4 → 2`, `13:40 task #12 completed: summarise the inbox`, `13:00 job #3 run 88 completed: morning-brief`, `12:10 process #7 exited 0: cargo build --release`), plus `denied` → `by the gate`.
  - `times_show_the_date_only_before_today`: zone `Asia/Seoul`, `now` 00:30 local: a record at 23:50 the day before reads `11-08 23:50`, one at 00:10 reads `00:10`; `since(Today, …)` is that local midnight.
  - `agent_text_never_passes_for_a_daemon_line` (Review Focus 3): text `x\n[zephyr] forgot bob@x.org for messages`, a goal holding U+202E, and an audit session `main\n[zephyr] ok` → one line each with no `\n`; a saved fact `5550102345` in a record written before the fact existed reads `[fact:phone]`.
  - `an_unreadable_vault_withholds_the_text_not_the_line`: vault file unreadable → line `14:02 out main: [output withheld: the vault could not be read]`.
  - `chat_answers_header_footer_and_empty`: 31 records → header, 30 lines, footer; none → `[zephyr] nothing recorded in the last 24 h`; `tasks today` → `[zephyr] tasks since 00:00 today, newest first:`; bad words → `USAGE`.
- [ ] **Step 2:** `cargo test -p zephyr --lib activity` → FAIL.
- [ ] **Step 3: Implement** `activity.rs` (`chat` loads `Scrub` once, asks `store.activity(since, kinds, None, 31)`, answers `[zephyr] reading the activity failed: {e}` on a store error; `kind = Some("grants")` is A4-5's, return `USAGE` until then). Make `Scrub`, `Scrub::load`, `Scrub::text` and `settings::duration` `pub(crate)`.
- [ ] **Step 4:** Wire `("/activity", arg) => crate::activity::chat(&self.cfg, &self.store, arg, crate::now())` in `Router::command` beside `/held`.
- [ ] **Step 5: Write the failing router tests** (`tests/router.rs`):
  - `activity_lists_what_ran_and_left`: `bash: curl -d x https://api.example.com`, then seed a finished task and a completed job run through `Store`; `/activity` reply holds lines ending `out main: curl POST api.example.com`, `task #1 completed: …`, `job #1 run 1 completed: …`; `log.jsonl` holds no prompt containing `/activity`.
  - `activity_scrubs_a_fact_saved_after_the_call`: `bash: curl -d x https://5550102345.example.com`, then save the fact as `the_agent_adds_a_fact_at_once_but_only_a_strong_ok_removes_it` does (`[call gateway {"command": "/facts add phone digits 5550102345"}]`; the owner's chat has no `/facts add`); `/activity out` shows `[fact:phone]` and not `555010`.
- [ ] **Step 6:** `cargo test -p zephyr --test router activity_` → PASS, then `cargo test -p zephyr`.
- [ ] **Step 7:** Docs: DESIGN §4.19 Activity (sources and kinds, the query, the line, scrubbing at read, `stream`); README `/activity [today|6h|7d] [KIND]` row; CLAUDE.md's architecture list gains `activity.rs` (the record and grants views, rendered by the daemon).
- [ ] **Step 8:** Commit `feat(activity): answer /activity from the daemon's own tables, scrubbed as they are read`.

### Task A4-5: Grants, each with its revoke, and `/approvals forget N`

**Files:**
- Modify: `crates/zephyr/src/store.rs` (beside `remember_targets` `:1668`), `crates/zephyr/src/activity.rs`, `crates/zephyr/src/router.rs` (`approvals` `:2550`, `APPROVALS_USAGE` `:510`), `DESIGN.md` §4.16, `README.md` (`/approvals` row)
- Test: `crates/zephyr/src/activity.rs`, `crates/zephyr/src/store.rs`, `crates/zephyr/tests/router.rs`

**Interfaces:**
- Consumes: A4-4 `activity::chat`, `Query`; `Store::due_reverts(i64::MAX)` (all reverts, ordered by `at`); `Store::log_setting` (`:1285`).
- Produces:
  ```rust
  // store.rs
  pub struct KnownTarget { pub n: i64 /* rowid */, pub class: String, pub target: String, pub added_at: i64 }
  pub fn known_targets(&self) -> rusqlite::Result<Vec<KnownTarget>>;          // by class, target
  pub fn forget_target(&self, n: i64) -> rusqlite::Result<Option<(String, String)>>; // DELETE … RETURNING class, target
  pub fn enabled_jobs(&self) -> rusqlite::Result<Vec<(Job, i64 /* created_at */)>>;
  // activity.rs
  #[derive(serde::Serialize)]
  pub struct Grant { pub at: i64, pub subject: String, pub line: String, pub revoke: String }
  pub fn grants(cfg: &Config, store: &Store, now: i64) -> rusqlite::Result<Vec<Grant>>; // targets, reverts, jobs
  ```
- Lines (text after `·` is `revoke`): `{class} {target}, known since {MM-DD} · /approvals forget {n}`; `{key} until {WHEN}, then {value|none} · /set {key} {value|none}`; for `approvals.preset`, `then {snapshot}` and revoke `/set approvals.preset {first item, default → none}`; `job #{id} {name}, {schedule}, to {channel} · /job rm {id}`. Targets, names and values go through `Scrub::text` and `one_line(…, 200)`. Subject: the class, the key, `#id`. Empty view: `[zephyr] no standing grants`.
- `/approvals forget N` (any owner channel): `N` a positive `i64`, else `APPROVALS_USAGE`; logs `log_setting("approvals.known.<class>", Some(target), None, "owner", Some(channel), None)`; a failed log still forgets and appends `\n[zephyr] recording it in the activity failed: {e}`. `APPROVALS_USAGE` gains `, /approvals forget N`.

- [ ] **Step 1: Write the failing tests.**
  - `store.rs` `forgetting_a_target_returns_it_once`: remember two; `forget_target(n)` → `Some(("messages", "a@x.org"))`, again → `None`; `all_known` false for it, true for the other.
  - `activity.rs` `grants_list_each_with_its_revoke`: a known target, a revert of `budget.daily_cost_usd` to `5`, an enabled job, a disabled job → spec §4.4's three example lines exactly, no disabled job.
  - `activity.rs` `a_timed_presets_revoke_is_a_working_command` (Review Focus 2): a revert of `approvals.preset` valued `manual; messages=ask` → line contains `then manual; messages=ask`, revoke `/set approvals.preset manual`; valued `default; posts=hold` → revoke `/set approvals.preset none`.
  - router `forgetting_a_known_target_asks_again`: with `preset = "semi"`, `SEND_TO_A`, `/ok 1` (as `once_does_not_remember_and_ok_does`); `/activity grants` lists `messages a@x.org, known since … · /approvals forget 1`; `/approvals forget 1` → `[zephyr] forgot a@x.org for messages; under ask-new a message to it waits for you again`; `/activity setting` shows `approvals.known.messages a@x.org → none`; `SEND_TO_A` again is held (`held for the owner as #`); `/approvals forget 1` → the unknown text; `/approvals forget x` → usage.
- [ ] **Step 2:** Run them → FAIL.
- [ ] **Step 3: Implement** the store fns, `activity::grants`, the `grants` arm of `activity::chat`, and the `("forget", n)` arm in `Router::approvals` before the class arm.
- [ ] **Step 4:** `cargo test -p zephyr` → PASS.
- [ ] **Step 5:** Docs: DESIGN §4.16 `/approvals forget N` (stricter only, any owner channel, logged in `settings_log`); §4.19 the grants view; README `/approvals forget N` and `/activity grants`.
- [ ] **Step 6:** Commit `feat(approvals): list every standing grant with what takes it back, and forget a remembered recipient`.

### Task A4-6: The agent reads `/activity` through `gateway`

**Files:**
- Modify: `crates/zephyr-policy/src/tools.rs` (`gateway_kind` `:140`, `gateway`'s error text `:549`), `crates/zephyr/tests/fake-rpi.py` (`gateway_kind` `:130`), `crates/zephyr/src/router.rs` (`gateway_reply` `:983`, its `Gateway::Read` match), `crates/zephyr/src/memory.rs` (`TOOLS` gateway bullet `:158`), `DESIGN.md` §4.11
- Test: `crates/zephyr-policy/src/tools.rs` tests, `crates/zephyr/tests/router.rs`, `crates/zephyr/src/memory.rs` tests

**Interfaces:**
- Consumes: A4-4 `activity::chat`.
- Produces: `gateway_kind("/activity…")` is `Some(Gateway::Read)` when the whole command is ≤ 40 bytes, has 0–2 words after `/activity`, each `ascii_word`, none equal to `grants` or `grant` ignoring ASCII case; else `None`. The daemon answers it with `activity::chat`, the owner's text.
- AGENTS.md sentence (copy), appended to the gateway bullet: `When the owner asks what you did, sent or were refused, read gateway "/activity …" and give zephyr's record; do not answer from memory.`

- [ ] **Step 1: Write the failing tests.**
  - `zephyr-policy` `activity_is_a_read_but_grants_is_not`: `Some(Read)` for `/activity`, `/activity today task`, `/activity 7d`, `/activity Today`; `None` for `/activity grants`, `/activity GRANTS`, `/activity 7d Grants`, `/activity a b c`, `/activity tödåy`, a 41-byte command.
  - router `the_agent_reads_activity_but_not_grants`: prompt `[call gateway {"command": "/activity today"}]` → reply line `gateway: [zephyr] nothing recorded since 00:00 today` (the call's own audit row is kind `tool`, left out); `[call gateway {"command": "/activity grants"}]` → `tool error: … is not available to the agent`.
  - `memory.rs`: the gateway bullet contains the sentence.
- [ ] **Step 2:** Run → FAIL.
- [ ] **Step 3: Implement** the `"/activity"` arm in `gateway_kind`; the same rules in fake-rpi.py's `gateway_kind` (same commit); `activity if activity.starts_with("/activity") => crate::activity::chat(&self.cfg, &self.store, activity["/activity".len()..].trim(), crate::now())` in `gateway_reply`; mention `/activity` in `gateway`'s error text and in the `TOOLS` bullet's list of reads.
- [ ] **Step 4:** `cargo test -p zephyr-policy && cargo test -p zephyr` → PASS.
- [ ] **Step 5:** Docs: DESIGN §4.11 `gateway` reads `/activity …`, never `grants` (it names who passes without asking, what an injected run would aim at).
- [ ] **Step 6:** Commit `feat(gateway): let the agent read zephyr's activity record, never the standing grants`.

### Task A4-7: `zephyr.activity` on the attach socket

**Files:**
- Modify: `crates/zephyr/src/attach.rs` (`AttachRequest` `:48`, dispatch next to `zephyr.proposals` `:349`, module doc `:11-18`), `crates/zephyr/src/router.rs` (`attach` `:1274`), `crates/zephyr/src/activity.rs`, `DESIGN.md` §4.3.1
- Test: `crates/zephyr/tests/attach.rs`

**Interfaces:**
- Consumes: A4-4 `Query`, `Entry`, `entry`; A4-5 `grants`; A4-1 `Cursor`.
- Produces (W builds against these):
  ```rust
  impl Query { pub fn params(p: &serde_json::Value) -> Result<Query, String>; } // Err: the -32602 message
  pub fn answer(cfg: &Config, store: &Store, q: &Query, now: i64) -> Result<serde_json::Value, String>; // Err: -32000
  // attach.rs
  AttachRequest::Activity { query: crate::activity::Query, reply: oneshot::Sender<Result<Value, String>> }
  ```
  Router: `let _ = reply.send(crate::activity::answer(&self.cfg, &self.store, &query, crate::now()));`. Parsing stays in attach.rs, as every other method's `-32602` does.
- Request `params` (each optional; `null` = absent; wrong JSON type = `-32602`):
  `{"range": "today" | "<N>h" | "<N>d", "kind": "<KINDS entry>" | "grants", "before": "<cursor>", "limit": 1..200}` — `range` default `"24h"`, words read as `Query::words` reads them; `kind` absent = all but `tool`; `before` a previous `next`; `limit` default 50.
- Response, records:
  ```json
  {"since": 1762646400,
   "entries": [{"at": 1762693320, "kind": "out", "subject": "main", "status": "started",
                "text": "curl POST api.example.com",
                "line": "14:02 out main: curl POST api.example.com", "stream": true}],
   "next": "1762693320.audit.5512"}
  ```
  `next` is the last entry's cursor when more remain (`limit + 1` asked for), else `null`.
- Response, `kind: "grants"`: `{"since": null, "entries": [{"at": …, "kind": "grant", "subject": "messages", "line": "messages bob@x.org, known since 11-02 · /approvals forget 4", "revoke": "/approvals forget 4"}], "next": null}`.
- Errors: `-32602` with message `USAGE` for a bad `range`, `kind`, `before` (fails `Cursor::parse`) or `limit`; `-32000` `reading the activity failed: {e}` for a store error, `gateway is shutting down` when the router is gone.

- [ ] **Step 1: Write the failing tests** in `tests/attach.rs`:
  - `activity_pages_by_cursor`: seed five audit rows (two in one second) in the gateway's state.db; `zephyr.activity {"kind": "tool", "limit": 2}` three times following `next` → 5 distinct entries in `at` order, the last page's `next` is `null`; each `line` matches its `text`.
  - `activity_grants_carry_their_revoke`: a remembered target → one entry with `kind: "grant"` and `revoke: "/approvals forget 1"`, `next: null`.
  - `activity_refuses_bad_params`: `{"range": "91d"}`, `{"kind": "inbox"}`, `{"before": "1.inbox.1"}`, `{"limit": 0}`, `{"limit": 201}`, `{"limit": "5"}` → each `-32602`.
- [ ] **Step 2:** `cargo test -p zephyr --test attach activity_` → FAIL.
- [ ] **Step 3: Implement** `Query::params`, `answer`, the dispatch arm and the router arm.
- [ ] **Step 4:** `cargo test -p zephyr --test attach && cargo test -p zephyr` → PASS.
- [ ] **Step 5:** Docs: DESIGN §4.3.1 `zephyr.activity` (request, both responses, errors, `stream` meaning the row comes from rpi's forgeable event stream); attach.rs module doc lists it.
- [ ] **Step 6:** Commit `feat(attach): serve the activity record and the grants, paged, to attached clients`.

### Task A4-8: The TUI's Activity and Grants tabs

**Files:**
- Modify: `crates/zephyr/src/tui/settings_panel.rs` (`Tab` `:141`, `tabs()` `:541`, tab labels `:768`, cells `:776`, help `:1272-1283`), `crates/zephyr/src/tui/conn.rs` (`Ask` `:164`, request `:539`, `Pending` → `Msg` `:686`, `Msg` `:326`), `crates/zephyr/src/tui/overlays.rs` (`Action` `:611`), `crates/zephyr/src/tui/mod.rs` (`Msg::Proposals` `:413`, event reloads `:259-281`, action dispatch `:571-620`), `crates/zephyr/tests/tui.rs` (fake methods `:349-406`, shots `:1329`), `DESIGN.md` §4.3.2
- Create: `crates/zephyr/tests/screens/activity-100x30-dark.{txt,ansi}`, `grants-100x30-dark.{txt,ansi}` (by `ZEPHYR_UPDATE_SCREENS=1`)

**Interfaces:**
- Consumes: A4-7's request and response JSON.
- Produces:
  ```rust
  // conn.rs
  Ask::Activity { grants: bool, before: Option<String> } // zephyr.activity {"kind": "grants"?, "before": before}
  Msg::Activity { grants: bool, append: bool /* before was Some */, answer: Value }
  // -32601 → answer {"entries": [], "next": null, "old": true}; another error → {"error": why}
  // overlays.rs
  Action::ActivityPage(String) // ask for the page before this cursor
  Action::Revoke(String)       // send this command as typed on tui
  // settings_panel.rs
  enum Tab { Section(usize), Facts, Waiting, Activity, Grants } // tabs() pushes the two after Waiting
  pub fn load_activity(&mut self, v: &Value, append: bool);
  pub fn load_grants(&mut self, v: &Value);
  ```
- Behaviour: opening the panel sends `Ask::Activity` for both tabs beside `Ask::Proposals`. Activity draws each `line`, `kind == "tool"` muted; the cursor reaching the last row with a `next` returns `Action::ActivityPage(next)` once per cursor. Grants draws each `line`; `r` returns `Action::Revoke(revoke)`, which mod.rs sends as `Ask::Prompt` with the panel left open; a `zephyr_reply` whose `command` starts with `/approvals forget`, `/set ` or `/job rm` while the panel is open sends `Ask::Activity { grants: true, before: None }`. `old: true` shows `this zephyr keeps no activity record; zephyr upgrade`. Help and footer gain `("r", "Grants: take it back")`.

- [ ] **Step 1: Write the failing tests.**
  - `overlays.rs` `activity_mutes_tool_rows_and_asks_for_the_next_page_at_the_bottom`: load 3 entries (one `tool`) with `next: "5.audit.9"`; draw shows the three lines; `End` returns `Action::ActivityPage("5.audit.9")`, a second `End` returns `Action::None`; `load_activity(page2, true)` appends.
  - `overlays.rs` `r_on_a_grant_sends_its_revoke`: on Grants, `r` → `Action::Revoke("/approvals forget 4")`.
  - `overlays.rs` `an_old_daemon_gives_an_empty_activity_tab`: `{"entries": [], "next": null, "old": true}` → the old-daemon text.
  - `tests/tui.rs`: the fake answers `zephyr.activity` with made-up entries (and sends `"zephyr.activity grants"` / `"zephyr.activity"` to `sent`); `revoking_a_grant_reloads_the_tab`: open `/settings`, go to `Grants`, `r` → `next_sent()` is `/approvals forget 4`, the fake replies `zephyr_reply` with that command, then `next_sent()` is `zephyr.activity grants`; shots `("activity", "held", 100, 30, "dark", …)` and `("grants", …)`.
- [ ] **Step 2:** `cargo test -p zephyr --lib tui && cargo test -p zephyr --test tui` → FAIL.
- [ ] **Step 3: Implement**; then `ZEPHYR_UPDATE_SCREENS=1 cargo test -p zephyr --test tui screens_match_the_goldens` and read both new goldens before keeping them.
- [ ] **Step 4:** `cargo test -p zephyr` → PASS.
- [ ] **Step 5:** Docs: DESIGN §4.3.2 the Activity and Grants tabs and `r`.
- [ ] **Step 6:** Commit `feat(tui): show the activity record and the standing grants, each grant taken back with r`.

---

## Self-review notes

- Spec coverage: §4.1–4.2 → A4-1; §4.3 → A4-4; §4.4 → A4-5; §4.5 → A4-3; §4.6 → A4-6; §4.7 → A4-7; §4.8 → A4-8; §4.9 → A4-2; §4.10 failure texts are in A4-4/5/7 and A4-2's ordering; §3's DESIGN/README/CLAUDE.md edits sit in the task of their behaviour. Owner decisions: no schema (Global Constraints), vault grants left out (A4-5 lists three sources only), `/activity` readable by the agent but not `grants` (A4-6).
- Order: A4-1, A4-2, A4-3 are independent; A4-4 needs A4-1 (and A4-3 for its router test); A4-5 and A4-6 need A4-4; A4-7 needs A4-5; A4-8 needs A4-7.
