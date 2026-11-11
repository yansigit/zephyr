# Distribution (E) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** zephyr can be built, installed and run by someone other than this owner, with nothing published and every default unchanged: `ZEPHYR_REPO`, an image without the owner's provider sources, `deploy/install.sh`, and `zephyr init` covering the iMessage forward, the web app and `--network`.

**Architecture:** The CLI's image repository becomes `host::repo()` (env, then build time, then `DEFAULT_REPO`); `pull` gives the `gh` token to ghcr.io only. The Dockerfile reads an empty provider-repo arg as "leave it out". `deploy/install.sh` is a checkout-local bash installer that refuses an existing install. `zephyr init` gains three steps in `host/setup.rs`, sharing `tailscale serve` handling with `host/web.rs`. No schema change, no new crate.

**Tech Stack:** Rust (MSRV 1.89, CI 1.95), bash, Docker BuildKit, GitHub Actions, dialoguer, the `tests/host.rs` docker/gh/tailscale shims.

**Spec:** `docs/superpowers/specs/2026-11-11-distribution-design.md`. Its closing "Decided with the owner (2026-11-11)" section wins: every open question takes the recommendation (1a: no CLI switch for the minimal image; 2b: tag the pin, an owner action recorded here, not done by this lane; 3a: the manual `providers: none` input; 4a: install.sh refuses an existing install; 5a: the forward question defaults to yes). Wave index: `docs/superpowers/plans/2026-11-11-wave2.md`.

## Global Constraints

- The wave index's and wave 1's Global Constraints apply in full: commits `type(scope): plain sentence`, authored as the repo's configured user, no trailers; failing test first; `cargo fmt`, `cargo clippy --all-targets -- -D warnings`, the task's tests, then the full `cargo test` before each commit; DESIGN and README in the same task as the behaviour.
- Branch `claude/distribution` from `origin/docs/wave2-specs`; task ids E-1 … E-8; **no schema change**; no new crate.
- **Defaults never move:** `DEFAULT_REPO = "ghcr.io/yansigit/zephyr"` (no trailing `:`); `RPI_PACKAGE_REPO`, `PLUGINS_REPO` and their REFs keep today's values; `zephyr up --build` passes no `--build-arg`; image.yml still pushes `ghcr.io/yansigit/zephyr:<v>` and `:latest` from a tag.
- Side containers only: any container this lane runs is named `zephyr-test-e` (attach 17890, web 17893), or is a `docker run --rm`. Never `zephyr`, never `zephyr-test`, never `~/.zephyr*`. Images are tagged `zephyr-dev:e-<sha>` and `zephyr-dev:e-<sha>-minimal`, removed after the check. No push, no PR.
- Copy, verbatim: install refusal `zephyr is installed here; upgrade with zephyr upgrade`; forward question `Forward this machine's Tailscale port {P} to the webhook?`; web questions `Change the web app?` / `Set up the web app (needs Tailscale with HTTPS certificates)?`; network lines `network: bridge (the agent's network floor)` / `network: host (no network floor; zephyr init --network bridge turns it on)`; hints `add a channel: zephyr channel add discord|imessage`, `web app: zephyr web setup`; doctor `warn {path} does not exist: no child can load it (the CLI and the image are out of step, or the image was built without it)`.

## Review Focus

1. **The gh token reaching a registry that is not ghcr.io** (`ZEPHYR_IMAGE=registry.example.com/x`, `user/zephyr` on Docker Hub, `ghcr.io.evil.com/x`, `localhost:5000/x`): no `docker login`, no `gh api user`; only `ghcr.io/…` logs in. Tests in E-1 and E-2.
2. **`zephyr init` re-run on the owner's install** (split layout, two accounts, Discord, iMessage, `[web]`, bridge, a preset) with Enter everywhere: `zephyr.toml` and `host.toml` byte-identical, no `tailscale serve`, exactly one `docker restart`, no `docker run -d`. Test in E-8.
3. **`install.sh` on a machine with zephyr** (container exists, or only `~/.zephyr-daemon` after `zephyr down`): refuses before any `pull`, `cargo` or write. Test in E-5.
4. **An empty `ZEPHYR_REPO`** (`ZEPHYR_REPO=` in the shell, or an undefined Docker `ARG` baked in as `""`): reads as the default. Tests in E-2 (run time) and E-3's image check (build time).
5. **The minimal image's config still listing all four extensions**: the child starts, and `doctor` names each missing path with the new wording rather than failing. Tests in E-3 (`real_rpi`, `cli.rs`) and the E-3 image check.

