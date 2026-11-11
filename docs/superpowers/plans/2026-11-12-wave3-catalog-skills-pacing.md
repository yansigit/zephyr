# Connector catalog, starter skills and browser pacing (D1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Six curated MCP connectors the owner adds with one command and whose secrets reach the agent only as stand-ins, four starter output skills seeded into `<state>/skills` from the image, and a per-site minimum gap between the agent's browser navigations and clicks, shared by every wrapper process.

**Architecture:** `catalog.rs` (new, pure) parses `deploy/connectors/NAME.toml` into an `[[mcp]]` table; the host CLI reads the entry through the container, the chat command through `Config::catalog_dir`, and both write through the existing `mcp add` / `drafts::write_config` paths. `drafts::seed` commits `/opt/zephyr/skills/*` into `<state>/skills` once per start, guided by `<state>/skills-seeded.json`. Pacing state lives in the gate (`gate::Pace`, one map for every connection); the wrapper (`browser_gate::paced`) names the command's registrable domain (the `psl` crate), asks the gate for a slot, closes the connection and sleeps before it takes its place on the desktop.

**Tech Stack:** Rust; `psl` 2.1 (new), `ring::rand` (present), `toml_edit`; the image's Chromium, `fonts-nanum`, `poppler-utils`, `markdown-it-py`, `uv run --with`; `github-mcp-server` 2.1.0, `npx` and `mcp-remote` 0.16.6.

**Spec:** `docs/superpowers/specs/2026-11-12-connectors-skills-pacing-design.md`. Its "Decided with the owner (2026-11-12)" section wins: pacing is on at 1500 ms; Python libraries for slides and charts are fetched on first use; Gmail and Google Calendar are left out; starter skills are seeded on an upgraded install at first start with one notice; remote catalog connectors run through `mcp-remote`.

## Global Constraints

The wave 2 index's Global Constraints (`2026-11-11-wave2.md`, and wave 1's, which it keeps) apply: commit style with no trailers, authored as the repo's configured user; failing test first; `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, the task's tests, then the full `cargo test` before each commit; event waits, never fixed sleeps; root's binaries by full path; published files in root-owned 0755 folders; DESIGN, README and CLAUDE.md in the same task as the behaviour; side containers only. Updated for wave 3:

