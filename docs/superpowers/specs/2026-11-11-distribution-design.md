# Distribution: zephyr ready to self-host, with nothing published

Status: design drafted 2026-11-10; awaiting the owner's review.
Sub-project **E** of the Muse-parity roadmap (Wave 2; the table is in
`2026-11-07-settings-registry-design.md` §1). It depends on **S** (`set_setting`, which
`zephyr web setup` writes through) and **W** (`zephyr web setup`, since 0.8.0). It takes no schema
number: nothing here touches `state.db`.

## 1. Why

Today zephyr installs in one place: this owner's machine. The CLI pulls
`ghcr.io/yansigit/zephyr:<version>`, a private package. `release.sh` looks for rpi under
`/home/user/Projects/.always-on`. The fallback runner registers against `yansigit/zephyr`. The
README's first line asks for read access to a private repo. Before zephyr can be published, a
stranger must be able to build, install and run it with no value of this owner's baked in. Until the
owner decides to publish, every default must stay exactly as it is.

One premise of the roadmap no longer holds. The roadmap calls the provider repos private, but both
are **public** now (checked with `gh repo view` on 2026-11-10), and both are MIT:
`yansigit/rpi-package` is a fork of the public `pi-rust/rpi-package`, and
`yansigit/rpi-provider-plugins` is the owner's own repo. The fetch stage already builds without a
token when no secret is given (Dockerfile:45). So "the image builds without the private repos"
comes down to two things. First, a build must not depend on the owner's forks: they can move, go
private again, or lose the feature branch the pin sits on. Second, a smaller image should be
possible for anyone who wants neither source.

## 2. Requirements

- Every owner-specific value is listed (§4.1) and either becomes a knob or stays, with the reason.
- With no knob set, the CLI, the image, CI, `release.sh` and the runner do exactly what they do
  today, and tests prove it (Tests).
- The CLI's image repository can be changed, at build time and at run time, and versioned upgrades
  keep working (`ZEPHYR_REPO`, §4.2).
- `deploy/Dockerfile` builds with either provider source or both left out (§4.3), and CI can build
  that variant on request without pushing it (§4.4).
- `deploy/install.sh` installs the CLI on a clean Linux machine and then runs `zephyr init`. It
  never runs as root, never fetches a script, and refuses to touch an existing install (§4.5).
- `zephyr init` also covers the iMessage webhook forward, the web app and the network mode, and
  a re-run on the owner's install changes nothing the owner does not choose (§4.6).
- The README gains a self-hosting guide (§4.7).
- A publish checklist is recorded here and not acted on (§5).

### Non-goals (decided, with the reason)

- **Nothing goes public**: no visibility change, no public image, no rename, no release assets,
  and no public URL for `install.sh`. That is the owner's decision.
- **No new host.toml key for the repository.** host.toml is read through `Data::via`
  (conf.rs:135), which runs `self.image` when the container is stopped, so the image's name
  cannot come from a file that is read through the image.
- **No provider menu filtering** in `init` and `login` for an image built without providers. The
  owner's image has every provider. A failed sign-in already prints rpi's error and init carries on
  (setup.rs:144), and `doctor` names each missing extension (main.rs:580).
- **No non-interactive channel setup.** Its secrets would go on argv or into a script's
  environment. `zephyr channel add` takes them through hidden prompts, as today.
- **No `zephyr up --build` switch for the minimal image**: that is open question 1. A
  `docker build` with two empty args does the job (§4.3).
- **No multi-arch image, no signing, no SBOM**: those come at publish time (§5).

## 3. What changes in DESIGN.md, README.md and CLAUDE.md

- DESIGN §4.4 item 1: the image's provider sources are build args, and an empty one leaves that
  source out (§4.3). §4.7: the providers each source brings. §4.8: `ZEPHYR_REPO`,
  `deploy/install.sh`, and init's web, iMessage-forward and `--network` steps. §5 Repo layout gains
  `deploy/install.sh`. The decision lines at DESIGN.md:5 and :988 stay as they are (§5).
- README: Quick start leads with `deploy/install.sh`; a new **Self-hosting** section (§4.7);
  "Building the image" covers the empty args and `ZEPHYR_REPO`; "Releasing" covers `RPI_BIN`; the
  `init` row gains `--network`.
