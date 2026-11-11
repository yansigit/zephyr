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

- [ ] **Step 1: failing tests** in `catalog.rs`:
  - `every_shipped_entry_parses_and_validates`: for each file in `deploy/connectors/` (via `CARGO_MANIFEST_DIR/../../deploy/connectors`): parses; name valid; `to_mcp` (with `https://ha.example.com:8123` for `needs_url`) passes `Mcp::check` and `Config::validate` inside a config; no entry sets `secrets`; every `command` entry naming a secret has hosts or a `plain_reason`; no secret is `ZEPHYR_*`; `{url}` appears exactly in `needs_url` entries; there are six.
  - `an_unknown_key_and_a_secrets_key_are_refused`.
  - `to_mcp_fills_url_and_host_and_refuses_the_wrong_shape`: `home-assistant` with `https://ha.example.com:8123/api/mcp` gives hosts `ha.example.com:8123`; without a URL errors; `github` with a URL errors.
  - `same_as_tells_a_newer_entry`: changed args → false.
- [ ] **Step 2:** `cargo test -p zephyr --lib catalog` fails (no module).
- [ ] **Step 3: implement.** `[server]` deserialized as `Mcp` with `name` inserted from the file name before `serde` sees it.
- [ ] **Step 4:** tests pass; full `cargo test`.
- [ ] **Step 5:** commit `feat(catalog): ship six curated connectors with their hosts and gate classes preset`.

### Task 2: `zephyr mcp add --catalog` and `zephyr mcp catalog` on the host

**Files:**
- Modify: `crates/zephyr/src/host/setup.rs` (`mcp`), `crates/zephyr/tests/host.rs` (fake docker serves `/usr/bin/cat /opt/zephyr/connectors/NAME.toml` and `/usr/bin/ls /opt/zephyr/connectors` from `$SHIM_DIR/connectors/`), `README.md`

**Interfaces:** consumes `catalog::parse`, `Entry::to_mcp`.