---

### Task E-1: `pull` sends the gh token only to ghcr.io

**Files:**
- Modify: `crates/zephyr/src/host/mod.rs` (`pull` `:1135`; new `registry` beside it; unit test)
- Test: `crates/zephyr/tests/host.rs`

**Interfaces:**
- Produces: `fn registry(image: &str) -> &str` (the first path component when it has `.` or `:` or is `localhost`, else `docker.io`); `pull` logs in only when `image.starts_with("ghcr.io/")`, else prints `{image} is not on ghcr.io: docker login {registry} first, if it needs a login`.

- [x] **Step 1: Write the failing tests.**
  - unit `registries_are_read_as_docker_reads_them`: `ghcr.io/a/b:1` → `ghcr.io`; `registry.example.com/me/z:1` → `registry.example.com`; `localhost:5000/z` → `localhost:5000`; `user/zephyr` → `docker.io`; `zephyr-lanetest` → `docker.io`.
  - `host.rs` `pull_logs_in_with_the_gh_token_only_at_ghcr` (`up` with `SHIM_IMAGE_MISSING=1 SHIM_PULL_FAIL=1 SHIM_BUILD_FAIL=1`): for `ZEPHYR_IMAGE` in `registry.example.com/me/zephyr:1`, `user/zephyr:1`, `ghcr.io.evil.com/x:1`, `localhost:5000/z:1`: no `docker login` call, no `gh api user` call. For `ghcr.io/someone/zephyr:1`: `docker login ghcr.io -u tester --password-stdin` and a second `docker pull`; the token in no argv.
- [x] **Step 2:** `cargo test -p zephyr --test host pull_logs_in` → FAIL (logs in to `registry.example.com`).
- [x] **Step 3: Implement** in `pull`.
- [x] **Step 4:** the tests, then the full suite → PASS.
- [x] **Step 5:** Commit `fix(host): log in with the gh token only to ghcr.io, never to another registry an image names`.

### Task E-2: `ZEPHYR_REPO`

**Files:**
- Modify: `crates/zephyr/src/host/mod.rs` (`REPO` → `DEFAULT_REPO` + `repo()`; callers `:126-159, 824, 964-966, 1548, 1602`; `USAGE` env line), `deploy/Dockerfile` (`rust` stage `ARG ZEPHYR_BUILD_REPO`, named `ZEPHYR_REPO` until the review), `.github/workflows/image.yml` (`v` step `image=`, tags, `build-args`), `DESIGN.md` §4.8, `README.md` ("Building the image"), `CLAUDE.md` (the `ZEPHYR_CONTAINER` line)
- Test: `crates/zephyr/tests/host.rs`, `host/mod.rs` unit tests

**Interfaces:**
- Produces: `pub const DEFAULT_REPO: &str = "ghcr.io/yansigit/zephyr";` and `fn repo() -> String` exactly as spec §4.2 (env `ZEPHYR_REPO`, else `option_env!("ZEPHYR_BUILD_REPO")` since the review, else default; empty counts as unset). Images are `format!("{}:{v}", repo())`. `latest()`'s error names `registry(&image)`.