- CLAUDE.md: the Commands block gains `deploy/install.sh`, and the `ZEPHYR_CONTAINER` /
  `ZEPHYR_IMAGE` line gains `ZEPHYR_REPO`.

## 4. Design

### 4.1 Inventory of owner-specific values

Found by `git grep -E '/home/user|yansigit|\.always-on|aiden|rpi-package|rpi-provider-plugins'`
across all tracked files, together with a read of `deploy/`, `.github/` and `host/`.

| # | Where | Value | Becomes | Why |
|---|---|---|---|---|
| 1 | `host/mod.rs:39` `REPO` | `ghcr.io/yansigit/zephyr:` | `fn repo()`: env `ZEPHYR_REPO`, else build-time `option_env!("ZEPHYR_REPO")`, else `DEFAULT_REPO` (unchanged) | §4.2. Not host.toml (non-goals) |
| 2 | `host/mod.rs:126-129, 152-159, 824, 964, 1548` | uses of `REPO` | call `repo()` | same |
| 3 | `host/mod.rs:966` | ``docker login ghcr.io`` in the error | names `repo()`'s registry | text |
| 4 | `host/mod.rs:1145` `pull` | logs in to *any* image's registry with the `gh` token | logs in only when the registry is `ghcr.io` | today a `ZEPHYR_IMAGE` on another registry is sent this user's GitHub token, and `user/zephyr` (Docker Hub) parses as registry `user` |
| 5 | `host/mod.rs:36` `USAGE` | env line | gains `ZEPHYR_REPO` | — |
| 6 | `host/mod.rs:1602-1605` | pinned hash of `ghcr.io/yansigit/zephyr:0.4.0` | left | proves local image tags (`zephyr-local:<v>-<hash>`) do not move |
| 7 | `deploy/Dockerfile:15, 17` | `RPI_PACKAGE_REPO`, `PLUGINS_REPO` (the owner's forks) | already build args; same defaults; empty now means "leave it out" (§4.3) | — |
| 8 | `deploy/Dockerfile:16, 18` | `RPI_PACKAGE_REF`, `PLUGINS_REF` | left | commit SHAs in public repos; see open question 2 |
| 9 | `deploy/Dockerfile:10` | `../rpi-package--codex`, `../rpi-provider-plugins--claude` | `<dir>` in the comment | the owner's checkout names |
| 10 | `deploy/Dockerfile` `rust` stage | — | new `ARG ZEPHYR_REPO` | the image's own CLI gets the repo it was published to (§4.2) |
| 11 | `.github/workflows/image.yml:82-83` | `ghcr.io/yansigit/zephyr` tags | `ghcr.io/${GITHUB_REPOSITORY,,}` from the `v` step, also passed as build arg `ZEPHYR_REPO` | here that is the same name; a fork publishes to its own |
| 12 | `image.yml:4-5, 58, 85` | `PROVIDER_REPOS_TOKEN` | left | not needed while both repos are public; harmless, and a private pin keeps working |
| 13 | `deploy/release.sh:31` | `rpi=/home/user/Projects/.always-on/rpi-0.21.2/bin/rpi` | `rpi=${RPI_BIN:-<same path>}`, and `export RPI_BIN="$rpi"` before the ignored run | the tests read `RPI_BIN` already; `bump-rpi.sh:354` still matches the literal |
| 14 | `release.sh:55, 62` | `ghcr.io/yansigit/zephyr` in a comment and a message | left | text; the workflow decides the real name |
| 15 | `tests/real_rpi.rs:17`, `tests/attach.rs:852`, `zephyr-policy/tests/rpi_load.rs:25`, `zephyr-cursor/tests/e2e.rs:27` | the same rpi path | left | each reads `RPI_BIN` first; `bump-rpi.sh` moves the literal |
| 16 | `deploy/bump-rpi.sh` | none of its own | left | it only rewrites #13 and #15 (line 354) |
| 17 | `deploy/runner/start.sh:12` | `repo=yansigit/zephyr` | `repo=${ZEPHYR_GH_REPO:-yansigit/zephyr}` | a fork registers its own runner |
| 18 | `deploy/runner/start.sh:35` | `/home/user/.local/share/buildkit` | left | the home of the rootless BuildKit image's own user (`user`), not this machine's |
| 19 | `main.rs:583` doctor | "the CLI and the image are out of step" | "…out of step, or the image was built without it" | a minimal image (§4.3) |
| 20 | `README.md:8, 18, 864-870, 885` | private repo, `ghcr.io/yansigit` | rewritten with §4.7; the defaults are still named | docs |
| 21 | `CLAUDE.md:20, 35`; `DESIGN.md:3, 5, 43, 988, 1007` | paths, repo name | left | dev notes and decision history; §5 |
| 22 | fixtures: `memory.rs:1420, 1437`; `vault.rs:925, 950, 1103, 1110, 1206`; `tui/overlays.rs:2074, 2089`; `tests/attach.rs:901, 908`; `tests/router.rs:5244, 5266`; `tests/host.rs:2479, 2499` | `aiden`, `Aiden` | left | test data; §5 |
| 23 | `docs/superpowers/plans/2026-11-07-settings-and-approvals.md:17`, `2026-11-10-wave1.md:24`; git history | the owner's commit identity | left | §5 |
| 24 | `Dockerfile:97`, `release.sh:38`, `bump-rpi.sh:31`, `rpi-bump.yml` | `bigfish1913/pi-rust` | left | upstream rpi, not the owner's |
| 25 | `host/setup.rs:18` `CLAUDE_CLI` | the linux-x64 path | left | not owner-specific (its `shortcut:` already names arm64) |

### 4.2 `ZEPHYR_REPO`

```rust
/// The published image's repository; images are `<repo>:<version>`.
const DEFAULT_REPO: &str = "ghcr.io/yansigit/zephyr";

fn repo() -> String {
    let set = |v: Option<String>| v.filter(|v| !v.is_empty());
    set(std::env::var("ZEPHYR_REPO").ok())
        .or_else(|| set(option_env!("ZEPHYR_REPO").map(String::from)))
        .unwrap_or_else(|| DEFAULT_REPO.into())
}
```

- The value has no trailing `:`, and callers write `format!("{}:{v}", repo())`. `running_version`
  and `local_image` strip `format!("{}:", repo())`. An empty value counts as unset, because an
  undefined Docker `ARG` can still reach the build as an empty string.
- Setting it **at build time** (`ZEPHYR_REPO=… cargo install`, or the Dockerfile's `ARG` in the
  `rust` stage) bakes it into the CLI. The CLI that `zephyr upgrade` copies out of the image
  (`docker run --entrypoint cat`) therefore pulls from the same place on its next upgrade.
- Setting it **at run time** overrides the build-time value for one shell or one test.
  `ZEPHYR_IMAGE` still wins over both, as today, and still turns versioned upgrades off
  (mod.rs:808).
- `pull` logs in with `gh` only for `ghcr.io/...` images (#4). Another registry needs the
  owner's own `docker login`, and the error says so.

### 4.3 The image without the owner's provider sources

**Mechanism.** If a source's repo arg is empty, that source is left out. The defaults stay as they
are, so nothing changes for this owner.

- `fetch` (Dockerfile:42): `[ -n "$1" ] || { mkdir -p /src; exit 0; }` before `git init`.
- `rust` stage (Dockerfile:72): the zephyr build and copies run as now. The `rpi-package` build
  and its two `cp` lines run only when `[ -f /src/rpi-package/Cargo.toml ]`.
- `plugins` stage (Dockerfile:89): `stage.sh` runs when it is there, else `mkdir -p /out/ext`. The
  final stage's `COPY --from=plugins /out/ext/` then copies an empty folder.
- The build: `docker build -f deploy/Dockerfile --build-arg RPI_PACKAGE_REPO= --build-arg
  PLUGINS_REPO= -t "$ZEPHYR_REPO:<version>" .`. After that, `zephyr up` finds the image locally
  (`ensure_base`, mod.rs:268) and runs it.

**What is lost.**

| Source left out | Brings | Lost |
|---|---|---|
| `RPI_PACKAGE_REPO` (`yansigit/rpi-package`, 52 commits ahead of its `main` at the pin) | `librpi_providers.so`, `librpi_mcp_adapter.so` | OpenCode Go, Antigravity, ChatGPT/Codex (Dockerfile:16); every `[[mcp]]` connector, since there is no `mcp_*` tool (DESIGN §4.6) |
| `PLUGINS_REPO` (`yansigit/rpi-provider-plugins`) | `rpi-claude-agent/` | Claude through the Agent SDK, including `zephyr login claude-code` (`CLAUDE_CLI`) |

What stays: zephyr, the policy plugin, Cursor (`crates/zephyr-cursor`, this repo), and rpi's own
built-in providers. `rpi --local-only --list-models` on 0.21.2 lists only `anthropic` and
`openai`, so of init's menu the Anthropic API key and Cursor still work. `zephyr.toml` still lists
all four `EXTENSIONS` (conf.rs:13), and the child starts anyway, because rpi skips a path it
cannot load (main.rs:579). Today that is only a comment, so a new ignored test proves it (Tests).
`doctor` warns once per missing path, with the wording in #19.

**Tests lost.** No `cargo test` needs either source: CI never builds the image, and the host
tests only compare path strings (`tests/host.rs:1225, 1852`, `tests/cli.rs:660-670`). What is lost
is three lines of the image smoke test (image.yml:70, 71, 73). The minimal smoke test (§4.4)
checks the reverse: those files are absent.

**Keeping the full image for this owner.** The defaults do not move, so every tag build, every
`zephyr up --build` (mod.rs:325, which passes no `--build-arg`) and every `release.sh` run (which
waits on the tag build) builds today's image. The `gh_token` secret stays as it is. This is an
opt-out, not an add-in secret: an add-in would flip the default, and the sources are public anyway.

### 4.4 CI and release

- `image.yml`: the `v` step also writes `image=ghcr.io/${GITHUB_REPOSITORY,,}`, which here is
  `ghcr.io/yansigit/zephyr`. Both build steps tag with it and pass `build-args:
  ZEPHYR_REPO=<image>`. Expect one cold `rust` layer on the first build, since the arg is new.
- A new `workflow_dispatch` input, `providers: full|none` (default `full`). `none` adds the two
  empty build args, tags `:dev-minimal` and never pushes; push stays limited to `refs/tags/v*`.
  Its smoke test asserts `zephyr --version`, `rpi --version`, the policy plugin and
  `libzephyr_cursor.so`, and asserts the three files above are absent. On a tag, `inputs.providers`
  is empty and reads as `full`.
- `ci.yml` gains `shellcheck deploy/install.sh` (shellcheck comes preinstalled on GitHub's ubuntu
  images).
- `release.sh`: #13 only. `runner/start.sh`: #17 only.

### 4.5 `deploy/install.sh`

```
deploy/install.sh [--from-image | --from-source] [--no-init] [-- INIT ARGS..]
```

You run it from a checkout. Nothing gets piped into a shell (§5).

1. **Refuses an existing install.** If container `${ZEPHYR_CONTAINER:-zephyr}` exists or
   `~/.zephyr-daemon` exists, it stops with `zephyr is installed here; upgrade with zephyr
   upgrade`. This is what keeps this owner's machine safe: a new CLI followed by `init` would
   upgrade the live container through `up_checked`.
2. **Checks**, each failure naming the fix: Linux; `docker version` answers as this user (not
   root, and no `sudo`); `--from-image` only on x86_64, since image.yml builds amd64 only;
   `--from-source` needs `cargo` at least `rust-version` (1.89) and `docker buildx version`
   (`zephyr up --build` uses BuildKit secrets); and a warning when the Docker root has under 20 GB
   free.
3. **Installs the CLI.**
   - `--from-image`: `docker pull ${ZEPHYR_REPO:-ghcr.io/yansigit/zephyr}:latest`. For ghcr.io
     only, on failure it logs in with the `gh` token as `pull` does. It then runs
     `docker run --rm --entrypoint cat IMAGE /usr/local/bin/zephyr` into a temp file in
     `~/.local/bin`, does `chmod 755`, checks `--version`, and `mv`s the file into place.
   - `--from-source`: `cargo install --locked --path crates/zephyr`.
   - With neither flag: from the image if the pull works, else from source if `cargo` exists, else
     it stops and prints both ways.
4. **PATH**: it warns when the install folder is not on `PATH`, or when another `zephyr` comes
   first (as `release.sh:78` does).
5. Unless `--no-init`, it runs `exec zephyr init "$@"`.

**It never**: runs `sudo`; installs Docker, Rust, gh or Tailscale (it prints their install pages);
runs `curl` or `wget`; edits shell rc files, groups or systemd; logs in to a registry other than
ghcr.io; or writes anything but the one binary. Its shell default for the repo is the same string
as `DEFAULT_REPO`, and a test checks that.

**Tested** in `tests/host.rs`. The tests run `bash deploy/install.sh` with the existing shim on
`PATH` (docker, gh, tailscale), with `HOME` in the test dir, and with stubs for `cargo` (it
records its arguments) and for `sudo`, `curl` and `wget` (each fails the test if called). See
Tests.

### 4.6 `zephyr init` additions (`host/setup.rs`)

Interactive init already offers Discord and iMessage (setup.rs:167). What it lacks:

1. **The iMessage webhook forward.** Under the network floor, BlueBubbles posts to the Tailscale
   address, so `tailscale serve --bg --tcp P tcp://127.0.0.1:P` is needed (README:766). Today only
   `up` prints that, and only the once it moves an install over (mod.rs:476). `channel_add` (which
   serves both `init` and `zephyr channel add`) now handles it after writing `[imessage]`. When
   the network is bridge and `tailscale serve status` has no word `tcp://127.0.0.1:P`, it asks
   `Forward this machine's Tailscale port P to the webhook?` (default yes, since the channel does
   not work without it). It runs the command through a helper shared with `web::setup`, and on a
   refusal prints the same `sudo` form. A missed detection only means the question comes again;
   `tailscale serve` with the same mapping is idempotent.
2. **Publishing the ports init changed.** This fixes a bug that exists today. Init starts the
   container at setup.rs:123, and an `[imessage] listen` added after that is only reloaded by the
   final `docker restart` (setup.rs:183). A restart cannot add a published port, so the webhook is
   unreachable until the next `zephyr up`. Init now records `before = h.published()` after the
   first `up_checked`. Its last step is `up_checked()` when `h.published()? != before`, and the
   same `docker restart` as now otherwise.
3. **The web app.** After `init_approvals`, so that `web::setup`'s strong-channel and
   `strong_only` offers come after the preset:
   - When `[web] host` is set, init prints `web app: https://HOST` and asks `Change the web app?`
     (default no).
   - Otherwise it asks `Set up the web app (needs Tailscale with HTTPS certificates)?` (default
     no).
   - A yes calls `web::setup(h, false)`, made `pub(super)`. An error is printed and init carries
     on, as login errors do.
   - `web::setup` runs its own `up_checked`, so init then skips its final restart.
4. **`--network bridge|host`.** It writes host.toml `network` through a new
   `Data::set_network(Option<&str>)`, validated as `Host::bridge` (mod.rs:209) validates it.
   Without the flag it calls today's `set_network_default` (write `bridge` only when absent). There
   is no interactive prompt, because the floor is the safe default and a wizard should not offer
   the weaker choice. Init prints one line instead: `network: bridge (the agent's network floor)`
   or `network: host (no network floor; zephyr init --network bridge turns it on)`. Under `host`,
   step 3 is skipped with `crate::web::NEEDS_FLOOR`. The flag is the only CLI way to change it:
   after the first `up`, host.toml belongs to root.
5. **Non-interactive init** prints, after its sign-in hints, `add a channel: zephyr channel add
   discord|imessage` when there are none, and `web app: zephyr web setup` when `[web]` is unset.

**Re-running on this owner's install** (accounts, Discord, iMessage, `[web]`, bridge, a preset),
with Enter at every prompt, writes the same `zephyr.toml` and `host.toml`. It asks nothing about
the forward when `tailscale serve status` already shows it, and runs no `tailscale serve`. It ends
in one `docker restart`, as today. The one addition is the `Change the web app?` question.

### 4.7 The self-hosting guide (README, after Quick start)

1. **What you need.** Linux x86_64 (arm64 only from source: `CLAUDE_CLI` is x64), Docker with
   BuildKit run as your own user, about 20 GB of disk; optionally Tailscale (web app, iMessage) and
   gh.
2. **Get the CLI.** `deploy/install.sh` from a clone, or `cargo install --locked --path
   crates/zephyr`.
3. **Get the image.** Pull it, if you can read the registry. Otherwise build it with
   `zephyr up --build`: the provider sources are public, so no token is needed. Or build the
   minimal image (§4.3) and know what it lacks. Your own registry: `ZEPHYR_REPO` at
   `cargo install` and as the build arg.
4. **`zephyr init`, step by step.** Projects folder, providers and failover, Discord (making the
   bot, the user id), iMessage (BlueBubbles, the Tailscale forward), approvals preset, facts, web
   app, `--network`.
5. **Upgrading.** With a registry: `zephyr upgrade`. From source: `git pull && cargo install …
   && zephyr up --build`.
6. **Where your data lives, backups, uninstall**: links to the existing sections.
7. **The knobs**: `ZEPHYR_REPO`, `ZEPHYR_IMAGE`, `ZEPHYR_CONTAINER`, `ZEPHYR_SRC`, the Dockerfile
   args, `RPI_BIN`, `ZEPHYR_GH_REPO`, each with its default.

### 4.8 Failure handling

- `ZEPHYR_REPO` pointing nowhere: `could not pull <repo>:<v>`, and for a non-ghcr registry also
  `docker login <registry> first`. `up` still falls back to a local build (mod.rs:278), and
  `upgrade` still never does (mod.rs:826).
- Fetching a repo with an empty `REF` still fails, as `git fetch` does. Only an empty `REPO`
  means "leave it out".
- If `install.sh` is stopped midway, it leaves at most a temp file in `~/.local/bin`, which a
  `trap` removes. The `mv` is atomic.
- If init's forward or web step fails, it prints the command to run by hand and carries on.

## 5. What "publish" would need later (a checklist, not done now)

- [ ] Decide the name and the registry. Make `yansigit/zephyr` and its ghcr package public, or
      publish elsewhere and change `DEFAULT_REPO` with the matching tests.
- [ ] Add a `LICENSE` file. Cargo.toml says MIT, but the repo root has no license file.
- [ ] Check the licences of what the image redistributes. Above all, the Claude Code executable
      bundled by the Agent SDK in `rpi-claude-agent` is under Anthropic's terms: a public image may
      have to leave `PLUGINS_REPO` out. Also fingerprint-chromium, Camoufox, agent-browser, gh,
      himalaya, node and uv. Write a `THIRD_PARTY_NOTICES`.
- [ ] Pin the provider sources to tags that will not go away (open question 2), or to upstream
      `pi-rust/rpi-package` once its providers are enough.
- [ ] Review the tracked text for private material: `reports/`, `research_notes/`,
      `docs/superpowers/plans` (the commit identity), DESIGN.md's research paths and decision
      lines, and the `aiden` fixtures (#21-23).
- [ ] Scan the full git history for secrets (gitleaks or trufflehog) before anything goes public.
- [ ] Release assets: CLI binaries for amd64 and arm64 with sha256 files, a multi-arch image,
      cosign signatures and an SBOM. Only then a hosted `install.sh` that checks a checksum, and
      a one-line install in the README.
- [ ] `SECURITY.md` with a way to report vulnerabilities; CI on forks' PRs; drop
      `PROVIDER_REPOS_TOKEN` once no pin is private.
- [ ] Rewrite the Quick start for strangers, and update DESIGN.md:5 ("private").

## Open questions for the owner

1. **The minimal image from the CLI.** (a) A `docker build` with two empty args, documented in the
   guide, and no code. (b) A host.toml `providers = "none"` that `zephyr up --build` turns into
   those args. *Recommend (a)*: it takes no code, and `--build` is a developer path.
2. **The rpi-package pin** sits on `feat/providers-openai-codex`, 52 commits ahead of the fork's
   `main`. (a) Leave it. (b) Tag the commit in the fork (`zephyr-pin-11e8ac1`) so it stays
   fetchable if the branch is deleted. (c) Merge the branch into the fork's `main`. *Recommend (b)*:
   one tag, and no change to what is built.
3. **The minimal CI build.** (a) The manual `providers: none` input (§4.4). (b) Local builds only.
   *Recommend (a)*: it costs nothing until it is run, and it proves the variant still builds.
4. **`install.sh` on a machine that already has zephyr.** (a) Refuse and point at
   `zephyr upgrade`. (b) Hand over to `zephyr upgrade` itself. *Recommend (a)*: the installer
   then can never replace a live container.
5. **The iMessage forward question's default** in `channel_add`. (a) Yes, since the channel does
   not work without it. (b) No, like init's other questions. *Recommend (a)*: it is only asked
   when the forward is missing, so a re-run on a working install never sees it.

## Tests

**Defaults unchanged.**

- `host.rs` `default_image_is_the_owners_release` (new). With `ZEPHYR_IMAGE=""` and
  `ZEPHYR_REPO` unset, `init --non-interactive` inspects
  `ghcr.io/yansigit/zephyr:<CARGO_PKG_VERSION>`, and `up` runs it.
- The existing upgrade tests keep their literal names, and that is the proof:
  `upgrade_replaces_the_cli_from_the_newer_image_first` (`docker pull
  ghcr.io/yansigit/zephyr:latest`, `--entrypoint cat ghcr.io/yansigit/zephyr:99.0.0`, the
  `could not pull ghcr.io/yansigit/zephyr:latest` error) and
  `upgrade_waits_backs_up_and_goes_back_only_with_a_backup` (`ghcr.io/yansigit/zephyr:<me>`).
- `host/mod.rs` unit tests: `DEFAULT_REPO == "ghcr.io/yansigit/zephyr"`. The pinned `local_image`
  hash (mod.rs:1602) is unchanged.
- `host.rs`: `up --build` passes no `--build-arg` and still passes the `gh_token` secret.
- `host.rs`: a failed ghcr.io pull still logs in as the `gh` user and pulls again (there is no test
  of this today).
- `host.rs`: `deploy/install.sh`'s default repo string equals `DEFAULT_REPO` (read from the file).
- image.yml's tag smoke test keeps all four extensions and the policy plugin.

**New behaviour.**

- `host.rs` `zephyr_repo_moves_the_image_and_keeps_the_gh_token_at_ghcr`. With
  `ZEPHYR_REPO=registry.example.com/me/zephyr`, `upgrade` pulls
  `registry.example.com/me/zephyr:latest`. With `SHIM_PULL_FAIL` there is no `docker login` call,
  and `env.log` shows no `GH_TOKEN` to docker. With `ZEPHYR_REPO=""`, the default.
- `host.rs` init:
  - The driven-tty tests (`init_asks_for_a_preset_and_writes_it` and the rest) answer the new
    `Set up the web app` with `n`.
  - A re-run on a configured install (Discord, iMessage, `[web] host`, `network = "bridge"`, the
    tailscale shim showing both forwards), with Enter everywhere, leaves `zephyr.toml` and
    `host.toml` byte-identical, makes no `tailscale serve` call, and ends in exactly one
    `docker restart`.
  - Adding iMessage in init makes the container again with `-p 127.0.0.1:7791:7791`, and asks for
    the forward, then runs it on `y`.
  - `--network host` writes `network = "host"`, a re-run without the flag keeps it, and
    `--network foo` is refused. Under `host`, the web question is not asked.
  - Non-interactive init prints the channel and web hints only when they are unset.
- `host.rs` `channel add imessage`: the forward is asked when `tailscale serve status` lacks it and
  not when it has it. A refused `serve` prints the `sudo` form.
- `host.rs` `install.sh`, through the shim:
  - It refuses with an existing container or `~/.zephyr-daemon`, and calls neither `pull` nor
    `cargo`.
  - `--from-image` pulls `:latest` and writes `~/.local/bin/zephyr` (mode 755) from `cat`, then runs
    `zephyr init`; `--no-init` stops after the install.
  - `--from-source` calls `cargo install --locked --path crates/zephyr`.
  - Without a reachable docker, it stops naming Docker's install page.
  - `sudo`, `curl` and `wget` are never called.
- `real_rpi.rs` (ignored) `a_missing_extension_does_not_stop_the_child`. A config that lists
  `/nonexistent/librpi_providers.so` beside the policy plugin starts the child, which answers a
  prompt. This is the minimal image's case.
- `cli.rs`: doctor's missing-extension line carries the new wording.
- image.yml manual `providers: none`: the minimal smoke test (§4.4). `ci.yml`: `shellcheck
  deploy/install.sh`.
- `release.sh` and `runner/start.sh`: `bash -n`, plus a by-hand check that with `RPI_BIN` and
  `ZEPHYR_GH_REPO` unset the path and the repo are today's.