- Branch `claude/catalog-skills-pacing`, cut from `origin/docs/wave3-specs`. main is at 0.9.0, rpi 0.21.2 (`/home/user/Projects/.always-on/rpi-0.21.2/bin/rpi`, ignored tests with `ZEPHYR_REAL_RPI=1`). No push, no PR.
- **No schema version** (`SCHEMA_VERSION` stays 19). New DESIGN sections: §4.22 Connector catalog, §4.23 Starter skills, §4.24 Browser pacing.
- **New crates:** `psl` only (2.1.x, MIT/Apache-2.0, the public suffix list compiled in). `cargo deny check advisories bans` after adding it.
- **No new top-level table in `zephyr.toml` unless the owner set its key** (the 0.7.0 lesson): `[browser]` appears only after `/set browser.pace_ms`; the catalog writes no `catalog` key; seeding writes nothing there.
- **Pinned, checked against the real releases on 2026-11-11** (`gh release`, `gh api`, `npm view`, PyPI JSON):
  - `github/github-mcp-server` v2.1.0: `github-mcp-server_Linux_x86_64.tar.gz` sha256 `a442f9db287cea01008bcb55fe6bc0292270301046c3edbd1a8b22d6e4157b2b`, `github-mcp-server_Linux_arm64.tar.gz` `34d5a268f65cd1e9b7107fa976f3571ccd575f9df00cec489fec79a039923a22` (the release's `github-mcp-server_2.1.0_checksums.txt`, and recomputed); the tarball holds `github-mcp-server` (29.4 MB), `LICENSE`, `README.md`.
  - `@notionhq/notion-mcp-server@2.6.1` (npm `latest`; `gitHead` cf16e5c in makenotion/notion-mcp-server, whose `package.json` there says 2.6.1; the repo's tags stop at v2.5.2), `@brave/brave-search-mcp-server@2.1.4` (npm `latest`, tag v2.1.4), `mcp-remote@0.16.6` (npm `latest`, tag v0.16.6 in geelen/mcp-remote).
  - PyPI: `matplotlib==3.11.2`, `python-pptx==1.0.2`; `markdown-it-py` 4.2.0 is already in `deploy/python/uv.lock`.
- **Exact copy:**
  - Pacing refusals: `not run: with browser pacing on, a batch takes at most one navigation or click; run the others as separate commands`; `zephyr: too many browser actions are queued for SITE; wait, then try again`.
  - Chat refusal off a strong channel: `adding a catalog connector decides where its secret goes; from tui, chat or passkey`.
  - Seed notice, once: `[zephyr] added the starter skills report, slides, pdf, charts; /skills lists them, /skill rm NAME removes one` (the names those seeded).
  - Doctor: `a newer catalog entry for NAME ships: zephyr mcp add --catalog NAME`.
  - AGENTS.md `## Connectors` gains: `Ready-made connectors are described in /opt/zephyr/connectors/; to suggest one, ask the owner to run /mcp catalog add NAME.`
- Side container: `ZEPHYR_CONTAINER=zephyr-test-d1`, image `zephyr-dev:d1-<sha>`, attach port 21790, web port 21793, a scratch HOME, the bridge network. Never `zephyr`, `~/.zephyr*` or the host's tailscale. Remove the container and its network afterwards; keep the image.

## Review Focus

1. **A catalog command connector's real secret never reaching the agent.** Test in Task 3: `drafts::tests::a_catalog_connectors_secret_reaches_the_agent_only_as_a_stand_in` (secrets.env `GITHUB_TOKEN=real-gh-token`, `brokered`; `mcp.json`'s `env.GITHUB_PERSONAL_ACCESS_TOKEN` is `broker::surrogate(key, "env", "GITHUB_TOKEN")`, the file holds `real-gh-token` nowhere, and the replies of `/mcp catalog github`, `/mcp catalog add github` and `/mcp` hold it nowhere). In the side container (Task 12) with a dummy secret: `mcp.json` and the server's `/proc/<pid>/environ` hold the stand-in only.
2. **A skill the agent edited never being trusted.** Test in Task 6: `drafts::tests::a_starter_skill_the_agent_edits_is_not_trusted` (an agent-written `~/.approved/skills/report/SKILL.md` and `skills/drafts/report/`; the next `prepare` restores the seeded file, AGENTS.md keeps the seeded description, a second `seed` commits nothing, and the draft waits for `/skill ok` with a diff against the seeded copy).
3. **Pacing holding across separate wrapper processes.** Test in Task 10: `cli::pacing_spaces_navigations_across_wrapper_processes` (a daemon with `[browser] pace_ms = 300`, eight wrappers at once, five on `a.example.com`/`www.example.com`, three on `other.test`; each gap within `example.com` ≥ 300 ms; `other.test`'s first command not behind them; lower bounds only).
4. **Pacing never making a takeover wait.** Test in Task 10: `cli::a_wrapper_sleeping_out_its_pace_never_delays_a_take` (a wrapper sleeping a long pace holds no shared lock on `desktop.lock`, so an exclusive `flock` succeeds at once; after the owner's hold it exits 1 with the owner-has-the-desktop text and the real binary never ran).
5. **An upgrade with no `[browser]` key leaving `zephyr.toml` byte-identical.** Test in Task 7: `router::an_upgrade_with_no_browser_key_leaves_zephyr_toml_as_it_is` (a 0.9.0-shaped `zephyr.toml` with connectors and no `[browser]`; the daemon starts, seeds starter skills from a test folder, runs a turn and a reload; the file's bytes are unchanged and `settings::value(cfg, "browser.pace_ms")` reads `1500`).

---

### Task 1: The catalog (`catalog.rs`, `deploy/connectors/`)

**Files:**
- Create: `crates/zephyr/src/catalog.rs` (`pub mod catalog;` in `lib.rs`), `deploy/connectors/{github,notion,linear,home-assistant,brave-search,context7}.toml`
- Modify: `crates/zephyr/src/config.rs` (hidden `catalog_dir: PathBuf`, default `/opt/zephyr/connectors`, kept by `keep_hidden`), `DESIGN.md` (§4.22, its first part)

**Interfaces (produces):**
```rust
pub const DIR: &str = "/opt/zephyr/connectors";
pub struct Secret { pub name: String, pub help: String }
pub struct Entry { pub name: String, pub about: String, pub plain_reason: Option<String>,
                   pub needs_url: bool, pub secrets: Vec<Secret>, server: Mcp }
pub fn parse(name: &str, text: &str) -> anyhow::Result<Entry>;   // deny_unknown_fields; name passes valid_name; `secrets` in [server] refused
impl Entry {
    /// `{url}` / `{host}` filled in `url`, `args`, `hosts`; Err without a URL when needs_url, with one otherwise.
    pub fn to_mcp(&self, url: Option<&str>) -> anyhow::Result<Mcp>;
    /// Whether `m` (the owner's connector of this name) runs what this entry ships (command, args, url).
    pub fn same_as(&self, m: &Mcp) -> bool;
}
pub fn read(dir: &Path, name: &str) -> anyhow::Result<Entry>;    // dir/NAME.toml after valid_name
pub fn list(dir: &Path) -> Vec<Entry>;                            // sorted; one that does not parse is logged and left out
```
Entries as spec §4.1.2: `github` (command `/usr/local/bin/github-mcp-server stdio`, `env.GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}"`, hosts `api.github.com, raw.githubusercontent.com`, the twelve `posts` classes); `notion` (`npx -y @notionhq/notion-mcp-server@2.6.1`, `env.NOTION_TOKEN = "${NOTION_TOKEN}"`, hosts `api.notion.com`, `API-post-search = "none"`); `linear` (`npx -y mcp-remote@0.16.6 https://mcp.linear.app/mcp --transport http-only --enable-proxy --header "Authorization: Bearer ${LINEAR_API_KEY}"`, hosts `mcp.linear.app`); `home-assistant` (`needs_url`, mcp-remote with `{url}`, `--allow-http`, hosts `{host}`, `HA_TOKEN`); `brave-search` (`npx -y @brave/brave-search-mcp-server@2.1.4`, `env.BRAVE_API_KEY`, `plain_reason`); `context7` (`url = "https://mcp.context7.com/mcp"`, no secret).

- [x] **Step 1: failing tests** in `catalog.rs`:
  - `every_shipped_entry_parses_and_validates`: for each file in `deploy/connectors/` (via `CARGO_MANIFEST_DIR/../../deploy/connectors`): parses; name valid; `to_mcp` (with `https://ha.example.com:8123` for `needs_url`) passes `Mcp::check` and `Config::validate` inside a config; no entry sets `secrets`; every `command` entry naming a secret has hosts or a `plain_reason`; no secret is `ZEPHYR_*`; `{url}` appears exactly in `needs_url` entries; there are six.
  - `an_unknown_key_and_a_secrets_key_are_refused`.
  - `to_mcp_fills_url_and_host_and_refuses_the_wrong_shape`: `home-assistant` with `https://ha.example.com:8123/api/mcp` gives hosts `ha.example.com:8123`; without a URL errors; `github` with a URL errors.
  - `same_as_tells_a_newer_entry`: changed args → false.
- [x] **Step 2:** `cargo test -p zephyr --lib catalog` fails (no module).
- [x] **Step 3: implement.** `[server]` deserialized as `Mcp` with `name` inserted from the file name before `serde` sees it.
- [x] **Step 4:** tests pass; full `cargo test`.
- [x] **Step 5:** commit `feat(catalog): ship six curated connectors with their hosts and gate classes preset`.

### Task 2: `zephyr mcp add --catalog` and `zephyr mcp catalog` on the host

**Files:**
- Modify: `crates/zephyr/src/host/setup.rs` (`mcp`), `crates/zephyr/tests/host.rs` (fake docker serves `/usr/bin/cat /opt/zephyr/connectors/NAME.toml` and `/usr/bin/ls /opt/zephyr/connectors` from `$SHIM_DIR/connectors/`), `README.md`

**Interfaces:** consumes `catalog::parse`, `Entry::to_mcp`.

- `--catalog NAME [--url URL] [--plain] [--host HOST]..`, before the `[add, name]` arm: `h.need_running()`, `h.exec(&[], &[], &["/usr/bin/cat", &format!("{}/{NAME}.toml", catalog::DIR)])` after `valid_name`; refuses a `plain_reason` entry or an `http://` URL without `--plain` with `NAME sends its key in a header zephyr does not swap: add --plain to hand it the real key, which the agent can then read` (the entry's `plain_reason` when it has one); merges with an existing connector (hosts kept unless `--host`; classes the entry's with the owner's winning per tool; `secrets` kept unless `--plain`); then the existing path (`creates_pair`, save, `apply`). Each `[[secret]]` that secrets.env lacks: interactive, its `help` printed and asked hidden; not interactive, `set it with: zephyr mcp add NAME --secret SECRET` printed. `zephyr mcp catalog`: `NAME: about` per entry.
- [x] **Step 1: failing tests** in `tests/host.rs`: `mcp_add_from_the_catalog_writes_the_entry_and_keeps_the_owners_choices` (github: table with command, env, hosts, `merge_pull_request = "posts"`; the output names `GITHUB_TOKEN` once; with the owner's `hosts` and `merge_pull_request = "none"` edited in, a second add keeps both); `catalog_entries_that_hold_their_key_in_plain_need_plain` (`brave-search`, and `home-assistant --url http://ha.lan:8123/api/mcp`, refused and nothing written; with `--plain` written with `secrets = "plain"`); `mcp_catalog_lists_the_entries`; a stopped container refuses.
- [x] **Step 2:** fails. **Step 3:** implement; README (`zephyr mcp add --catalog`, `zephyr mcp catalog`). **Step 4:** tests, full `cargo test`.
- [x] **Step 5:** commit `feat(mcp): add a catalog connector from the host with zephyr mcp add --catalog`.

### Task 3: `/mcp catalog` in chat and the TUI

**Files:**
- Modify: `crates/zephyr/src/drafts.rs` (`command`; `sections`' new sentence), `crates/zephyr/tests/router.rs`, `DESIGN.md` §4.6 bullet and §4.22, `README.md`

**Interfaces:** consumes `catalog::list/read`, `Config::catalog_dir`. Arms in `drafts::command`, before `(_, [verb, name])`:
- `/mcp catalog`: `- NAME: about` per entry, `(added)` when `cfg.mcp` has that name.
- `/mcp catalog NAME`: the `[[mcp]]` TOML it would write (`toml::to_string` of the table), its secrets with `help`, and `held as a stand-in, swapped only for HOSTS` / `held in plain: <plain_reason>` / `no secret`.
- `/mcp catalog add NAME [URL] [plain]`: `!may_approve` → `NEEDS_PASSKEY`; `!strong` → the exact refusal; else merged as on the host, written through `write_config`, `After::Reload`; reply `[zephyr] connector NAME added to zephyr.toml` plus, when secrets.env lacks a secret, `; it is left out until secrets.env sets X: run \`zephyr mcp add NAME --secret X\` on the host`.

- [x] **Step 1: failing tests:**
  - `drafts::tests::a_catalog_connectors_secret_reaches_the_agent_only_as_a_stand_in` (Review Focus 1).
  - `drafts::tests::catalog_add_needs_a_strong_channel_and_never_writes_plain_by_itself` (weak: the refusal and nothing written; strong `brave-search` without `plain`: refused; with `plain`: `secrets = "plain"`).
  - `router::mcp_catalog_add_from_discord_is_refused_and_from_chat_adds_the_connector` (cfg `catalog_dir` at a test folder with `github.toml`; Discord: refused; chat: the table is in `zephyr.toml`, the child restarts, the reply names `GITHUB_TOKEN`; with the secret written to secrets.env and `/reload`, `/mcp` lists `github` without `left out`).
- [x] **Step 2:** fail. **Step 3:** implement; the AGENTS.md sentence. **Step 4:** tests, full `cargo test`.
- [x] **Step 5:** commit `feat(mcp): add a catalog connector from a strong channel with /mcp catalog add`.

### Task 4: doctor

Runs after Task 6, which defines `drafts::STARTER`.

**Files:** Modify `crates/zephyr/src/main.rs` (doctor), `crates/zephyr/src/egress.rs` (`broker_reach` gets the two image folders), `tests/cli.rs`.

- One `warn` per connector whose name is a catalog entry and `!entry.same_as(m)`: `a newer catalog entry for NAME ships: zephyr mcp add --catalog NAME`. `__netcheck`'s third and fourth arguments (when given) add `the agent cannot write /opt/zephyr/connectors` and `…/skills` checks, by the same create/append probe; doctor passes `catalog::DIR` and `drafts::STARTER`.
- [x] **Step 1: failing test:** `catalog::tests::doctor_notes_a_newer_entry` on a pure `catalog::newer(cfg) -> Vec<String>` (the lines). **Step 2–4.** **Step 5:** commit `feat(doctor): say when a newer catalog entry ships, and check the image's catalog and skills are root's`.

### Task 5: The starter skills (`deploy/skills/`)

**Files:**
- Create: `deploy/skills/report/{SKILL.md,render.py,style.css}`, `deploy/skills/slides/{SKILL.md,make_pptx.py}`, `deploy/skills/pdf/SKILL.md`, `deploy/skills/charts/{SKILL.md,chart.py}`
- Modify: `deploy/python/pyproject.toml` (`markdown-it-py`), `deploy/python/uv.lock` (`uv lock`)

- `render.py IN.md OUT.pdf [--slides]`: markdown-it-py `commonmark` with `table` and `strikethrough`, `style.css` inlined (NanumGothic first, then DejaVu), local images resolved against IN's folder; `--slides` splits on lines that are exactly `---`, one `<section>` per slide with `@page { size: 13.333in 7.5in }`; prints with `$AGENT_BROWSER_EXECUTABLE_PATH --headless --no-sandbox --disable-gpu --user-data-dir=<mktemp> --no-pdf-header-footer --print-to-pdf=OUT file://<html>`. `make_pptx.py` and `chart.py` run under `uv run --with python-pptx==1.0.2` / `--with matplotlib==3.11.2`. Each SKILL.md: frontmatter `name`, one-line `description`; the steps; "deliver with `send_file`; never upload a result anywhere to share it".
- [x] **Step 1: failing test:** `drafts::tests::every_starter_skill_passes_the_approval_checks` (`read_skill` on each folder in `deploy/skills/`; four of them).
- [x] **Step 2–4.** Run `render.py` on the host once against a Korean markdown file with a table (Chromium from the host or the image, Task 12) where available.
- [x] **Step 5:** commit `feat(skills): ship report, slides, pdf and charts skills that write files and deliver them with send_file`.

### Task 6: Seeding (`drafts::seed`)

**Files:** Modify `crates/zephyr/src/drafts.rs`, `crates/zephyr/src/config.rs` (hidden `starter_skills: PathBuf`, default `drafts::STARTER` = `/opt/zephyr/skills`), `crates/zephyr/src/router.rs` (call at start; the notice), `DESIGN.md` §4.23, `README.md`.

**Interfaces (produces):**
```rust
pub const STARTER: &str = "/opt/zephyr/skills";
/// Commits each starter skill as spec §4.2.1 says; the names newly seeded (not updated).
pub fn seed(cfg: &Config) -> Vec<String>;
```
Record `<state>/skills-seeded.json` (`replace_private`, `{NAME: digest}`; `digest` over `SKILL` + each path and bytes, as `fingerprint` does for a skill). Commits `skill: seed NAME` and `skill: seed-update NAME`. Router: after `settle_remote_connectors`, `seed`; non-empty and meta `skills.seed_told` unset → meta `skills.seed_pending` = names; `upgrade_notice`'s tick sends the notice once there is an owner, then sets `skills.seed_told`.
- [x] **Step 1: failing tests** (`drafts::tests`): `seed_commits_each_starter_skill_once`; `a_removed_starter_skill_stays_removed_and_undo_brings_it_back`; `a_new_image_updates_an_untouched_copy_and_leaves_the_owners`; `a_name_the_owner_already_has_is_left_alone`; `a_starter_skill_that_fails_the_checks_is_not_seeded`; `a_starter_skill_the_agent_edits_is_not_trusted` (Review Focus 2). Router: `router::starter_skills_are_seeded_and_told_once` (two starts, one notice).
- [x] **Step 2–4.** **Step 5:** commit `feat(skills): seed the image's starter skills at start, once, and never again after the owner removes one`.

### Task 7: `browser.pace_ms` (config and registry)

**Files:** Modify `crates/zephyr/src/config.rs` (`pub browser: Option<Browser>`, `Browser { pace_ms: Option<u64> }` `deny_unknown_fields`; `Config::pace_ms() -> u64`, 1500 when unset; validate 0..=60000), `crates/zephyr/src/settings.rs` (row: key `browser.pace_ms`, section `Browser`, `Kind::Int { min: 0, max: 60000 }`, `Access::Tighten`, strong false, `Stricter::Higher`, `Apply::Toml`; `value`), `DESIGN.md` §4.17 row and §4.24, `README.md` (the setting and the upgrade note).
- [x] **Step 1: failing tests:** `config::tests::no_browser_table_reads_the_default_pace`; `settings::tests::the_pace_row_parses_and_the_agent_only_raises_it` (agent raising applies, lowering proposes); `router::set_browser_pace_writes_the_table_and_nothing_else_does`; `router::an_upgrade_with_no_browser_key_leaves_zephyr_toml_as_it_is` (Review Focus 5); `reload::tests::a_pace_change_is_live`.
- [x] **Step 2–4.** **Step 5:** commit `feat(browser): add the browser.pace_ms setting, 1500 ms unless the owner sets it`.

### Task 8: The gate's pace (`gate::Pace`)

**Files:** Modify `crates/zephyr/src/gate.rs`.

**Interfaces (produces):**
```rust
pub const PACE_MAX_WAIT: Duration = Duration::from_secs(120);
#[derive(Default)] pub struct Pace(Mutex<HashMap<String, Instant>>);
impl Pace {
    /// Reserves `site`'s next slot: Ok(wait) or Err(text). `r` in [0, 0.5).
    pub fn reserve(&self, site: &str, count: u32, pace: Duration, now: Instant, r: f64) -> Result<Duration, String>;
}
```
Wire: `{"v": 1, "pace": SITE, "count": N, "session": S}` → `{"verdict": "allow", "wait_ms": N}` or `{"verdict": "deny", "text": …}`, answered at once, unaudited; `Pace` made once in `serve` and shared by its `Conn`s; `pace` from the connection's `cfg.pace_ms()`; `r` from `ring::rand::SystemRandom`.
- [x] **Step 1: failing tests:** `pace_gaps_on_one_site_lie_in_pace_to_one_and_a_half`; `two_sites_do_not_wait_on_each_other`; `a_batch_of_two_and_a_slot_past_two_minutes_are_refused_and_reserve_nothing`; `pace_zero_reserves_nothing`; `passed_entries_are_dropped`; `the_gate_answers_a_pace_request_at_once` (over the socket, as the desktop tests do).
- [x] **Step 2–4.** **Step 5:** commit `feat(gate): reserve each site's next browser slot for every wrapper alike`.

### Task 9: The wrapper paces (`browser_gate::paced`)

**Files:** Modify `crates/zephyr/src/browser_gate.rs`, `crates/zephyr/Cargo.toml` (`psl = "2.1"`), `Cargo.lock`.

**Interfaces (produces):**
```rust
/// Where a command's site comes from, and how many paced actions it holds (batch counts).
pub enum Paced { Url(String), Current }
pub fn paced(rest: &[String]) -> Option<(Paced, u32)>;
/// The registrable domain of `url` (bare names read as https://); None for file:, about:, data:, chrome:.
pub fn site(url: &str) -> Option<String>;
```
In `decide`: after `held` and the gate's verdict and `taken`, before `unchanged` and `go`: `pace(env, site, count)` asks the gate (`ask_open` with a 5 s wait), drops the connection, sleeps `wait_ms`; `deny` refuses with its text; no gate or no answer runs unpaced. `Act::Pass` commands that `paced` names run `pace` then `go`; clicks the gate lets through at once, likewise.
- [x] **Step 1: failing tests:** `paced_reads_the_commands_in_the_spec_table` (`open example.com`, `goto`, `navigate`, `tab new --label x URL`, `read URL`, `read` alone, `back`, `reload`, `click @e1`, `find role button click`, `batch "open a.test" "open b.test"` → count 2, `snapshot` and `fill` → None); `sites_are_registrable_domains` (`www.amazon.co.uk` = `smile.amazon.co.uk`; `a.co.kr` and `b.co.kr` are two sites; `192.168.0.1`; `file:///x` → None); `no_gate_runs_a_navigation_at_once_and_still_refuses_a_click`; `a_paced_command_sleeps_what_the_gate_says`.
- [x] **Step 2–4;** `cargo deny check advisories bans`. **Step 5:** commit `feat(browser): space the agent's navigations and clicks on one site by the gate's slot`.

### Task 10: Pacing end to end

**Files:** Modify `crates/zephyr/tests/cli.rs`, `DESIGN.md` §4.11 bullet.
- [x] **Step 1: failing tests:** `cli::pacing_spaces_navigations_across_wrapper_processes` (Review Focus 3); `cli::a_wrapper_sleeping_out_its_pace_never_delays_a_take` (Review Focus 4).
- [x] **Step 2–4.** **Step 5:** commit `test(browser): pacing holds across wrapper processes and never delays a takeover`.

### Task 11: The image

**Files:** Modify `deploy/Dockerfile` (`tools` stage: `GITHUB_MCP_VERSION=2.1.0`, `GITHUB_MCP_SHA256_AMD64`, `GITHUB_MCP_SHA256_ARM64`, to `/usr/local/bin/github-mcp-server`; apt `fonts-nanum poppler-utils`; `COPY deploy/connectors/ /opt/zephyr/connectors/`, `COPY deploy/skills/ /opt/zephyr/skills/`, chmod 0755/0644), `.dockerignore`, `DESIGN.md` §4.11 command-line tools, `CLAUDE.md` (`catalog.rs`; `deploy/connectors/`, `deploy/skills/`).
- [x] **Step 1:** `every_copy_in_the_dockerfile_is_in_the_build_context` fails until `.dockerignore` lists both folders. **Step 2–4.** **Step 5:** commit `build(image): add github-mcp-server, Korean fonts, poppler and the catalog and skills folders`.

### Task 12: Image and side container

- [x] `docker build -t zephyr-dev:d1-<sha> -f deploy/Dockerfile --secret id=gh_token,env=GH_TOKEN .`; in it: `/opt/zephyr/connectors/*.toml` (6), `/opt/zephyr/skills/*/SKILL.md` (4), `github-mcp-server --version`, `fc-list | grep -i nanum`, `pdftotext -v`.
- [x] `zephyr-test-d1` (scratch HOME, bridge, attach 21790, web 21793): `zephyr mcp add --catalog context7` and an `mcp_list` through it; a catalog command connector with a dummy secret: `mcp.json` and the server's environment hold only the stand-in; `/skills` lists the four; two `agent-browser open` to one site, as the agent, are ≥ 1500 ms apart. Remove the container and network; keep the image.

### Task 13: Real rpi

- [x] `ZEPHYR_REAL_RPI=1 cargo test -p zephyr --test real_rpi -- --ignored`, `-p zephyr-policy --test rpi_load -- --ignored`, `-p zephyr --test attach -- --ignored`.

## Deviations

(Recorded here as they are made.)

- **Task 1.** Spec §4.1.2 says none of the `github` preset names hits the verb rule; two do (`add_comment_to_pending_review`, `update_issue_comment`). They stay preset, so the entry carries the gh gate's list whole; `catalog::tests::presets_change_only_what_the_verb_rule_gets_wrong` pins which two. Task 4's pure part (`catalog::newer`, the doctor lines) landed with Task 1, where `same_as` is.
- **Task 2.** Without a terminal (`h.interactive` false, as in tests) `zephyr mcp add --catalog` cannot ask for a secret, so it prints the secret's `help` and `set it in a terminal with: zephyr mcp add NAME --secret X`, and still writes the connector, left out until then. `--secret NAME` alongside asks for that one as before and is not asked twice.
- **Task 5.** `slides` renders its PDF with the `report` skill's `render.py --slides` (spec §4.2.2) rather than a copy of its own, so removing `report` breaks `slides`' PDF; its SKILL.md says it uses the report skill's renderer. The descriptions are quoted in the frontmatter (they hold `: `, which strict YAML refuses unquoted). Chromium's `--print-to-pdf` hangs inside this agent's sandbox on the host, so the renderer is checked in the image (Task 12); `make_pptx.py` and `chart.py` ran on the host.
- **Task 6.** The seed notice lists the names in sorted order (`charts, pdf, report, slides`), not the spec's `report, slides, pdf, charts`.
- **Task 3.** The router test cannot set the secret and `/reload`: a secrets.env change re-execs the daemon (`reload::diff`), which ends the test's gateway. It adds `github` (left out, the missing secret named) and `context7` (published, the child restarted for each); `drafts::tests::a_catalog_connectors_secret_reaches_the_agent_only_as_a_stand_in` covers a catalog connector published with its secret set.
- Task 7: `router::an_upgrade_with_no_browser_key_leaves_zephyr_toml_as_it_is` was written on the pacing branch before `drafts::seed` existed; it was made to seed a starter skill (and wait for its notice) when the two branches were joined.
- Task 9: `browser_gate::tests::a_navigation_passes_straight_through` now expects `back` to read the page's address (`get url`), as §4.3.2 says it must for its site; the test's fake gate (`T::gate`) answers pacing requests `allow` with no wait, so the tests that check the gate heard nothing still check exactly that (`T::gate_all` sees them).
- Task 10: `cli::a_wrapper_sleeping_out_its_pace_never_delays_a_take` runs the wrapper process against a gate the test serves, not the daemon's: a take is only reachable through the web app's passkey-signed `desktop` op. The daemon's side of a take is `gate.rs`'s and `desktop.rs`'s tests. Both Task 10 tests passed on their first run, the behaviour being Tasks 8 and 9's; with `pace_ms = 0` the first fails.
- Task 10: `cli::pacing_spaces_navigations_across_wrapper_processes` asserts that a site's k-th command ran at least (k − 1) × 300 ms after the wrappers were started, not that each two neighbours ran ≥ 300 ms apart. The gate's slots are ≥ pace apart and each wrapper sleeps at least to its slot, but a neighbour's own gap can come out shorter by however late the earlier wrapper was scheduled after its sleep: the pairwise form failed once in four full `cargo test` runs. The cumulative form is the bound the design guarantees, and a slow machine cannot break it. (Strictly pairwise spacing would need the wrapper to tell the gate when it ran; not done.)
- **How it was built.** Tasks 7–10 (pacing) ran in parallel in their own worktree (`claude/d1-pacing`) and were cherry-picked onto this branch after Task 4; the conflicts were DESIGN.md (§4.23 and §4.24 both kept) and this list.
- **Not done:** `linear`'s tool names were not listed live (no Linear key), so it presets no classes; rpi's own `mcp_list` through a catalog connector was not driven (it needs a model); the ignored real-container test `tests/token_broker.rs` was not extended with `ZEPHYR_TEST_GITHUB_TOKEN` / `ZEPHYR_TEST_LINEAR_KEY` cases, the side container checked the same by hand below.
- Review fix 11b: Task 9's deviation is undone. `back`, `forward` and `reload` first send a pacing check with the site empty, whose answer says whether pacing is on, so with `pace_ms = 0` they read nothing of the page and `browser_gate::tests::a_navigation_passes_straight_through` again expects `t.argv()` empty for them; with pacing on that costs one extra gate round trip before `get url`.

## Results (2026-11-11, image `zephyr-dev:d1-295cfa3`, container `zephyr-test-d1`, attach 21790, bridge)

- `cargo test --no-fail-fast`: 1466 passed, 0 failed, 22 ignored. `clippy --all-targets -D warnings` clean. `cargo deny check advisories bans`: advisories ok, bans ok (`psl` 2.1.251, `psl-types` 2.0.11). Flakes seen once each and passing on rerun, in code this lane did not touch: `browser_gate::tests::a_command_runs_in_a_place_the_take_waits_for`, `voice::tests::a_hung_whisper_is_killed_and_the_next_note_is_heard`, `cli::status_fails_without_a_daemon_or_with_a_stale_heartbeat` (400 s against 401 s).
- Real rpi 0.21.2 (`ZEPHYR_REAL_RPI=1`): `real_rpi` 6, `rpi_load` 10, ignored `attach` 1, all passed.
- Image: `/opt/zephyr/connectors` (6 entries) and `/opt/zephyr/skills` (4) root's, 0755/0644; `github-mcp-server --version` 2.1.0 (commit 4bcdcdf); 12 Nanum fonts in `fc-list`; `pdftotext` 24.02.0; markdown-it-py 4.2.0 in `/opt/zephyr/py`.
- Side container (scratch HOME, its own bridge network; the live `zephyr`, `~/.zephyr*` and tailscale untouched):
  - First start seeded `charts`, `pdf`, `report`, `slides` (`skill: seed …` commits, `skills-seeded.json` root 0600); `zephyr skill list` and AGENTS.md list them.
  - `zephyr mcp catalog` listed the six; `zephyr mcp add --catalog context7` wrote the table, restarted the agent and published `{"url": "https://mcp.context7.com/mcp"}` in `mcp.json`. As the agent, through the proxy, an MCP `initialize` and `tools/list` to it answered (Context7 4.5.0; `resolve-library-id`, `query-docs`).
  - `zephyr mcp add --catalog github` with a dummy `GITHUB_TOKEN` in secrets.env: `mcp.json` holds `zsur_135b…` and not the value; the server, run as the agent with that env, has only the stand-in in `/proc/<pid>/environ`; its `get_me` reached GitHub with the dummy swapped in (`401 Bad credentials`), audited `net|swap|GITHUB_TOKEN -> api.github.com:443|allowed`; `grep -r` over `/zephyr` finds no value.
  - Pacing at the default 1500 ms, as the agent: three `agent-browser open https://www.example.com/?n=…` in a row took 0.66 s, 1.49 s and 1.61 s (the first unpaced, the others waiting out their slot); two started together from separate processes to `iana.org` and `www.iana.org` finished 2.0 s apart.
  - The skills in the image: `report` rendered Korean Markdown with a table to a PDF whose `pdftotext` holds the Hangul; `slides` gave 3 pages at 960 × 540 pt; pypdf merged them into 4; `charts` (matplotlib fetched by uv) wrote a 1350 × 750 PNG; `make_pptx.py` (python-pptx fetched by uv) wrote a `.pptx`.
  - `zephyr doctor`: `ok the agent cannot write /opt/zephyr/connectors`, `… /opt/zephyr/skills`, no failure.
  - Container and network removed; the image kept.

## Follow-up: the review of PR #53 (2026-11-11)

Each finding was confirmed in the code first; failing tests came first.

- [x] **1. Plain carried over.** `catalog::merged` no longer copies an old table's `secrets`, and neither add path counts an old `plain` as consent: only `--plain` or the word `plain` in this add writes it. A connector that was plain and is re-added without it holds a stand-in, and the reply (host and chat) says `it held its real secret before; now it holds a stand-in …` (`catalog::lost_plain`). Tests: `drafts::tests::a_catalog_add_never_carries_plain_over_from_an_old_table` (an owner's plain `notion`; `/mcp catalog add notion` is not plain; an old plain `brave-search` is still refused), `host::catalog_entries_that_hold_their_key_in_plain_need_plain`.
- [x] **2. Old hosts won.** A catalog add's hosts are the entry's, `{host}` from this add's URL; an old table's are never kept (others are `/set mcp.NAME.hosts` from a strong channel). On the host, `--host` given in the same add still sets them: that is the owner's word in this add, not an old table's. Tests: `drafts::tests::a_catalog_add_binds_the_entrys_hosts_and_not_an_old_tables` (a re-added home-assistant binds its new URL's host; github drops an old `evil.test`), `host::mcp_add_from_the_catalog_writes_the_entry_and_keeps_the_owners_choices`.
- [x] **5. Raw HTML.** `render.py` uses `MarkdownIt("commonmark", {"html": False})` (it is the only renderer; `make_pptx.py` reads Markdown lines itself and emits no HTML) and starts Chromium with `--proxy-server=127.0.0.1:9`, `--proxy-bypass-list=<-loopback>`, `--host-resolver-rules=MAP * ~NOTFOUND`, `--disable-background-networking` and `--blink-settings=scriptEnabled=false`. `drafts::tests::every_starter_skill_passes_the_approval_checks` pins the flags in every renderer; the behaviour (a `<script>` and a remote `<img>`) is checked in the image.
- [x] **6. doctor under host networking.** The check moved out of `__netcheck` into `zephyr __unwritable DIR..`, run as the agent by `doctor` in the container whatever the network; `unwritable` walks the whole tree (links not followed). Test: `egress::tests::unwritable_tells_a_folder_the_prober_can_change` (a file and a folder three levels down).
- [x] **7. Crash after a seed-update.** Reconciled at the next start: an approved copy that already equals the image's is recorded as it is, untouched, so later images update it. Test: `drafts::tests::a_crash_between_a_seed_update_and_its_record_is_reconciled` (the commit made, the record not).
- [x] **8. Later skills seeded silently.** `seed` returns each skill it committed (`Seeded`: name, digest, updated); the router keeps them in `meta` `skills.seed_pending` and tells one line for the added and one for the updated, recording each skill's told copy in `skills.seed_told` (a per-skill JSON object). Test: `router::starter_skills_are_seeded_and_told_once` (a later image adds `slides` and changes `report`: both told, once). 7 and 8 are one commit: they change the same `seed` return and its one caller.
- [x] **9.** `plain_needed` reads the URL's parsed scheme (`HTTP://…` counts). In the test of 2.
- [x] **10.** Documented in DESIGN §4.22 as a known limit, not fixed.
- [x] **11, first part.** doctor's newer-entry note is only for connectors recorded in `<state>/catalog-added.json` (the daemon's, 0600; set by a catalog add on either path, cleared by `rm`, by a hand-written add or by `/mcp ok`). Tests: `catalog::tests::same_as_tells_a_newer_entry`, the host and drafts catalog tests.
- Findings 3, 4 and the second part of 11 (pacing) were done in parallel on `claude/d1-pace-fixes` and cherry-picked; see below.
- [x] **4. The pace map.** `gate::Pace` keeps the site map and an index of the same entries by moment (`Slots`): each request drops at most two passed entries and, at 4096 sites, the one due soonest, all O(log n) under the lock. Tests: `gate::tests::the_pace_map_is_capped`, `a_flood_of_pace_requests_does_not_slow_a_gated_request` (40 000 pace requests on random sites; a plain gated request answers within 10 s throughout).
- [x] **3. Pacing on the gated click path.** Before the gate is asked about a click, the wrapper sends a check (`"check": true`) that refuses a batch or a queue past 120 s and reserves nothing, so the owner never approves what pacing would refuse. A check, not an early reservation: a slot reserved before minutes of the owner's deciding would be spent by the time the click runs. After an `allow` that ended a wait, the slot is asked with `max_ms` 30 s (the gate refuses further), so 5 s to ask + 30 s + six 20 s re-reads stays under `REPORT_WAIT` (180 s); a refusal then is said as `{"ran": false, "paced": true}`, recorded and told as pacing's, not as a page that changed. Tests: `browser_gate::tests::a_click_pacing_would_refuse_never_reaches_the_owner`, `a_pacing_refusal_after_the_owners_allow_is_told_as_pacing`, `a_paced_sleep_after_an_allow_leaves_the_report_in_time`, and gate socket and ending cases.
- [x] **11, second part.** The check's answer says whether pacing is on (`"on"`); `back`, `forward` and `reload` read `get url` only when it is. With `pace_ms = 0` they read nothing of the page, as before pacing (`a_navigation_passes_straight_through`, and `cli` against the daemon with pace 0). This undoes Task 9's deviation for pace 0.
- **Found in the side container, fixed after:** (a) `--blink-settings=scriptEnabled=false` makes Chromium's headless `--print-to-pdf` write no file at all (bisected flag by flag), so the renderer keeps scripts and remote loads off with the page's own Content-Security-Policy (`default-src 'none'; img-src file: data:; style-src 'unsafe-inline'; font-src file: data:`) and keeps the network flags. (b) The host's write of `catalog-added.json` went through the container's `__file`, which allowed only its six files: the record is now one of `DAEMON_FILES`, and a failed write only warns (an older image's `__file` refuses the name), never failing an add whose config was already saved.

Results of the follow-up (images `zephyr-dev:d1-5822e19` and `zephyr-dev:d1-c105576`, kept; side container `zephyr-test-d1`, scratch HOME, its own bridge; container, network and HOME removed afterwards; the live `zephyr` untouched):
- `cargo test --no-fail-fast`: 1477 passed, 0 failed, 22 ignored. `clippy -D warnings` clean; `cargo deny check advisories bans` ok. Real rpi 0.21.2 (`ZEPHYR_REAL_RPI=1`): `real_rpi` 6, `rpi_load` 10, ignored `attach` 1. Known flakes, once each and passing on rerun: `cli::status_fails_without_a_daemon_or_with_a_stale_heartbeat` (400 s against 401 s), and on the pacing branch `tui::screens_match_the_goldens` and `browser_gate::tests::a_command_runs_in_a_place_the_take_waits_for`.
- Rendering, as the agent: Markdown holding `<script>…fetch("http://127.0.0.1:8765/script")</script>`, a raw `<img src="http://127.0.0.1:8765/raw-img">`, a Markdown image to the same listener and one to example.com printed to a PDF whose text shows the script and tag as text, with "Before." still there (the script did not run); a loopback listener saw only the test's own control request, none from Chromium.
- The catalog record: `zephyr mcp add --catalog context7` on the host wrote `["context7"]` to `/var/lib/zephyr/catalog-added.json` (root, 0600) through `__file`. A re-added `home-assistant` with a new URL bound `b.home.test` only.
- Pacing at 1500 ms: three opens in a row to one site and two at once from separate processes (2.3 s apart) were spaced as before; doctor: `ok the agent cannot write /opt/zephyr/connectors`, `… /opt/zephyr/skills`.
- Not checked in a container: doctor's image-folder check under host networking (its code no longer depends on the floor), and the 30 s ceiling actually slept after an owner's `allow`.
