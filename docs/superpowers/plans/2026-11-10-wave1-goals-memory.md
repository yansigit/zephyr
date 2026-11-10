# Goals, daily review, memory v2 and persona (B) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Owner-set goals and persona in the agent's context, a memory change saved without asking only when the daemon saw that the run that made it was the owner's own, `/forget` and `/memory squash`, and a built-in daily review that can carry the memory digest.

**Architecture:** Goals and persona are two more files beside `RULES.md`, two registry rows applied through the router's `set_setting` and two AGENTS.md sections. Provenance is decided by the router from what it saw: a per-session taint in `session_origin`, the inbox rows a run took in, `rpc::Tally`'s judgement of each tool call, a database check for other writers, and a clean git base; `memory::commit_if_changed` takes the resulting `Origin` and approves under the memory lock only when the run made the one new commit on that base. The review is an ordinary job row marked `builtin = 'review'`, synced from `review.at`, whose run sends a daemon-composed block (goals, A4's activity, `memory::digest`) before an optional model check-in.

**Tech Stack:** Rust (MSRV 1.89, CI 1.95), tokio, rusqlite 0.41, toml_edit, jiff, git run as the agent (`memory::git`), Python `fake-rpi.py`.

**Spec:** `docs/superpowers/specs/2026-11-09-goals-memory-design.md` (its §6 "Decided with the owner" overrides the body). Wave index: `docs/superpowers/plans/2026-11-10-wave1.md`. A4's interface: `docs/superpowers/plans/2026-11-10-wave1-activity.md` (else `specs/2026-11-09-activity-design.md` §4.2–4.3).

## Global Constraints

- Everything under the wave index's **Global Constraints** applies (commit form and author, no trailers, failing test first, fmt/clippy/test before each commit, waits on events, fake-rpi mirror in the same commit, no secrets in logs, root rules, DESIGN.md and README.md in the same task).
- Branch `claude/goals-memory` in its own worktree, cut from `main` after A4 merges (B uses `Store::activity`). Task ids B-1 … B-10.
- Schema **v17**, exactly: `CREATE TABLE session_origin(session TEXT PRIMARY KEY, tainted TEXT, at INTEGER NOT NULL); CREATE TABLE memory_commits(sha TEXT PRIMARY KEY, at INTEGER NOT NULL, origin TEXT NOT NULL, auto INTEGER NOT NULL); ALTER TABLE jobs ADD COLUMN builtin TEXT; INSERT INTO session_origin(session, tainted, at) SELECT 'main', NULL, unixepoch() WHERE NOT EXISTS (SELECT 1 FROM inbox); PRAGMA user_version = 17;`
- Limits, verbatim: at most **20 goals**, each at most **500 characters**, whitespace collapsed; persona at most **4000 characters**, line breaks kept; the agent's proposals at most `CARD_CHARS` (**1500**); `/forget` text at least **3 characters**, matched case-insensitively with whitespace collapsed; the review's activity part `Store::activity(since, &[], None, 50)` cut at **3000 characters**; review `since` = the last completed review run's `due_at`, else **24 h** ago.
- `GOALS.md` and `PERSONA.md` live in `<state>`, written with `host::conf::replace_private` (0600), as `RULES.md` is.
- Every doubt lands on the waiting path: a missing or unreadable `session_origin` row, a failed taint write, an unknown tool, a git error, any check that cannot be made.
- `/forget`'s TEXT goes in no commit message, log line or table row. The persona's text never goes into `settings_log` (its rows say `set` / none).
- Memory commit messages stay `memory: <utc time>`; `memory_commits`, not the git log, is what the digest trusts.
- Reply and AGENTS.md texts quoted in this plan or the spec are copy: use them byte for byte.

## Review Focus

1. **A task report or an owner-sent file, read in the same conversation, then a clean-looking owner turn that edits MEMORY.md** (wave index item 3): it waits for `/memory ok`, never auto-approves. Tests `a_read_file_taints_the_conversation_for_good` and `a_task_report_taints_the_conversation` in B-6.
2. **A commit that only touched `notes/` (a job's or a tainted run's) before `/new`**: an owner's "remember X" afterwards still saves at once; `notes/` history must not leave HEAD ≠ `.head` blocking auto-approve forever (`/memory ok` answers "nothing waiting" there and cannot clear it). Tests `clean_base_ignores_notes_only_history` (B-5) and `a_note_committed_earlier_does_not_stop_a_save_after_new` (B-6).
3. **A persona holding Markdown headings** (`## Memory`, `## Skills`): the skills, connectors and subagents still land between `## Approvals` and the real `## Memory`, never inside the persona. Test `a_persona_with_headings_keeps_the_skills_after_approvals` in B-2.
4. **Moving `review.at` to a time earlier today than now**: no review is sent at once; the next one comes at the new time. Test `a_moved_job_counts_from_its_move` in B-8.
5. **`/forget TEXT` where TEXT is also in a `/rule`, a goal or the persona**: the reply names those places, since they stay in every prompt. Test `forget_names_the_rules_goals_and_persona_that_hold_it_too` in B-7.

## Decisions this plan makes where the spec is silent or wrong

1. **Clean base** (§4.3 item 5, §4.4): `clean_base` requires working files = approved and `git status --porcelain` empty, not HEAD = `.head`; at settle the run's commit must be the one new commit on that base (HEAD's parent = base). The spec's HEAD = `.head` check never holds after a notes-only commit (Review Focus 2).
2. **Other writers** (§4.3 item 4): `Store::writers_since(since)` (a job run, task or process running, or started at or after `since`) replaces `Wake::writers: AtomicU64`: one query, durable, no edits at three spawn sites, fails closed at second granularity.
3. **Strays**: a run is not owner-origin while `Procs::strays_alive()` finds an agent process outside every tracked group (a `setsid` left from an earlier conversation can write memory too). Cost for the coordinator to confirm with the owner: agent-browser's detached daemon counts, so after browser use auto-approve needs `/stop` first. Dropping it is one condition in `begin_run`. Strict version owner confirmed 2026-11-10.
4. **Daemon rows** (§4.3 item 2): platform ids starting `task-`, `proc-` or `action-` are the daemon's; every other `new` row was written by an adapter after the owner allowlist and is the owner's. ("Unknown platform id" in §4.8 has no other reading.)
5. **Clean mark**: `new_session` marks the new id clean, not `switch_main`, so `/switch` back never cleans.
6. **Clean list paths**: an absolute path is accepted only under `cfg.home`; `~`, `..`, `.` and any dot-named component (`memory/.git/config`, `.gitattributes`) taint.
7. **Taint reasons** are stored as the phrase after "which": `used exec at 14:02`, `took in task #7's report at 14:05`. Labels for the other failures: `your message, while other work of the agent's was running` (item 4), `your message, on top of a change still waiting` (item 5), `your message, but another run wrote memory meanwhile` (the settle race, spec).
8. **Review order**: the review job ships (B-8) before digest mode (B-9), so no build silences notices with no review to carry them.
9. **Review delivery**: the row is retargeted to `last_owner()` (`Store::retarget_job`) before each run, so the run's output and an interrupted run's report go where the block went. `claim_due` counts from `max(last due, created_at)` and a schedule change resets `created_at` (Review Focus 4).
10. **`zephyr.goals` `done` entries**: open goals come from `GOALS.md`; done ones are the `settings_log` rows `key = 'goal', new = 'done'` since `review::since`, with `n: null`.