- [x] **Step 1: Write the failing tests.**
  - unit: `DEFAULT_REPO == "ghcr.io/yansigit/zephyr"`; the pinned `local_image` test uses `format!("{DEFAULT_REPO}:0.4.0")` and still gives `zephyr-local:0.4.0-90531f12`.
  - `default_image_is_the_owners_release`: `ZEPHYR_IMAGE=""`, no `ZEPHYR_REPO`, `init --non-interactive` → `docker image inspect ghcr.io/yansigit/zephyr:<CARGO_PKG_VERSION>` and `docker run -d` ends with that image.
  - `zephyr_repo_moves_the_image_and_keeps_the_gh_token_at_ghcr`: `ZEPHYR_IMAGE=""`, `ZEPHYR_REPO=registry.example.com/me/zephyr`, `SHIM_LATEST=99.0.0`: `upgrade` pulls `registry.example.com/me/zephyr:latest`. With `SHIM_PULL_FAIL=1`: it fails saying `could not pull registry.example.com/me/zephyr:latest` and `docker login registry.example.com`, no `docker login` call, no `gh api user`, and `env.log` has no `GH_TOKEN set`. With `ZEPHYR_REPO=""` and `SHIM_PULL_FAIL=1`: the error names `ghcr.io/yansigit/zephyr:latest`.
  - `the_image_workflow_tags_the_repository_it_runs_in` (reads image.yml): `image=ghcr.io/${GITHUB_REPOSITORY,,}`, `ZEPHYR_REPO=${{ steps.v.outputs.image }}`, and no literal `ghcr.io/yansigit` in a tag line.
- [x] **Step 2:** run them → FAIL (no `ZEPHYR_REPO`, `DEFAULT_REPO` undefined).
- [x] **Step 3: Implement.** Dockerfile: `ARG ZEPHYR_BUILD_REPO` (named `ZEPHYR_REPO` until the review) after `FROM toolchain AS rust` (the `RUN cargo build` sees it as an env var). image.yml: the `v` step writes `image=ghcr.io/${GITHUB_REPOSITORY,,}`; both build steps pass `build-args: ZEPHYR_BUILD_REPO=${{ steps.v.outputs.image }}`; tags use it.
- [x] **Step 4:** the tests, then the full suite → PASS.
- [x] **Step 5:** Commit `feat(host): read the image repository from ZEPHYR_REPO at run or build time, defaulting to today's`.

### Task E-3: The image without the owner's provider sources

**Files:**
- Modify: `deploy/Dockerfile` (`fetch` `:42`, `rust` `:72`, `plugins` `:88`, the header comment `:10`), `crates/zephyr/src/main.rs` (doctor `:578-587`), `.github/workflows/image.yml` (`workflow_dispatch` input `providers`, minimal smoke test), `DESIGN.md` §4.4 item 1 and §4.7, `README.md` ("Building the image")
- Test: `crates/zephyr/tests/cli.rs` (`doctor_warns_when_an_extension_path_is_missing`), `crates/zephyr/tests/real_rpi.rs`, `crates/zephyr/tests/host.rs`

**Interfaces:**
- Produces: `docker build -f deploy/Dockerfile --build-arg RPI_PACKAGE_REPO= --build-arg PLUGINS_REPO= .` builds an image with zephyr, the policy plugin, `libzephyr_cursor.so`, and none of `librpi_providers.so`, `librpi_mcp_adapter.so`, `rpi-claude-agent/`.

- [x] **Step 1: Write the failing tests.**
  - `cli.rs`: the doctor test expects the full new line (Global Constraints).
  - `real_rpi.rs` (ignored) `a_missing_extension_does_not_stop_the_child`: config with `extensions = ["/nonexistent/librpi_providers.so"]` beside the built policy plugin (when present) answers `ping` with `pong`.
  - `host.rs` `up_build_passes_the_gh_token_as_a_secret_only` also asserts no `--build-arg` in the build call.
  - `host.rs` `the_image_workflow_smoke_tests_every_extension`: image.yml has `test -f` for the policy plugin and each `conf::EXTENSIONS` path, and a `providers` choice input `full`/`none` defaulting to `full`.