- `--catalog NAME [--url URL] [--plain] [--host HOST]..`, before the `[add, name]` arm: `h.need_running()`, `h.exec(&[], &[], &["/usr/bin/cat", &format!("{}/{NAME}.toml", catalog::DIR)])` after `valid_name`; refuses a `plain_reason` entry or an `http://` URL without `--plain` with `NAME sends its key in a header zephyr does not swap: add --plain to hand it the real key, which the agent can then read` (the entry's `plain_reason` when it has one); merges with an existing connector (hosts kept unless `--host`; classes the entry's with the owner's winning per tool; `secrets` kept unless `--plain`); then the existing path (`creates_pair`, save, `apply`). Each `[[secret]]` that secrets.env lacks: interactive, its `help` printed and asked hidden; not interactive, `set it with: zephyr mcp add NAME --secret SECRET` printed. `zephyr mcp catalog`: `NAME: about` per entry.
- [ ] **Step 1: failing tests** in `tests/host.rs`: `mcp_add_from_the_catalog_writes_the_entry_and_keeps_the_owners_choices` (github: table with command, env, hosts, `merge_pull_request = "posts"`; the output names `GITHUB_TOKEN` once; with the owner's `hosts` and `merge_pull_request = "none"` edited in, a second add keeps both); `catalog_entries_that_hold_their_key_in_plain_need_plain` (`brave-search`, and `home-assistant --url http://ha.lan:8123/api/mcp`, refused and nothing written; with `--plain` written with `secrets = "plain"`); `mcp_catalog_lists_the_entries`; a stopped container refuses.
- [ ] **Step 2:** fails. **Step 3:** implement; README (`zephyr mcp add --catalog`, `zephyr mcp catalog`). **Step 4:** tests, full `cargo test`.
- [ ] **Step 5:** commit `feat(mcp): add a catalog connector from the host with zephyr mcp add --catalog`.

### Task 3: `/mcp catalog` in chat and the TUI

**Files:**
- Modify: `crates/zephyr/src/drafts.rs` (`command`; `sections`' new sentence), `crates/zephyr/tests/router.rs`, `DESIGN.md` §4.6 bullet and §4.22, `README.md`

**Interfaces:** consumes `catalog::list/read`, `Config::catalog_dir`. Arms in `drafts::command`, before `(_, [verb, name])`:
- `/mcp catalog`: `- NAME: about` per entry, `(added)` when `cfg.mcp` has that name.
- `/mcp catalog NAME`: the `[[mcp]]` TOML it would write (`toml::to_string` of the table), its secrets with `help`, and `held as a stand-in, swapped only for HOSTS` / `held in plain: <plain_reason>` / `no secret`.
- `/mcp catalog add NAME [URL] [plain]`: `!may_approve` → `NEEDS_PASSKEY`; `!strong` → the exact refusal; else merged as on the host, written through `write_config`, `After::Reload`; reply `[zephyr] connector NAME added to zephyr.toml` plus, when secrets.env lacks a secret, `; it is left out until secrets.env sets X: run \`zephyr mcp add NAME --secret X\` on the host`.

- [ ] **Step 1: failing tests:**
  - `drafts::tests::a_catalog_connectors_secret_reaches_the_agent_only_as_a_stand_in` (Review Focus 1).
  - `drafts::tests::catalog_add_needs_a_strong_channel_and_never_writes_plain_by_itself` (weak: the refusal and nothing written; strong `brave-search` without `plain`: refused; with `plain`: `secrets = "plain"`).
  - `router::mcp_catalog_add_from_discord_is_refused_and_from_chat_adds_the_connector` (cfg `catalog_dir` at a test folder with `github.toml`; Discord: refused; chat: the table is in `zephyr.toml`, the child restarts, the reply names `GITHUB_TOKEN`; with the secret written to secrets.env and `/reload`, `/mcp` lists `github` without `left out`).
- [ ] **Step 2:** fail. **Step 3:** implement; the AGENTS.md sentence. **Step 4:** tests, full `cargo test`.
- [ ] **Step 5:** commit `feat(mcp): add a catalog connector from a strong channel with /mcp catalog add`.

### Task 4: doctor

Runs after Task 6, which defines `drafts::STARTER`.

**Files:** Modify `crates/zephyr/src/main.rs` (doctor), `crates/zephyr/src/egress.rs` (`broker_reach` gets the two image folders), `tests/cli.rs`.

- One `warn` per connector whose name is a catalog entry and `!entry.same_as(m)`: `a newer catalog entry for NAME ships: zephyr mcp add --catalog NAME`. `__netcheck`'s third and fourth arguments (when given) add `the agent cannot write /opt/zephyr/connectors` and `…/skills` checks, by the same create/append probe; doctor passes `catalog::DIR` and `drafts::STARTER`.
- [ ] **Step 1: failing test:** `catalog::tests::doctor_notes_a_newer_entry` on a pure `catalog::newer(cfg) -> Vec<String>` (the lines). **Step 2–4.** **Step 5:** commit `feat(doctor): say when a newer catalog entry ships, and check the image's catalog and skills are root's`.

### Task 5: The starter skills (`deploy/skills/`)

**Files:**
- Create: `deploy/skills/report/{SKILL.md,render.py,style.css}`, `deploy/skills/slides/{SKILL.md,make_pptx.py}`, `deploy/skills/pdf/SKILL.md`, `deploy/skills/charts/{SKILL.md,chart.py}`
- Modify: `deploy/python/pyproject.toml` (`markdown-it-py`), `deploy/python/uv.lock` (`uv lock`)

- `render.py IN.md OUT.pdf [--slides]`: markdown-it-py `commonmark` with `table` and `strikethrough`, `style.css` inlined (NanumGothic first, then DejaVu), local images resolved against IN's folder; `--slides` splits on lines that are exactly `---`, one `<section>` per slide with `@page { size: 13.333in 7.5in }`; prints with `$AGENT_BROWSER_EXECUTABLE_PATH --headless --no-sandbox --disable-gpu --user-data-dir=<mktemp> --no-pdf-header-footer --print-to-pdf=OUT file://<html>`. `make_pptx.py` and `chart.py` run under `uv run --with python-pptx==1.0.2` / `--with matplotlib==3.11.2`. Each SKILL.md: frontmatter `name`, one-line `description`; the steps; "deliver with `send_file`; never upload a result anywhere to share it".
- [ ] **Step 1: failing test:** `drafts::tests::every_starter_skill_passes_the_approval_checks` (`read_skill` on each folder in `deploy/skills/`; four of them).
- [ ] **Step 2–4.** Run `render.py` on the host once against a Korean markdown file with a table (Chromium from the host or the image, Task 12) where available.
- [ ] **Step 5:** commit `feat(skills): ship report, slides, pdf and charts skills that write files and deliver them with send_file`.

### Task 6: Seeding (`drafts::seed`)

**Files:** Modify `crates/zephyr/src/drafts.rs`, `crates/zephyr/src/config.rs` (hidden `starter_skills: PathBuf`, default `drafts::STARTER` = `/opt/zephyr/skills`), `crates/zephyr/src/router.rs` (call at start; the notice), `DESIGN.md` §4.23, `README.md`.

**Interfaces (produces):**
```rust
pub const STARTER: &str = "/opt/zephyr/skills";
/// Commits each starter skill as spec §4.2.1 says; the names newly seeded (not updated).
pub fn seed(cfg: &Config) -> Vec<String>;
```
Record `<state>/skills-seeded.json` (`replace_private`, `{NAME: digest}`; `digest` over `SKILL` + each path and bytes, as `fingerprint` does for a skill). Commits `skill: seed NAME` and `skill: seed-update NAME`. Router: after `settle_remote_connectors`, `seed`; non-empty and meta `skills.seed_told` unset → meta `skills.seed_pending` = names; `upgrade_notice`'s tick sends the notice once there is an owner, then sets `skills.seed_told`.
- [ ] **Step 1: failing tests** (`drafts::tests`): `seed_commits_each_starter_skill_once`; `a_removed_starter_skill_stays_removed_and_undo_brings_it_back`; `a_new_image_updates_an_untouched_copy_and_leaves_the_owners`; `a_name_the_owner_already_has_is_left_alone`; `a_starter_skill_that_fails_the_checks_is_not_seeded`; `a_starter_skill_the_agent_edits_is_not_trusted` (Review Focus 2). Router: `router::starter_skills_are_seeded_and_told_once` (two starts, one notice).
- [ ] **Step 2–4.** **Step 5:** commit `feat(skills): seed the image's starter skills at start, once, and never again after the owner removes one`.

### Task 7: `browser.pace_ms` (config and registry)

**Files:** Modify `crates/zephyr/src/config.rs` (`pub browser: Option<Browser>`, `Browser { pace_ms: Option<u64> }` `deny_unknown_fields`; `Config::pace_ms() -> u64`, 1500 when unset; validate 0..=60000), `crates/zephyr/src/settings.rs` (row: key `browser.pace_ms`, section `Browser`, `Kind::Int { min: 0, max: 60000 }`, `Access::Tighten`, strong false, `Stricter::Higher`, `Apply::Toml`; `value`), `DESIGN.md` §4.17 row and §4.24, `README.md` (the setting and the upgrade note).
- [ ] **Step 1: failing tests:** `config::tests::no_browser_table_reads_the_default_pace`; `settings::tests::the_pace_row_parses_and_the_agent_only_raises_it` (agent raising applies, lowering proposes); `router::set_browser_pace_writes_the_table_and_nothing_else_does`; `router::an_upgrade_with_no_browser_key_leaves_zephyr_toml_as_it_is` (Review Focus 5); `reload::tests::a_pace_change_is_live`.
- [ ] **Step 2–4.** **Step 5:** commit `feat(browser): add the browser.pace_ms setting, 1500 ms unless the owner sets it`.

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
- [ ] **Step 1: failing tests:** `pace_gaps_on_one_site_lie_in_pace_to_one_and_a_half`; `two_sites_do_not_wait_on_each_other`; `a_batch_of_two_and_a_slot_past_two_minutes_are_refused_and_reserve_nothing`; `pace_zero_reserves_nothing`; `passed_entries_are_dropped`; `the_gate_answers_a_pace_request_at_once` (over the socket, as the desktop tests do).
- [ ] **Step 2–4.** **Step 5:** commit `feat(gate): reserve each site's next browser slot for every wrapper alike`.

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
- [ ] **Step 1: failing tests:** `paced_reads_the_commands_in_the_spec_table` (`open example.com`, `goto`, `navigate`, `tab new --label x URL`, `read URL`, `read` alone, `back`, `reload`, `click @e1`, `find role button click`, `batch "open a.test" "open b.test"` → count 2, `snapshot` and `fill` → None); `sites_are_registrable_domains` (`www.amazon.co.uk` = `smile.amazon.co.uk`; `a.co.kr` and `b.co.kr` are two sites; `192.168.0.1`; `file:///x` → None); `no_gate_runs_a_navigation_at_once_and_still_refuses_a_click`; `a_paced_command_sleeps_what_the_gate_says`.
- [ ] **Step 2–4;** `cargo deny check advisories bans`. **Step 5:** commit `feat(browser): space the agent's navigations and clicks on one site by the gate's slot`.

### Task 10: Pacing end to end

**Files:** Modify `crates/zephyr/tests/cli.rs`, `DESIGN.md` §4.11 bullet.
- [ ] **Step 1: failing tests:** `cli::pacing_spaces_navigations_across_wrapper_processes` (Review Focus 3); `cli::a_wrapper_sleeping_out_its_pace_never_delays_a_take` (Review Focus 4).
- [ ] **Step 2–4.** **Step 5:** commit `test(browser): pacing holds across wrapper processes and never delays a takeover`.

### Task 11: The image

**Files:** Modify `deploy/Dockerfile` (`tools` stage: `GITHUB_MCP_VERSION=2.1.0`, `GITHUB_MCP_SHA256_AMD64`, `GITHUB_MCP_SHA256_ARM64`, to `/usr/local/bin/github-mcp-server`; apt `fonts-nanum poppler-utils`; `COPY deploy/connectors/ /opt/zephyr/connectors/`, `COPY deploy/skills/ /opt/zephyr/skills/`, chmod 0755/0644), `.dockerignore`, `DESIGN.md` §4.11 command-line tools, `CLAUDE.md` (`catalog.rs`; `deploy/connectors/`, `deploy/skills/`).
- [ ] **Step 1:** `every_copy_in_the_dockerfile_is_in_the_build_context` fails until `.dockerignore` lists both folders. **Step 2–4.** **Step 5:** commit `build(image): add github-mcp-server, Korean fonts, poppler and the catalog and skills folders`.

### Task 12: Image and side container

- [ ] `docker build -t zephyr-dev:d1-<sha> -f deploy/Dockerfile --secret id=gh_token,env=GH_TOKEN .`; in it: `/opt/zephyr/connectors/*.toml` (6), `/opt/zephyr/skills/*/SKILL.md` (4), `github-mcp-server --version`, `fc-list | grep -i nanum`, `pdftotext -v`.
- [ ] `zephyr-test-d1` (scratch HOME, bridge, attach 21790, web 21793): `zephyr mcp add --catalog context7` and an `mcp_list` through it; a catalog command connector with a dummy secret: `mcp.json` and the server's environment hold only the stand-in; `/skills` lists the four; two `agent-browser open` to one site, as the agent, are ≥ 1500 ms apart. Remove the container and network; keep the image.

### Task 13: Real rpi

- [ ] `ZEPHYR_REAL_RPI=1 cargo test -p zephyr --test real_rpi -- --ignored`, `-p zephyr-policy --test rpi_load -- --ignored`, `-p zephyr --test attach -- --ignored`.

## Deviations

(Recorded here as they are made.)