---

### Task B-1: Store v17: conversation origin, memory commits, built-in jobs

**Files:**
- Modify: `crates/zephyr/src/store.rs` (`SCHEMA_VERSION` :7, `InboxRow` :17, `Job` :73, `JOB_COLUMNS`/`job_row` :86-104, migration after the `version < 16` step :755, `inbox_with_status` :961, `prune` :1761, migration test :3108)

**Interfaces:**
- Produces:
  ```rust
  pub struct InboxRow { /* … */ pub platform_id: String }      // every constructor (9) fills it
  pub struct Job { /* … */ pub builtin: Option<String> }        // add_job never sets it
  pub enum SessionOrigin { Clean, Tainted(String), Unknown }    // Unknown: no row
  pub struct MemoryCommit { pub sha: String, pub at: i64, pub origin: String, pub auto: bool }
  impl Store {
      pub fn mark_clean(&self, session: &str) -> rusqlite::Result<()>;          // INSERT OR IGNORE, tainted NULL
      pub fn taint(&self, session: &str, reason: &str) -> rusqlite::Result<()>; // upsert; the first reason is kept
      pub fn session_origin(&self, session: &str) -> rusqlite::Result<SessionOrigin>;
      pub fn writers_since(&self, since: i64) -> rusqlite::Result<bool>;        // runs/tasks running or started_at >= since; procs running or created_at >= since
      pub fn record_memory_commit(&self, sha: &str, origin: &str, auto: bool) -> rusqlite::Result<()>;
      pub fn memory_commits_since(&self, since: i64) -> rusqlite::Result<Vec<MemoryCommit>>; // oldest first
      pub fn clear_memory_commits(&self) -> rusqlite::Result<usize>;
  }
  ```
- `prune` also deletes `memory_commits` with `at < ?1`; `session_origin` rows are kept.

- [x] **Step 1: Write the failing tests** in `store.rs` `mod tests`:
  - Extend `older_databases_migrate_in_order_and_keep_their_rows`: `const V17: &str = "DROP TABLE session_origin; DROP TABLE memory_commits; ALTER TABLE jobs DROP COLUMN builtin;"` prepended to every undo, a `(16, "")` case; per version assert `session_origin("main") == Unknown` (the db had a row), `record_memory_commit("a1", "task #1", false)` then `memory_commits_since(0)` has it, `jobs()[0].builtin == None`.
  - `a_brand_new_database_starts_its_first_conversation_clean`: fresh open → `session_origin("main") == Clean`.
  - `a_taint_keeps_its_first_reason_and_a_clean_mark_never_cleans_it`: `taint("s", "used exec at 14:02")`, `taint("s", "used read at 14:03")`, `mark_clean("s")` → `Tainted("used exec at 14:02")`; `mark_clean("n")` → `Clean`; `"x"` → `Unknown`.
  - `writers_since_sees_running_and_recently_started_work`: a running proc → `true` for any `since`; once it ended with `created_at` 100, `writers_since(101) == false`, `writers_since(100) == true`.
  - `prune_drops_old_memory_commits` and `inbox_rows_carry_their_platform_id` (`insert_inbox` id `"task-3"` → `inbox_with_status("new")[0].platform_id == "task-3"`).
- [x] **Step 2:** `cargo test -p zephyr --lib store` → FAIL (no such functions/columns).
- [x] **Step 3:** Implement: the v17 step exactly as Global Constraints, `SCHEMA_VERSION = 17`, the functions above.
- [x] **Step 4:** `cargo test -p zephyr --lib store` → PASS; `cargo test -p zephyr` → PASS.
- [x] **Step 5:** Commit `feat(store): record where each conversation's text came from and who made each memory commit`.

Deviation: the `upgraded_from` mark stays in the v16 step (it means "upgraded from before the approvals gate"; moving it would write `preset = "full"` into a 0.7 install), so the migration test expects it only for versions below 16; `tests/router.rs` `old_install` also undoes v17.

### Task B-2: Goals and persona, set by the owner

**Files:**
- Modify: `crates/zephyr/src/memory.rs` (`prepare` :83, `agents_md` :183, `command` :352, beside `read_rules` :246 and `set_rule` :398; `APPROVALS` :181 becomes `pub(crate)`), `crates/zephyr/src/settings.rs` (`Apply` :93, rows after `rule` ~:348), `crates/zephyr/src/router.rs` (`command` :2020, `set_setting` :2278/:2326/:2408 and the log at ~:2460, `shown` :2628, `current` :2659, `set_command` :2519, `settings_set` :1359), `crates/zephyr/src/drafts.rs` (`insert` :984), `crates/zephyr/tests/router.rs`, `DESIGN.md` §4.6 and §4.17, `README.md`

**Interfaces:**
- Produces:
  ```rust
  // memory.rs
  pub(crate) fn read_goals(state: &Path) -> anyhow::Result<Vec<String>>;   // as read_rules
  pub(crate) fn read_persona(state: &Path) -> anyhow::Result<String>;      // "" when absent
  /// A goal or persona change: the owner's reply and what settings_log records.
  pub struct Change { pub reply: String, pub old: Option<String>, pub new: Option<String> }
  pub fn goal(state: &Path, arg: &str) -> Result<Change, String>;     // "add TEXT" | "done N" | "rm N"
  pub fn persona(state: &Path, arg: &str) -> Result<Change, String>;  // "set TEXT" | "clear"
  pub const GOAL_USAGE: &str = "usage: /goal add <text> | /goal done N | /goal rm N | /goals";
  fn agents_md(rules: &[String], goals: &[String], persona: &str, files: &Files) -> String;
  // settings.rs
  pub enum Apply { /* … */ Goal, Persona }
  ```