- [x] **Step 2:** run them → the cli and workflow tests FAIL.
- [x] **Step 3: Implement** spec §4.3 (`fetch` exits 0 with an empty `/src` on an empty repo; `rust` builds rpi-package only when `/src/rpi-package/Cargo.toml` exists; `plugins` runs `stage.sh` only when it exists, else `mkdir -p /out/ext`) and §4.4 (`providers: none` adds the two empty args, tags `:dev-minimal`, never pushes, smoke-tests presence and absence). Doctor wording.
- [x] **Step 4:** the tests (and `--ignored` for the new real_rpi test with `ZEPHYR_REAL_RPI=1`), then the full suite → PASS.
- [x] **Step 5:** Commit `feat(image): leave a provider source out of the image when its repo arg is empty`.
- [x] **Step 6: Image checks** (background, after the commit): `docker build -f deploy/Dockerfile -t zephyr-dev:e-<sha> .` and the same with both args empty as `zephyr-dev:e-<sha>-minimal`. In `docker run --rm`: `zephyr --version`, `rpi --version`, the present and absent files, and for the minimal image `rpi --list-models` with all four `--extension` flags from `conf::EXTENSIONS` plus the policy plugin exits 0. Remove both images after.

### Task E-4: `RPI_BIN` in release.sh, `ZEPHYR_GH_REPO` in the runner

**Files:**
- Modify: `deploy/release.sh:31,45`, `deploy/runner/start.sh:12`, `README.md` ("Releasing")
- Test: `crates/zephyr/tests/host.rs`

- [x] **Step 1: Write the failing test** `release_and_runner_scripts_keep_their_defaults`: `bash -n` passes on both; release.sh has `rpi=${RPI_BIN:-/home/user/Projects/.always-on/rpi-` and `export RPI_BIN="$rpi"`; start.sh has `repo=${ZEPHYR_GH_REPO:-yansigit/zephyr}`.
- [x] **Step 2:** → FAIL. **Step 3:** edit the two lines (the ignored run gets `RPI_BIN="$rpi"` exported). `bump-rpi.sh`'s `rpi-$old/bin/rpi` rewrite still matches.
- [x] **Step 4:** full suite → PASS. **Step 5:** Commit `chore(deploy): let release.sh take RPI_BIN and the runner ZEPHYR_GH_REPO, defaulting to today's`.

### Task E-5: `deploy/install.sh`

**Files:**
- Create: `deploy/install.sh` (mode 755)
- Modify: `.github/workflows/ci.yml` (`shellcheck deploy/install.sh`), `DESIGN.md` §4.8 and §5, `README.md` (Quick start leads with it), `CLAUDE.md` (Commands block)
- Test: `crates/zephyr/tests/host.rs` (shim: `SHIM_DOCKER_DOWN` makes every docker call fail; stubs `cargo`, `sudo`, `wget`)

**Interfaces:**
- Consumes: `zephyr::host::DEFAULT_REPO` (E-2).
- Produces: `deploy/install.sh [--from-image | --from-source] [--no-init] [-- INIT ARGS..]` per spec §4.5; `repo=${ZEPHYR_REPO:-ghcr.io/yansigit/zephyr}`; refuses with `zephyr is installed here; upgrade with zephyr upgrade` when `docker inspect --type container ${ZEPHYR_CONTAINER:-zephyr}` succeeds or `~/.zephyr-daemon` exists; writes only `~/.local/bin/zephyr` (temp file + `trap` + `mv`); then `exec <installed binary> init "$@"` unless `--no-init`.

- [x] **Step 1: Write the failing tests** (each runs `bash deploy/install.sh` with the shim first on `PATH` and `HOME` in the test dir):
  - `install_refuses_an_existing_install`: with the shim's container (default) → exit 1, the refusal text; with `SHIM_NO_CONTAINER=1` and `~/.zephyr-daemon` made → the same. Neither calls `docker pull` nor `cargo`.
  - `install_from_the_image_writes_the_cli_and_runs_init`: `SHIM_NO_CONTAINER=1 --from-image -- --non-interactive` → `docker pull ghcr.io/yansigit/zephyr:latest`, `docker run --rm --entrypoint cat ghcr.io/yansigit/zephyr:latest /usr/local/bin/zephyr`, `~/.local/bin/zephyr` mode 755, `exec.log` has `init --non-interactive`; `--no-init` leaves `exec.log` without `init`; no temp file left in `~/.local/bin`.
  - `install_from_source_runs_cargo_install`: `--from-source --no-init` → the cargo stub's log has `install --locked --path crates/zephyr`.
  - `install_without_docker_names_its_install_page`: `SHIM_DOCKER_DOWN=1` → exit 1, output has `https://docs.docker.com/engine/install/`.
  - Every test: `sudo`, `curl`, `wget` absent from `calls.log`.
  - `install_sh_defaults_to_the_clis_repo`: the file contains `${ZEPHYR_REPO:-{DEFAULT_REPO}}`.