- Rows: `goal` (section `Goals and review`, label `Goals`, `Kind::Text { max: 500 }`, `Access::Propose`, not strong, `Stricter::NotOrdered`, `Apply::Goal`); `persona` (section `Memory`, label `Persona`, `Kind::Text { max: 4000 }`, same, `Apply::Persona`).
- Router: `Apply::Goal | Apply::Persona` join `Apply::Rule` in the `list` match (not timed), the proposal command (`/goal {raw}`, `/persona {raw}`), `current` (SHA-256 of the file via `drafts::digest`) and the apply arm (`memory::goal`/`memory::persona`, `self.restart = true`). Their `settings_log` old/new are the `Change`'s: add → (none, TEXT); done → (TEXT, `done`); rm → (TEXT, `removed`); persona set → (`set` or none, `set`); clear → (`set`, none). `shown("goal")` is `1 goal` / `N goals` / none; `shown("persona")` `set` / none.
- Commands: router `("/goal", arg)` when the first word is `add|done|rm`, `("/persona", arg)` when it is `set|clear`, both `owner_set(key, arg, from.0, |a| a)`. `memory::command` answers `/goals`, `/goal` alone or misshapen (`GOAL_USAGE`), `/persona` alone (the text, or `no persona; set one with /persona set <text>`) and `/persona` misshapen (`usage: /persona set <text> | /persona clear | /persona`). `/set goal|persona …` and `zephyr.settings_set` on them: `[zephyr] change goals with /goal and the persona with /persona` (own arm, existing texts unchanged).
- Replies (spec §4.1 table, §4.2): `[zephyr] goal 3 set; it applies from the next message`, `[zephyr] goal 3 done: TEXT`, `[zephyr] removed goal 3: TEXT`, `[zephyr] persona set; it applies from the next message`, `[zephyr] persona cleared; it applies from the next message`. Errors: `goal: no goal 7; /goals lists them`, `goal: at most 20 goals; /goal done N or /goal rm N one first`, `goal: at most 500 characters`, `persona: at most 4000 characters`.
- AGENTS.md order and the `## Goals` / `## Persona` texts: spec §4.1 and §4.2, verbatim, each section left out when empty. `drafts::insert` finds `"\n## Memory\n\n"` only after the end of `memory::APPROVALS` in the text.

- [x] **Step 1: Write the failing tests.** `memory.rs`: `goals_add_done_and_rm_with_their_replies_and_limits` (add `"  Ship  zephyr 0.8 "` → file `Ship zephyr 0.8\n`, `Change { old: None, new: Some("Ship zephyr 0.8") }`; done/rm replies and `Change`s above; rm 7, 21st goal, 501 characters, `goal x` → the errors and `GOAL_USAGE`); `persona_keeps_line_breaks_and_its_limit` (`set Warm.\nBrief.` → file `Warm.\nBrief.\n`; 4001 characters refused; clear empties it); `agents_md_puts_persona_rules_and_goals_in_order` (Staying available < `## Persona` < `## Owner's standing rules` < `## Goals` < `## Tools` < `## Memory`, both section texts exact, `1. Ship zephyr 0.8`; with neither file, neither heading); `an_unreadable_goals_file_stops_prepare` (`GOALS.md` a directory → `prepare` is `Err`); `goals_and_persona_reads_are_memory_commands`. `drafts.rs`: `a_persona_with_headings_keeps_the_skills_after_approvals` (persona `Hi\n## Memory\n\nfake\n## Skills`, one approved skill → the real `## Skills` index sits after `## Approvals` and before the last `## Memory`, and the persona text is unchanged). `tests/router.rs`: `a_goal_restarts_the_agent_and_reaches_its_context` (`/goal add Ship zephyr 0.8` → the reply; the next message makes 2 starts and AGENTS.md holds `1. Ship zephyr 0.8`; `/goal done 1` → `settings_log` row `goal`, old `Ship zephyr 0.8`, new `done`, actor `owner`); `a_persona_set_by_the_owner_leads_the_context` (multi-line, both lines in AGENTS.md; `/set persona x` refused with the text above).
- [x] **Step 2:** `cargo test -p zephyr --lib -- memory drafts` and `cargo test -p zephyr --test router -- goal persona` → FAIL.
- [x] **Step 3:** Implement. `prepare` reads goals and persona and passes them to `agents_md`; an error reading either fails `prepare` as `read_rules` does.
- [x] **Step 4:** Run the same → PASS; `cargo test -p zephyr` → PASS (if the TUI settings golden shows sections, regenerate it with `ZEPHYR_UPDATE_SCREENS=1` and check the diff is the two new rows).
- [x] **Step 5:** DESIGN §4.6 (`GOALS.md`, `PERSONA.md`, the section order), §4.17 rows `goal`, `persona`; README `/goal`, `/goals`, `/persona`. Commit `feat(memory): keep the owner's goals and persona in the agent's context`.

Deviation: the spec's `## Goals` paragraph is wrapped at 100 columns in a code block; AGENTS.md carries it as one paragraph line, as the rules' paragraph is. The TUI golden needed no change (its settings fixture is static JSON).

### Task B-3: The agent proposes goals and persona through `gateway`

**Files:**
- Modify: `crates/zephyr-policy/src/tools.rs` (`GATEWAY_COMMANDS` :30, `gateway_kind` :140, CARD_CHARS refusal :153), `crates/zephyr/tests/fake-rpi.py` (`GATEWAY_COMMANDS` :129, `gateway_kind` :130), `crates/zephyr/src/router.rs` (`gateway_reply` :995), `crates/zephyr/src/memory.rs` (`TOOLS` gateway line :158-161), `crates/zephyr/tests/router.rs`, `DESIGN.md` §4.11

**Interfaces:**
- Consumes: B-2's `Apply::Goal`/`Apply::Persona` through `agent_set` (key = the command word, so `goal`/`persona`).
- Produces: `GATEWAY_COMMANDS` gains `/goals`, `/persona`. `gateway_kind`: `/goal add TEXT` (`plain_text`), `/goal done N`, `/goal rm N` (N ASCII digits), `/persona set TEXT` (as `plain_text` but `\n` allowed), `/persona clear` are `Gateway::Propose`; `/goal …` and `/persona …` over `CARD_CHARS` are `None`. `/forget` and `/memory squash` stay `None`. `gateway_reply` sends `/goal ` and `/persona ` proposals to `agent_set`, as `/rule `.
- `TOOLS` line, copy: ``- **gateway** runs a read-only Zephyr command and returns its output in your turn:\n  `/status`, `/model`, `/switch`, `/rules`, `/goals`, `/persona`, `/memory diff`, `/memory log`, `/job list`. It can\n  also propose `/model PROVIDER/MODEL`, `/new`, `/switch ID`, `/pause`, `/reload`, `/goal add TEXT`, `/goal done N`\n  or `/persona set TEXT`: the owner is asked, and it runs only after their `/ok N`. Approving anything stays with the owner.`` (keep what A4 added to the block).

- [ ] **Step 1: Write the failing tests.** Policy: `gateway_reads_goals_and_persona_and_proposes_their_changes` (Read: `/goals`, `/persona`; Propose: `/goal add Run a 10k by June`, `/goal done 2`, `/goal rm 1`, `/persona set Warm.\nBrief.`, `/persona clear`; `None`: `/goal done two`, `/goal add`, `/goal add a\nb`, `/persona set a\u{202e}b`, `/persona set ` + 1490 × `x`, `/forget tea`, `/memory squash yes`). Router: `the_agent_proposes_a_goal_and_a_stale_card_is_refused` (`[call gateway {"command": "/goal add Run a 10k by June"}]` → owner card `[zephyr] the agent proposes #1: /goal add Run a 10k by June`; owner `/goal add Other`; `/ok 1` → the stale text, `GOALS.md` holds only `Other`; a second proposal `/ok 2` → it is goal 2); `the_agent_proposes_a_persona_shown_whole_on_its_card`.
- [ ] **Step 2:** `cargo test -p zephyr-policy gateway` and `cargo test -p zephyr --test router the_agent_proposes_a_` → FAIL.
- [ ] **Step 3:** Implement, with fake-rpi.py's mirror in the same commit.
- [ ] **Step 4:** Run the same → PASS; `cargo test` → PASS.
- [ ] **Step 5:** DESIGN §4.11. Commit `feat(gateway): let the agent read the goals and persona and propose changes to them`.

### Task B-4: `Tally` judges each of main's tool calls against the clean list

**Files:**
- Modify: `crates/zephyr/src/rpc.rs` (`Tally` :465, `Tally::add` :510, tests)

**Interfaces:**
- Produces:
  ```rust
  impl Tally { pub fn main(home: &Path) -> Tally }   // a private `home: Option<PathBuf>`; Default has none and never taints
  pub taint: Option<String>                          // the first reason, "used TOOL"
  ```
- Judged at `tool_execution_start` from `toolName` and `args.path` (the key `policy::check` reads, policy.rs:42). Clean: `read`/`edit` of exactly `memory/USER.md` or `memory/MEMORY.md`; `write` of `memory/<one or more normal components>`. The path is taken relative to `home` when absolute (else as given); `~`, `..`, `.` or a component starting with `.` taints. Any other call taints, success or not.

- [ ] **Step 1: Write the failing test** `main_taints_on_any_call_off_the_clean_list`: clean — `(read, memory/USER.md)`, `(edit, memory/MEMORY.md)`, `(read, <home>/memory/USER.md)`, `(write, memory/notes/2026-11-10.md)`, `(write, memory/x.md)`; each tainting alone — `(read, memory/notes/x.md)`, `(read, inbox/a.png)`, `(read, ../memory/USER.md)`, `(write, memory/../x)`, `(read, ~/memory/USER.md)`, `(write, memory/.git/config)`, `(write, memory)`, `(write, notes.md)`, `exec`, `process`, `gateway`, `recall`, `task`, `vault`, `mcp_call`, `ls`, `frobnicate`; `exec` then `gateway` → `taint == Some("used exec")`; an `isError` end changes nothing; `Tally::default()` with an `exec` → `None`.
- [ ] **Step 2:** `cargo test -p zephyr --lib rpc::tests::main_taints` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** `cargo test -p zephyr --lib rpc` → PASS.
- [ ] **Step 5:** Commit `feat(rpc): tell which of main's tool calls could have read text that is not the owner's`.

### Task B-5: Memory commits with an origin, approved at once only on a clean base

**Files:**
- Modify: `crates/zephyr/src/memory.rs` (`commit` :260, `commit_if_changed` :267, `commit_locked` :298, tests), `crates/zephyr/src/router.rs` (`settled` :1897), `crates/zephyr/src/scheduler.rs` (:254), `crates/zephyr/src/tasks.rs` (:95), `crates/zephyr/tests/router.rs` (:948, `a_job_run_commits_its_memory_edits` :1006)

**Interfaces:**
- Consumes: B-1 `Store::record_memory_commit`.
- Produces:
  ```rust
  pub enum Origin { Owner { base: String }, Main(String), Task(i64), Job { id: i64, run: i64, name: String } }
  impl Origin { pub fn label(&self) -> String }  // "your message" | the Main text | "task #7" | "job #3 (news) run 12"
  pub const LEGACY: &str = "a conversation from before zephyr kept track; /new starts a clean one";
  pub struct Committed { pub sha: Option<String>, pub auto: bool, pub origin: String, pub notice: Option<String> }
  pub fn commit_if_changed(home: &Path, state: &Path, origin: &Origin) -> anyhow::Result<Committed>;
  pub async fn commit(home: &Path, state: &Path, origin: Origin) -> anyhow::Result<Committed>;
  /// HEAD, when the working files equal the approved ones and nothing is uncommitted.
  pub fn clean_base(home: &Path, state: &Path) -> Option<String>;
  pub async fn base(home: &Path, state: &Path) -> Option<String>;   // on a blocking thread
  fn commit_locked(home: &Path) -> anyhow::Result<Option<String>>;  // the new commit's sha
  ```