- [x] **Step 2:** → FAIL (no script). **Step 3:** write the script (spec §4.5 steps 1–5 and "It never").
- [x] **Step 4:** `shellcheck deploy/install.sh` (via `docker run --rm koalaman/shellcheck:stable` when not installed), the tests, the full suite → PASS.
- [x] **Step 5:** Commit `feat(install): add deploy/install.sh, which installs the CLI from the image or the source and refuses an existing install`.

### Task E-6: `zephyr init --network bridge|host`

**Files:**
- Modify: `crates/zephyr/src/host/conf.rs` (`set_network_default` → `set_network(Option<&str>)`), `crates/zephyr/src/host/setup.rs` (`init`), `crates/zephyr/src/host/mod.rs` (`USAGE` init line), `README.md` (init row), `DESIGN.md` §4.8
- Test: `crates/zephyr/tests/host.rs`

**Interfaces:**
- Produces: `Data::set_network(&self, network: Option<&str>) -> anyhow::Result<()>`: `Some(v)` writes `network = v`; `None` writes `bridge` only when absent; a new key gets today's `# "host" runs without the network floor (DESIGN §4.20)` comment. `init` refuses any `--network` other than `bridge`/`host` before writing anything, and prints one of the two network lines.

- [x] **Step 1: Write the failing test** `init_network_flag_sets_and_keeps_the_mode`: `--network host` → host.toml `network = "host"` and the `network: host (…)` line; re-run without the flag keeps `host`; `--network foo` fails with `--network must be bridge or host` and leaves host.toml unchanged; `--network bridge` → `network: bridge (the agent's network floor)`.
- [x] **Step 2:** → FAIL. **Step 3:** implement. **Step 4:** full suite → PASS.
- [x] **Step 5:** Commit `feat(init): set the network mode with --network bridge|host and say which one runs`.

### Task E-7: The iMessage webhook forward in `channel add`

**Files:**
- Modify: `crates/zephyr/src/host/web.rs` (extract `serve` and `forwarded`), `crates/zephyr/src/host/setup.rs` (`channel_add`), `DESIGN.md` §4.8, `README.md` (the iMessage forward paragraph `:766`)
- Test: `crates/zephyr/tests/host.rs`

**Interfaces:**
- Produces in `web.rs`: `pub(super) fn forwarded(target: &str) -> bool` (`tailscale serve status` has `target` as a whole word; `status` uses it) and `pub(super) fn serve(args: &[&str], again: &str) -> anyhow::Result<()>` (runs `tailscale serve ARGS`; on refusal prints its stderr and fails with the sudo form `sudo tailscale serve ARGS`, `sudo tailscale set --operator=$USER`, `then run {again} again`). `web::setup` uses both; its messages are unchanged.
- `channel_add` for `imessage`, after saving: under bridge, with `imessage.listen`'s port P and `tcp://127.0.0.1:P` not `forwarded`, asks the forward question (default yes); yes runs `serve(&["--bg", "--tcp", P, "tcp://127.0.0.1:P"], "zephyr channel add imessage")`, printing its error and carrying on; no prints `forward it later: sudo tailscale serve --bg --tcp P tcp://127.0.0.1:P`.

- [x] **Step 1: Write the failing test** `channel_add_imessage_offers_the_tailscale_forward` (driven tty): no forward in `SHIM_TS_SERVE` → asked, `y` → `tailscale serve --bg --tcp 7791 tcp://127.0.0.1:7791`; with `SHIM_TS_SERVE` holding `tcp://127.0.0.1:7791` → not asked, no `serve --bg`; with `SHIM_TS_DENIED=1` → `sudo tailscale serve --bg --tcp 7791 tcp://127.0.0.1:7791` printed, exit 0, and the channel saved.
- [x] **Step 2:** → FAIL. **Step 3:** implement. **Step 4:** the web tests and the full suite → PASS.
- [x] **Step 5:** Commit `feat(channel): offer the Tailscale forward to the iMessage webhook when it is missing`.