- With `Owner { base }`, under the same lock: a commit was made and `rev-parse HEAD^` is `base` (and HEAD has one parent) → `approve`, `auto = true`, notice `[zephyr] memory saved from your message; /memory undo drops it:\n{diff}` (none when the diff is empty: a notes-only change); otherwise the waiting path with origin `your message, but another run wrote memory meanwhile`. Waiting notice: `[zephyr] memory change waiting (from {label}); /memory ok keeps it, /memory undo drops it:\n{diff}`. `Committed.origin` is the label recorded.
- Callers record `record_memory_commit(sha, &c.origin, c.auto)` when `sha` is `Some` (a failure is logged) and send `notice` as today. Here the router passes `Origin::Main(memory::LEGACY.into())`; B-6 replaces it. Jobs pass `Job { id, run, name }`, tasks `Task(id)`.

- [ ] **Step 1: Write the failing tests** in `memory.rs`: `an_owner_origin_change_is_saved_and_told` (prepare; `clean_base` is `Some`; USER.md gets `Takes coffee black`; `Owner { base }` → `auto`, notice exactly `[zephyr] memory saved from your message; /memory undo drops it:\nUSER.md\n+ Takes coffee black`, `sha` = HEAD; prepare → AGENTS.md has it; `/memory undo` → `[zephyr] undid …`, gone after prepare); `an_owner_origin_change_waits_when_another_commit_came_between` (a `Task(1)` commit after the base is taken → `auto == false`, notice starts `[zephyr] memory change waiting (from your message, but another run wrote memory meanwhile); /memory ok keeps it`); `the_waiting_notice_names_its_origin` (`(from task #7)`, `(from job #3 (news) run 12)`, `(from this conversation, which used exec at 14:02)`); `clean_base_is_none_while_a_change_waits_or_files_are_uncommitted`; `clean_base_ignores_notes_only_history` (Review Focus 2: a `Task(1)` commit of only `notes/x.md` → `clean_base == Some(HEAD)` though `.head` differs). Update the existing memory tests to pass an `Origin`. Router: :948 asserts `starts_with("[zephyr] memory change waiting (from a conversation from before zephyr kept track; /new starts a clean one); /memory ok keeps it")`; `a_job_run_commits_its_memory_edits` asserts `memory_commits_since(0)` holds origin `job #1 (mem) run 1`, `auto == false`.
- [ ] **Step 2:** `cargo test -p zephyr --lib memory` → FAIL.
- [ ] **Step 3:** Implement and update the three callers.
- [ ] **Step 4:** `cargo test -p zephyr --lib memory` and `cargo test -p zephyr --test router memory` → PASS; `cargo test -p zephyr` → PASS.
- [ ] **Step 5:** Commit `feat(memory): say where each memory change came from, and approve one from the owner's run at once`.

### Task B-6: The router decides when a run of main is the owner's

**Files:**
- Modify: `crates/zephyr/src/router.rs` (fields ~:440, `tally` init :588 and the takes :1903/:3725, `tapped` :725, `dispatch_new` :1824-1862, `settled` :1888, `reconcile` resume :3524, `compact` :3901, `new_session` :3111), `crates/zephyr/src/procs.rs` (`kill_strays` :193), `crates/zephyr/src/memory.rs` (the `## Memory` line in `agents_md` :211), `crates/zephyr/tests/router.rs`, `crates/zephyr/tests/cli.rs`, `DESIGN.md` §4.6, `README.md`, `CLAUDE.md` (the `memory.rs` bullet)

**Interfaces:**
- Consumes: B-1 store fns, B-4 `Tally::main`/`taint`, B-5 `Origin`, `memory::base`, `memory::commit`.
- Produces:
  ```rust
  // procs.rs
  pub fn strays_alive(&self) -> bool;   // kill_strays' search, no kill; false where kill_strays would sweep nothing
  // router.rs, private
  enum Trust { Owner { base: String, since: i64 }, Not(String) }  // field `trust`, plus `run_taint: bool`
  async fn begin_run(&mut self);        // sets run_start; replaces `self.run_start = Some(Instant::now())` at :1861, :3524, :3901
  fn taint_session(&mut self, reason: &str);  // `store.taint(session, "{reason} at {self.clock(now)}")` once per run, then trust = Not; a failed write is logged and trust is still Not
  fn daemon_row(platform_id: &str) -> Option<String>;  // "took in task #7's report" | "took in process #3's report" | "took in action #12's result"
  ```
- `begin_run` (before the `prompt` call): `Trust::Owner` only when the child's session is `SessionOrigin::Clean`, `!writers_since(now)` (nothing running or started this second; `now` is kept as `since`), `!procs.strays_alive()`, and `memory::base` is `Some`; else `Not` with label: Unknown → `LEGACY`; Tainted(r) → `this conversation, which {r}`; `writers_since(now)` or strays → `your message, while other work of the agent's was running`; no base → `your message, on top of a change still waiting`.
- `dispatch_new`: a `daemon_row` taints before the row is sent, prompt or steer. `tapped`: after `tally.add`, a new `tally.taint` calls `taint_session`. `settled`: `Owner { since }` with `writers_since(since)` or `strays_alive()` true becomes `Not("your message, while other work of the agent's was running")`; then `memory::commit` with `Origin::Owner { base }` or `Origin::Main(label)`. The tally is reset with `Tally::main(&self.cfg.home)`. `new_session` calls `store.mark_clean(&id)` before `switch_main`.
- `## Memory` text, copy (decision §6.2 included): `## Memory\n\nWhat you write to memory/ reaches this context only after the owner approves it. When the owner tells you something to remember and you only read and edit memory/USER.md and MEMORY.md, zephyr saves it at once. A rule the owner states in chat is not enforced until it is a /rule: say so, and offer to propose it with gateway "/rule TEXT".\n\n`

- [ ] **Step 1: Write the failing tests** in `tests/router.rs` (helper `fn taint(cfg: &Config) { Store::open(&cfg.db_path()).unwrap().taint("main", "used exec at 00:00").unwrap() }`, called before `start` in `a_memory_edit_waits_for_the_owner_who_sees_its_diff` and `gateway_never_approves_for_the_owner`; :948 then asserts `(from this conversation, which used exec at 00:00)`):
  - `after_new_an_owner_message_saves_memory_at_once`: `/new`, `WRITE_MEMORY likes tea` → a notice starting `[zephyr] memory saved from your message; /memory undo drops it:`; the next message's AGENTS.md holds `remembered:`; `memory_commits` last row origin `your message`, `auto`.
  - `a_read_file_taints_the_conversation_for_good` (Review Focus 1): `/new`; `[call read {"path": "inbox/a.png"}] look`; then `WRITE_MEMORY likes tea` → notice starts `[zephyr] memory change waiting (from this conversation, which used read at `; `session_origin` of the new id is `Tainted("used read at …")`.
  - `a_task_report_taints_the_conversation` (Review Focus 1): `/new`; `gw.say("task-99", "owner", "report: done")`; then `WRITE_MEMORY likes tea` → waiting `(from this conversation, which took in task #99's report at `.
  - `work_running_beside_the_run_keeps_its_change_waiting`: `slow` = `2`, `/job add bg | {NEVER} | hello`, `/job run 1`, wait for `run_status(&home, "running") == 1`, then `WRITE_MEMORY likes tea` → waiting `(from your message, while other work of the agent's was running)`.
  - `an_upgraded_conversation_waits_until_new`: delete `session_origin` rows with `rusqlite::Connection` before `start` → waiting `(from a conversation from before zephyr kept track; /new starts a clean one)`; after `/new` the next one saves.
  - `a_note_committed_earlier_does_not_stop_a_save_after_new` (Review Focus 2): with `taint`, write `memory/notes/x.md` by hand, send `hi` (its settle commits the note, no notice); `/new`; `WRITE_MEMORY likes tea` → `memory saved from your message`.
  - `tests/cli.rs` `a_stray_agent_process_keeps_an_owner_memory_change_waiting`: the subreaper harness of `stop_kills_the_agents_stray_processes_and_spares_the_live_child` (`stray` file), `/new`, `WRITE_MEMORY likes tea` → waiting `while other work of the agent's was running`.
- [ ] **Step 2:** `cargo test -p zephyr --test router -- memory taint` and `cargo test -p zephyr --test cli stray` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the same → PASS; `cargo test -p zephyr` → PASS.
- [ ] **Step 5:** DESIGN §4.6 provenance (the five checks with decisions 1–7, `session_origin`, the notices, the `## Memory` text); README one paragraph on when memory is saved without asking; CLAUDE.md's `memory.rs` bullet. Commit `feat(memory): save a memory change at once when the daemon saw it came only from the owner`.

### Task B-7: `/forget TEXT` and `/memory squash yes`

**Files:**
- Modify: `crates/zephyr/src/memory.rs` (`command` :352, new fns beside `undo` :457), `crates/zephyr/src/router.rs` (`command` :2001), `crates/zephyr/tests/router.rs`, `DESIGN.md` §4.6, `README.md`

**Interfaces:**
- Consumes: B-1 `record_memory_commit`, `clear_memory_commits`; B-2 `read_rules`/`read_goals`/`read_persona`; B-5 `commit_locked`.
- Produces:
  ```rust
  pub struct Forgot { pub reply: String, pub sha: Option<String> }   // sha None: nothing held the text
  pub fn forget_lines(home: &Path, state: &Path, text: &str) -> Result<Forgot, String>;
  pub async fn forget(home: &Path, state: &Path, text: &str) -> Result<Forgot, String>;   // blocking thread
  pub fn squash_history(home: &Path, state: &Path) -> Result<String, String>;            // Ok: squashed
  pub async fn squash(home: &Path, state: &Path) -> Result<String, String>;
  ```