### Task E-8: init's web step, its published ports, its hints, and the re-run proof

**Files:**
- Modify: `crates/zephyr/src/host/setup.rs` (`init`; new `init_web`), `crates/zephyr/src/host/web.rs` (`setup` → `pub(super)`), `DESIGN.md` §4.8, `README.md` (Quick start's init paragraph)
- Test: `crates/zephyr/tests/host.rs` (`init_asks_for_a_preset_and_writes_it` gains `("Set up the web app", "n")`)

**Interfaces:**
- Consumes: E-6's network line, E-7's forward (through `channel_add`), `web::setup(h, false)`.
- Produces: interactive `init` records `before = h.published()?` after its first `up_checked`; after `init_approvals` calls `init_web(h) -> anyhow::Result<bool>` (true when `web::setup` succeeded; skipped with `web app: {NEEDS_FLOOR}` under host); then `up_checked()` when `h.published()? != before`, else `docker restart` unless `init_web` was true; then `doctor`. Non-interactive init prints the two hints only when unset (the web hint only under bridge).

- [x] **Step 1: Write the failing tests.**
  - `init_again_on_a_configured_install_changes_nothing` (Review Focus 2): non-interactive init, two accounts, then Discord, iMessage, `[web] host`, `[approvals] preset` written into zephyr.toml, `env.split()`; snapshot both files; driven init answering Enter at every prompt (`Projects folder`, `Providers`, `Add another` ×2, `Failover order`, `Change the discord`, `Change the imessage`, `Add private facts`, `Change the web app?`) with `SHIM_TS_SERVE` showing both forwards → exit 0, both files byte-identical, no `tailscale serve --bg`, exactly one `docker restart`, no `docker run -d` after the first `up`.
  - `init_adding_imessage_publishes_its_port_and_offers_the_forward`: driven init answering `y` to `Set up imessage?` and the forward → `tailscale serve --bg --tcp 7791 …`, a second `docker run -d` with `-p 127.0.0.1:7791:7791`, no `docker restart`.
  - `init_under_host_networking_skips_the_web_app`: driven init with `--network host` → `Set up the web app` never printed.
  - `non_interactive_init_hints_at_channels_and_the_web_app`: fresh → both hints; with `[discord]` and `[web] host` set → neither.
- [x] **Step 2:** → FAIL. **Step 3:** implement. **Step 4:** full suite → PASS.
- [x] **Step 5:** Commit `feat(init): offer the web app, publish the ports init added, and hint at what is left`.

### Task E-9: The self-hosting guide

**Files:**
- Modify: `README.md` (new **Self-hosting** section after Quick start, spec §4.7 items 1–7; "Building the image" names `ZEPHYR_REPO` and the minimal build), `DESIGN.md` §4.8 (the knobs, one line)

- [x] **Step 1:** write it, with each knob's default (`ZEPHYR_REPO` `ghcr.io/yansigit/zephyr`, `ZEPHYR_IMAGE` unset, `ZEPHYR_CONTAINER` `zephyr`, `ZEPHYR_SRC` this checkout, `RPI_PACKAGE_REPO`/`PLUGINS_REPO` the forks, `RPI_BIN` the pinned path, `ZEPHYR_GH_REPO` `yansigit/zephyr`).
- [x] **Step 2:** `cargo test` (the README is read by no test; this is the full suite for the commit) → PASS.
- [x] **Step 3:** Commit `docs(readme): add a self-hosting guide with every knob and its default`.

## Not done here (owner actions, spec §5 and decision 2b)

- Tag the rpi-package pin in the fork (`zephyr-pin-11e8ac1`) so it survives the branch: an owner action on `yansigit/rpi-package`, not this lane's to push.
- Everything in spec §5's publish checklist.

## Deviations

1. **Commit order** is E-1, E-2, E-3, E-6, E-7, E-5, E-8, E-4, E-9, not the task numbers. E-5 was built in parallel by a subagent in its own worktree, from E-2's commit (it needs `DEFAULT_REPO`), and cherry-picked after E-7 with one DESIGN §4.8 conflict resolved by keeping both bullets. The other tasks are independent of each other's order.
2. **E-5 beyond the plan** (the subagent's, reviewed): a seventh test, `install_logs_in_only_at_ghcr_when_the_pull_fails`; both `--from-image` and `--from-source` together print the usage and exit 2; a failed pull from another registry names it (`docker login <registry> first`) as E-1's `registry()` reads it; `cargo install` runs in a subshell so `init`'s relative arguments still resolve from the caller's directory.
3. **E-8: `web::setup` returns `anyhow::Result<bool>`**, true once it has started the container (step 6). The plan only made it `pub(super)`, but setup can stop early when a question is declined, and init must then still restart or `up`. `zephyr web setup` itself is unchanged (`.map(|_| ())`).
4. **E-8: the re-run test uses the split layout** (`env.split()`, the files reached through `zephyr __file`), as the owner's install is. The shim cannot make `~/.zephyr-daemon` root's, so init's closing `doctor` fails that one check there; the test asserts the files, the calls and every prompt answered, not the exit code. The other new init tests use the unsplit layout and assert exit 0.
5. **E-8 was written code-and-test together**, then checked failing-first by running its five tests against the E-7 code (all five fail), then passing.
6. **Image checks** (E-3 step 6) were built from `git archive d8dfc3e`, not the worktree, which held later work by then, and with no gh token, which proves the public build. A third build of the `rust` stage alone with `--build-arg ZEPHYR_REPO=registry.example.com/me/zephyr` proved the build-time value bakes in, and that the run-time one overrides it. rpi's start was checked with `rpi --mode rpc` (a dummy `ANTHROPIC_API_KEY`, `get_state` answered) as well as `--list-models`. All three images were removed.
7. **Known flakes, not this lane's:** `cli.rs` tests that compare wall-clock seconds (`status_shows_the_main_child_the_waiting_message_and_the_replies`, `status_fails_without_a_daemon_or_with_a_stale_heartbeat`, `a_reexec_waits_for_the_running_reply`) and, once each, `attach.rs` `facts_put_answers_without_values` and `tui.rs` `panels_pick_a_model_list_commands_and_close` failed in full runs while four lanes and a docker build loaded the machine (load average 30 to 42). Each passed alone; none touches host code. The subagent also reported `up_in_bridge_mode_publishes_the_web_port_on_loopback` matching `7793` in the temp path, worth fixing separately.

## Review fixes (PR #46)

`/code-review high` found ten issues; each was confirmed, none skipped, each fixed failing test
first (item 8 is a refactor with the same behaviour, so its tests stay as they were):

| # | Finding | Commit |
|---|---|---|
| 1 | Provider sources were skipped when a file was missing, not when the arg was empty | `0f64864` |
| 2 | `option_env!("ZEPHYR_REPO")` baked a run-time override in; now `ZEPHYR_BUILD_REPO`, and `release.sh` unsets the CLI's knobs | `f409f48` |
| 8 | The gh login is gated on `registry(image) == "ghcr.io"` | `8be9125` |
| 9 | `set_network` (and `set_projects`) dropped an inline comment | `b08ff5f` |
| 4, 7 | init restarted after a declined web restart, or twice after a web error; `Host::republish` shared, with its `floored()` check | `4df37aa` |
| 3, 5, 6, 10 | install.sh: upgrade repo (source build bakes `ZEPHYR_BUILD_REPO`; the image path prints to keep `ZEPHYR_REPO` set), more refusals and `mv -n`, version check against the image, `--no-init` with init args | `d50c9b2` |

Checks: a default `docker build` at `f409f48` (full set, the CLI defaulting to
`ghcr.io/yansigit/zephyr`), the minimal build (the three paths absent), and a `plugins`-stage build
with the arg set over an empty tree, which now fails. A `cargo build` with `ZEPHYR_REPO` exported
still defaults; with `ZEPHYR_BUILD_REPO` it bakes that in. Full suite: 1171 passed, 0 failed.