- Router: `("/forget", text)` → on `sha` `Some`: `record_memory_commit(sha, "you (/forget)", true)`, `self.restart = true`, then `restart_if_idle`. `("/memory", "squash yes")` → `Ok` clears `memory_commits`. `memory::command` returns `None` for `/memory squash yes`, answers bare `/memory squash` with `[zephyr] /memory squash yes drops the memory's git history, keeping the files as they are now; /memory undo cannot go back past it.`, and its usage becomes `usage: /memory log | diff | ok | undo | squash`.
- Texts: spec §4.6 verbatim (refusal while waiting, nothing found, the forgot reply and its "Still kept" paragraph, the squash reply, squash refused while waiting with the same words as `/forget`'s but `then /memory squash yes`). Count word: `forgot 1 line` / `forgot N lines`. Under 3 characters: `[zephyr] usage: /forget TEXT (at least 3 characters) removes every memory line holding it`. When rules, goals or persona hold TEXT, one more line: `Also in your {rules (/rules), goals (/goals), persona (/persona)}, which /forget does not change.` naming only those that do.
- Squash: spec §4.6's steps, as the agent through `memory::git`; also delete `.git/ORIG_HEAD` and `.git/FETCH_HEAD` before `gc`.

- [ ] **Step 1: Write the failing tests** in `memory.rs`: `forget_removes_matching_lines_everywhere_and_undo_brings_them_back` (USER.md `Prefers green tea after 3pm`, `notes/2026-11-02.md` `bought  TEA at the shop`, MEMORY.md `coffee`, approved; `forget_lines("tea")` → reply starts `[zephyr] forgot 2 lines (commit `, holds `USER.md\n- Prefers green tea after 3pm` and `notes/2026-11-02.md\n- bought  TEA at the shop`; `git log --format=%B` has no `tea`; prepare → no `green tea`; `/memory undo` → both lines back); `forget_refuses_short_text_and_a_waiting_change` (`te`; a waiting change; `zzz` → `[zephyr] no memory line holds that`); `forget_names_the_rules_goals_and_persona_that_hold_it_too` (Review Focus 5: a rule `no tea after 3pm` and the persona `likes tea` → `Also in your rules (/rules), persona (/persona), which /forget does not change.`); `squash_leaves_one_root_commit_and_undo_stops_there` (`rev-list --all --count` = 1, `for-each-ref` lists one branch, `/memory undo` errs at the seed, refused while a change waits, bare `/memory squash` explains). Router: `forget_restarts_the_agent_and_records_its_commit` (`/forget likes tea` after a saved change → `[zephyr] forgot 1 line`; next message → 2 starts; last `memory_commits` origin `you (/forget)`, `auto`).
- [ ] **Step 2:** `cargo test -p zephyr --lib -- memory::tests::forget memory::tests::squash` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the same and `cargo test -p zephyr --test router forget` → PASS; `cargo test -p zephyr` → PASS.
- [ ] **Step 5:** DESIGN §4.6, README `/forget`, `/memory squash yes`. Commit `feat(memory): forget a text across the memory files, and drop the memory's history on request`.

### Task B-8: The built-in daily review

**Files:**
- Create: `crates/zephyr/src/review.rs`
- Modify: `crates/zephyr/src/lib.rs`, `crates/zephyr/src/config.rs` (`Config` :9, `validate` :345), `crates/zephyr/src/settings.rs` (row, `value` :845), `crates/zephyr/src/store.rs` (`claim_due` :1833), `crates/zephyr/src/memory.rs`, `crates/zephyr/src/scheduler.rs` (`run` :176-196 and :223-237, `command` :848, `list` :905), `crates/zephyr/src/main.rs` (`doctor` warnings :592), `crates/zephyr/tests/router.rs`, `DESIGN.md` §4.5 and §4.17, `README.md`, `CLAUDE.md` (architecture list: `review.rs`)

**Interfaces:**
- Consumes: A4's `Store::activity(since: i64, kinds: &[&str], before: Option<&Cursor>, limit: u32) -> rusqlite::Result<Vec<activity::Record>>` and the line renderer A4 uses for `/activity` (scrubbed with `gate::Scrub`; use A4's name as merged); B-1 `memory_commits_since`, `Job.builtin`; B-2 `read_goals`; `settings::clock`, `scheduler::zone`.
- Produces:
  ```rust
  // config.rs
  pub struct Review { pub at: Option<String> }          // Config.review: Option<Review>; "[review] at must be HH:MM, e.g. 21:30" (00-23:00-59)
  // store.rs
  pub fn builtin_job(&self, builtin: &str) -> rusqlite::Result<Option<Job>>;
  pub fn add_builtin_job(&self, builtin: &str, name: &str, schedule: &str, prompt: &str, channel: &str, recipient: &str) -> rusqlite::Result<i64>;
  pub fn set_job_schedule(&self, id: i64, schedule: &str) -> rusqlite::Result<()>;   // also created_at = unixepoch()
  pub fn retarget_job(&self, id: i64, channel: &str, recipient: &str) -> rusqlite::Result<()>;
  pub fn goals_done_since(&self, since: i64) -> rusqlite::Result<Vec<GoalDone>>;     // settings_log key 'goal', new 'done', oldest first
  pub struct GoalDone { pub text: String, pub actor: String, pub proposal: Option<i64>, pub at: i64 }
  // memory.rs
  /// One line per memory commit since `since`, then the waiting diff (marked shown); None: nothing.
  pub fn digest(home: &Path, state: &Path, store: &Store, since: i64, tz: &TimeZone) -> anyhow::Result<Option<String>>;
  // review.rs
  pub const PROMPT: &str;                                  // spec §4.5, verbatim
  pub fn sync(store: &mut Store, cfg: &Config) -> anyhow::Result<()>;
  pub fn since(store: &Store, now: i64) -> i64;            // last completed review run's due_at, else now - 86400
  pub fn block(cfg: &Config, store: &Store, since: i64, now: i64) -> Option<String>;   // None: no goals, no activity, no memory change
  pub enum Step { Run(String), Done, Failed(String) }
  pub fn prepare(cfg: &Config, store: &mut Store, run: &mut JobRun, now: i64) -> Step;
  pub fn warning(cfg: &Config, store: &Store) -> Option<String>;
  ```
- Row `review.at`: section `Goals and review`, label `Daily review`, `Kind::Optional(&Kind::Text { max: 5 })`, `Access::Propose`, not strong, `Stricter::NotOrdered`, `Apply::Toml`; `value` reads `cfg.review?.at`.
- `sync`: `review.at` none → `remove_job` the built-in row; `HH:MM` → schedule `M H * * *` (numbers without leading zeros), inserting `builtin = 'review'`, name `review`, prompt `PROMPT`, channel/recipient `last_owner()` or `""`, or `set_job_schedule` when it differs. Called at scheduler start and on each config change (:196); a failure is logged.
- `claim_due` dates a job from `max(coalesce(max(due_at), created_at), created_at)`.
- `prepare`: `last_owner()` none → `Failed("no owner to send the review to")`; else `retarget_job` and the in-memory `run.job` to it, `store.notify` the block when `Some`; no goals → `Done`; else `Run(format!("{PROMPT}\n\n{block}"))`. Scheduler: `Done` → `finish_run(completed, "[SILENT]", None)`; `Failed(w)` → `finish_run(failed, w, None)`; `Run(p)` → the usual `Spec` with `prompt: &p`.
- Block, copy: `[zephyr] daily review, {date "%a %-d %b"}` / `Goals` / `N. TEXT` lines or `no goals; add one with /goal add <text>` / `Done since the last review: TEXT (you, HH:MM), …` (actor `owner` → `you`, else `proposed as #N`; line left out when none) / `Activity since {settings::clock(since)}` / the activity lines or `nothing recorded`, cut at 3000 characters with `\n(cut; /activity shows it all)`, or `Activity: unavailable ({error})` / `Memory` / `digest` or `no changes`.
- Digest lines: `- HH:MM abc1234 saved, from your message` (auto) / `- HH:MM e4f5a6b from task #7`, then `Waiting (/memory ok keeps it, /memory undo drops it):` and the diff.
- `/job`: `list` shows `#5 review (built-in, review.at 21:30)`; `rm 5` and `ok 5` on it reply `[zephyr] job #5 is the daily review; /set review.at none turns it off`; `run 5` runs it.
- `warning`: `review.at` set and no built-in row → `review.at is set but the daily review job is missing; it is made again at the next start or config change`.

- [ ] **Step 1: Write the failing tests.** `store.rs`: `a_moved_job_counts_from_its_move` (Review Focus 4: a cron `30 21 * * *` job whose last run was due yesterday 21:30; at 07:00 `set_job_schedule(id, "0 6 * * *")` → `claim_due(07:00)` claims nothing, `claim_due(tomorrow 06:00)` claims it). `config.rs`: `a_review_time_must_be_hh_mm` (`21:30` ok; `25:00`, `9:30`, `21:5` refused with the text). `memory.rs`: `digest_lists_commits_with_their_origin_then_the_waiting_diff` (and `None` with nothing; after it `/memory ok` keeps at once). `review.rs`: `the_block_lists_goals_done_activity_and_memory`, `nothing_to_say_is_none`, `activity_is_cut_at_3000_characters`, `warning_names_a_missing_review_job`. `tests/router.rs`: `review_at_creates_moves_and_removes_the_builtin_job` (`/set review.at 21:30` → `/job list` holds `#1 review (built-in, review.at 21:30)`; `08:05` → schedule `5 8 * * *`; `none` → `[zephyr] no jobs`); `the_review_sends_the_daemons_block_then_the_check_in` (`hi`, `/goal add Ship zephyr 0.8`, `/set review.at 21:30`, `/job run 1` → a reply starting `[zephyr] daily review, ` holding `1. Ship zephyr 0.8`, then a reply holding `This is the owner's daily review`); `a_review_with_no_goals_calls_no_model` (no new `prompt` in `log(&home, "prompt")`, the run `completed`); `the_review_is_not_removed_or_approved_through_job`; `a_review_before_any_message_fails_and_sends_nothing`.
- [ ] **Step 2:** `cargo test -p zephyr --lib -- review store::tests::a_moved memory::tests::digest config` and `cargo test -p zephyr --test router review` → FAIL.
- [ ] **Step 3:** Implement; `doctor` prints `warn {w}` as for the backup.
- [ ] **Step 4:** Run the same → PASS; `cargo test -p zephyr` → PASS.
- [ ] **Step 5:** DESIGN §4.5 (the built-in job, its ledger and delivery, `/job` on it), §4.17 row `review.at`; README `review.at`; CLAUDE.md `review.rs`. Commit `feat(review): send the owner a daily review of goals, activity and memory at review.at`.

### Task B-9: `memory.notify = digest`

**Files:**
- Modify: `crates/zephyr/src/config.rs`, `crates/zephyr/src/settings.rs`, `crates/zephyr/src/memory.rs` (`commit_if_changed`, `commit`), `crates/zephyr/src/router.rs` (`settled`), `crates/zephyr/src/scheduler.rs` (:254), `crates/zephyr/src/tasks.rs` (:95), `crates/zephyr/tests/router.rs`, `DESIGN.md` §4.6 and §4.17, `README.md`

**Interfaces:**
- Consumes: B-8's review and `memory::digest`.
- Produces:
  ```rust
  pub struct MemorySettings { pub notify: Option<String> }   // Config.memory: Option<MemorySettings>, `[memory]`
  impl Config { pub fn digest(&self) -> bool }
  pub fn commit_if_changed(home: &Path, state: &Path, origin: &Origin, digest: bool) -> anyhow::Result<Committed>;
  pub async fn commit(home: &Path, state: &Path, origin: Origin, digest: bool) -> anyhow::Result<Committed>;
  ```
- Row `memory.notify`: section `Memory`, label `Memory notices`, `Kind::Choice(&["each", "digest"])`, `Access::Tighten`, not strong, `Stricter::Earlier` (`each` stricter), `Apply::Toml`; `value` defaults to `each`.
- `validate`: `[memory] notify must be each or digest`; digest without `[review] at` → `[memory] notify = "digest" needs [review] at: the digest goes in the daily review` (so `/set memory.notify digest` with no review and `/set review.at none` under digest are both refused).
- With `digest`, `commit_if_changed` neither marks shown nor returns a notice (an auto-approve still approves); callers pass `cfg.digest()`.

- [ ] **Step 1: Write the failing tests.** `memory.rs`: `digest_mode_tells_nothing_and_leaves_it_unshown` (`Task(1)` commit with `digest = true` → `notice == None`; `/memory ok` → `memory changed since you saw it`). `config.rs`: `digest_needs_a_review_time`. `tests/router.rs`: `digest_is_refused_without_a_review_in_either_order`; `in_digest_mode_the_review_lists_the_change_and_ok_keeps_it` (with `taint`; `/set review.at 21:30`, `/set memory.notify digest`, `WRITE_MEMORY likes tea` → no reply starts `[zephyr] memory`; `/job run 1` → the block holds `Memory\n- `, `from this conversation, which used exec at 00:00` and `Waiting (/memory ok keeps it, /memory undo drops it):`; `/memory ok` → `[zephyr] memory kept`).
- [ ] **Step 2:** `cargo test -p zephyr --lib -- memory config` and `cargo test -p zephyr --test router digest` → FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the same → PASS; `cargo test -p zephyr` → PASS.
- [ ] **Step 5:** DESIGN §4.6 digest mode, §4.17 row `memory.notify`; README `memory.notify`. Commit `feat(memory): list memory changes in the daily review instead of after each run, when the owner asks`.

### Task B-10: `zephyr.goals` for clients

**Files:**
- Modify: `crates/zephyr/src/attach.rs` (`AttachRequest` :48, dispatch beside `zephyr.settings` :349), `crates/zephyr/src/router.rs` (the attach request match), `crates/zephyr/tests/attach.rs`, `DESIGN.md` §4.3.1

**Interfaces:**
- Consumes: B-2 `read_goals`, B-8 `review::since`, `Store::goals_done_since`.
- Produces: `AttachRequest::Goals(oneshot::Sender<Result<Value, String>>)`; method `zephyr.goals`, behind `authenticate` like the others, read-only (changes go through the `/goal` commands as text). For W:
  ```json
  // request (params ignored; may be {} or absent)
  {"jsonrpc": "2.0", "id": 7, "method": "zephyr.goals", "params": {}}
  // response: open goals in file order, then those done since the last review, newest first
  {"jsonrpc": "2.0", "id": 7, "result": {"goals": [
      {"n": 1, "text": "Ship zephyr 0.8", "done": false},
      {"n": null, "text": "Renew passport", "done": true}]}}
  // errors: -32000 "gateway is shutting down" | -32000 "reading the goals failed: <error>"
  ```

- [ ] **Step 1: Write the failing test** `goals_lists_open_and_recently_done_goals` in `tests/attach.rs` (two goals added, one done through `/goal done 1` → exactly the shape above with the remaining goal as `n: 1`).
- [ ] **Step 2:** `cargo test -p zephyr --test attach goals` → FAIL (`-32601`).
- [ ] **Step 3:** Implement as a router round trip, as `zephyr.settings`.
- [ ] **Step 4:** `cargo test -p zephyr --test attach` → PASS; `cargo test` → PASS.
- [ ] **Step 5:** DESIGN §4.3.1. Commit `feat(attach): serve the owner's goals to clients`.
