# Connectors, starter skills and browser pacing: ready-made pieces that keep the owner's guards

Status: design drafted 2026-11-12; awaiting the owner's review.
Sub-project **D1** of the Muse-parity roadmap (Wave 3 in the plan; the table is in
`2026-11-07-settings-registry-design.md` §1). It needs A1 (the gate's connector classes, DESIGN
§4.16), A3 (the token broker, DESIGN §4.21, shipped in 0.9.0) and S (one registry row). Phone
calls and virtual cards (D2) are a separate spec.

## 1. Why

- **Connectors.** Adding an MCP server today means knowing its package, its command line, which
  variable holds its token, which hosts that token belongs to (`hosts`, §4.21) and which of its
  tools reach other people (`classes`, §4.16). An owner who gets any of those wrong either has a
  connector that does not work, or one whose token the agent can read, or one whose "post" tool is
  not gated. The Muse audit (`reports/Muse agent audit.md`, part D) asks for curated connectors
  "with the owner's `classes` pre-filled, read verbs left ungated", per tool, never per connector.
- **Output skills.** The agent can already write files and `send_file` them, but making a PDF
  report, a deck or a chart means rediscovering, every time, which tools the image has (no pandoc,
  one Latin font). Muse's Library shows owners expect finished artifacts. The audit adds the
  share-link lesson: a skill that publishes must treat publishing as a post; ours publish nothing.
- **Pacing.** Amazon blocked Muse's browsing, and one Instacart account was shut for "suspicious
  activity" (audit, part D). An agent in a loop (`for u in …; do agent-browser open $u; done`)
  hits a site faster than any person. Pacing protects the owner's own accounts with human-scale
  gaps; it is not stealth against sites that forbid automation (no fingerprint or user-agent
  change). No source measured its effect on account flags: this part is judgement.

## 2. Requirements

1. A catalog of 6 well-known MCP servers ships in the image, root-owned. Each entry states its
   command or URL, the secrets it needs, the hosts they are bound to, and the gate classes it
   presets. `zephyr mcp add --catalog NAME` on the host and `/mcp catalog add NAME` on a strong
   channel turn one into an ordinary `[[mcp]]` table.
2. A catalog secret reaches the agent only as a surrogate wherever §4.21 allows it, and the
   catalog never implies `secrets = "plain"`: the owner types it, as today.
3. Four `SKILL.md` skills (report, slides, pdf, charts) ship in the image and are approved into
   `<state>/skills` at the first start that sees them, without `/skill ok`. The owner removes one
   with `/skill rm NAME` and it stays removed.
4. The browser wrapper keeps a minimum gap, with jitter, between the agent's navigations and
   clicks on the same site, across every wrapper process, main's and tasks' alike. Registry
   setting `browser.pace_ms`.
5. No new top-level table is written to `zephyr.toml` unless the owner sets the new setting. No
   schema version. One new crate (`psl`, §4.3), justified there.

### Non-goals

- Updating an added catalog connector on upgrade. `zephyr.toml` gets no `catalog` key (an older
  strict `Config` would refuse it); `doctor` says when a newer entry ships (§4.1.5).
- Google Calendar and Gmail (§4.1.2), Playwright MCP (it drives a browser past the wrapper and
  the gate), and fetch and filesystem servers (they duplicate rpi's `read`/`write` and the
  browser, and a fetch server's GETs skip the wrapper).
- A "sent by the owner's agent" marker, commitment tools, CAPTCHA policy (audit part D): later.
- Pacing Camoufox, `xdotool`, the real `agent-browser` run by path, `eval` that navigates, and
  the daemon's own vault fill (it fills fields on the page already open, and clicks nothing).
- A gate class for physical actions (a Home Assistant lock). The classes stay messages, posts
  and money; §4.1.2 says what the owner does instead.

## 3. What changes in DESIGN.md

- §4.6 Skills and connectors: one bullet each pointing at §4.22 and §4.23; the `## Connectors`
  text in AGENTS.md (`drafts::sections`) gains one sentence: "Ready-made connectors are described
  in `/opt/zephyr/connectors/`; to suggest one, ask the owner to run `/mcp catalog add NAME`."
- §4.11 Browser: one bullet pointing at §4.24; Command-line tools: `poppler-utils`,
  `fonts-nanum`, `/usr/local/bin/github-mcp-server`.
- §4.17: one row, `browser.pace_ms` (§4.3.1 below).
- New §4.22 Connector catalog, §4.23 Starter skills, §4.24 Browser pacing (this spec, as built).
- README: `zephyr mcp add --catalog`, `zephyr mcp catalog`, `/mcp catalog`, the starter skills
  and how to remove one, `browser.pace_ms`, and the upgrade note (§4.3.1). CLAUDE.md: `catalog.rs`
  in the architecture list; `deploy/connectors/` and `deploy/skills/` in the layout.

## 4. Design

### 4.1 Curated connectors

#### 4.1.1 The catalog files

`deploy/connectors/NAME.toml`, copied by `deploy/Dockerfile` to `/opt/zephyr/connectors/`
(root's, folder 0755, files 0644). One file per entry, parsed by a new `crates/zephyr/src/catalog.rs`
(pure: `parse(name, text) -> Entry`, `Entry::to_mcp(url) -> Mcp`, no I/O beyond the caller's read):

```toml
about = "GitHub: issues, pull requests, code search (github/github-mcp-server 2.1.0)"
# Why the owner must type --plain for this one; absent when stand-ins work.
# plain_reason = "…"
# true: the owner gives --url, which fills {url} below; {host} is its host[:port].
# needs_url = false

[[secret]]
name = "GITHUB_TOKEN"
help = "a fine-grained personal access token: https://github.com/settings/personal-access-tokens/new"

[server]                  # exactly an [[mcp]] table (config::Mcp), minus `name` and `secrets`
command = "/usr/local/bin/github-mcp-server"
args = ["stdio"]
env = { GITHUB_PERSONAL_ACCESS_TOKEN = "${GITHUB_TOKEN}" }
hosts = "api.github.com, raw.githubusercontent.com"

[server.classes]
merge_pull_request = "posts"
```

- `Entry` and its tables are `deny_unknown_fields`: the image and its binary are one release, so
  an unknown key is a bug, caught by the unit test over every file (Tests).
- `[server]` is parsed as `config::Mcp` with `name` set from the file name, which must pass
  `drafts::valid_name`; `secrets` in it is refused (requirement 2).
- `{url}` and `{host}` are the only placeholders, in `url`, `args` and `hosts`; `needs_url`
  entries are refused without a URL, others refuse one.
- The daemon reads `Config::catalog_dir` (hidden field, default `/opt/zephyr/connectors`, tests
  point it at their own folder). The host CLI reads the same file through the container
  (`h.exec(&[], &[], &["cat", "/opt/zephyr/connectors/NAME.toml"])`, after `valid_name`), so the
  catalog has one copy and it is the image's. Adding one therefore needs the container running
  (`h.need_running()`), as `zephyr skill add` does.

#### 4.1.2 The starting catalog, checked 2026-11-11

How each is held follows §4.21: rpi's own MCP client (`url` connectors) trusts only its compiled
roots, so a decrypted host breaks it; only a program that runs as the agent and goes out through
the proxy (a `command` connector) can hold a stand-in, and every swap is into `Authorization` only.
Where the server is remote, the catalog runs it through **`mcp-remote`** (geelen/mcp-remote
0.16.6, a stdio-to-HTTP bridge): a `command` connector like any other, so its key is a stand-in.
Checked in its 0.16.6 dist: `--header` sets a header on every request, `--enable-proxy` uses
undici's `EnvHttpProxyAgent` (a `CONNECT` tunnel through `HTTPS_PROXY`), and Node trusts the
broker's CA through `NODE_EXTRA_CA_CERTS` (`agent::CA_ENV`). The stdio servers inherit the rpi
child's environment, proxy variables included: `rpi-mcp-adapter` spawns them with
`Command::new(command).args(args).envs(env)` and no `env_clear` (`transport.rs:78`).

| Entry | Package and command | Auth as checked | Held as | Hosts | Classes preset |
|---|---|---|---|---|---|
| `github` | github/github-mcp-server 2.1.0 (Go), `/usr/local/bin/github-mcp-server stdio` | `GITHUB_PERSONAL_ACCESS_TOKEN`, sent as `Authorization: Bearer` (`pkg/http/transport/bearer.go`) | stand-in | `api.github.com, raw.githubusercontent.com` (its REST, GraphQL and raw URLs, `pkg/utils/api.go`; `uploads.github.com` left out: release uploads only) | the gh gate's posts list, by tool: `issue_write`, `sub_issue_write`, `update_issue_comment`, `update_pull_request`, `merge_pull_request`, `pull_request_review_write`, `add_comment_to_pending_review`, `create_gist`, `update_gist`, `create_repository`, `fork_repository`, `delete_repository` = posts (none of these names hits the verb rule); `add_issue_comment`, `create_pull_request` already do |
| `notion` | `@notionhq/notion-mcp-server@2.6.1`, `npx -y @notionhq/notion-mcp-server@2.6.1` | `NOTION_TOKEN`, sent as `Authorization: Bearer`; axios ≥ 1.21 tunnels through `HTTPS_PROXY` with `https-proxy-agent` | stand-in | `api.notion.com` | `API-post-search = none` (a search the verb rule reads as a post: tool names are `API-<operationId>`, `src/openapi-mcp-server/openapi/parser.ts`); `API-post-page` and `API-create-a-comment` stay posts by the rule |
| `linear` | Linear's own server, `https://mcp.linear.app/mcp`, through `npx -y mcp-remote@0.16.6 https://mcp.linear.app/mcp --transport http-only --enable-proxy --header "Authorization: Bearer ${LINEAR_API_KEY}"` | Linear's docs: an API key in `Authorization: Bearer` instead of OAuth | stand-in | `mcp.linear.app` | none until the build lane lists its tools live (`mcp_list`) and records them; `create_comment`-style names hit the verb rule anyway |
| `home-assistant` | Home Assistant's built-in MCP Server integration, `/api/mcp` (Streamable HTTP), through mcp-remote as above with `{url}` and `--allow-http` | a long-lived access token in `Authorization: Bearer` (HA docs) | stand-in for an `https://` URL (Nabu Casa or the owner's TLS); an `http://` LAN URL needs `--plain`, since the proxy never swaps on plain HTTP and refuses a stand-in there (§4.21 `scan`), and a `network.allow` entry for `HOST:8123`, since the floor refuses private addresses (§4.20) | `{host}` | none. HA's tools are Assist intents (`HassTurnOn`, …), no gate class covers a lock or a garage door: the entry's `about` tells the owner to expose only safe entities to Assist in HA |
| `brave-search` | `@brave/brave-search-mcp-server@2.1.4`, `npx -y @brave/brave-search-mcp-server@2.1.4` (stdio is its default) | `BRAVE_API_KEY`, sent as `X-Subscription-Token` (`dist/BraveAPI`) | **plain**: §4.21 swaps a command connector's secret only in `Authorization`. `plain_reason` says so; the key spends search quota and reads nothing of the owner's | none (plain) | none: `brave_*` names hit no verb |
| `context7` | Upstash Context7, `url = "https://mcp.context7.com/mcp"` | none needed (a key only raises rate limits) | no secret, a plain tunnel | none | none |

Considered and left out:

- **Google Calendar / Gmail** (`@cocal/google-calendar-mcp` 2.8.1; taylorwilsdon/google_workspace_mcp
  2.4.1). Both run Google's OAuth themselves: the server writes its refresh token to a file in
  the agent's home and posts it to `oauth2.googleapis.com` in a request **body**, which the broker
  never swaps (§4.21), so nothing can be a stand-in; an unverified app's tokens expire every 7
  days; the first needs 241 MB of `node_modules`. Mail is already `himalaya`. Open question 3.
- Notion's and GitHub's remote servers need OAuth, which `[[mcp]]` cannot hold; the local ones above
  take a token.

#### 4.1.3 Runtimes and image cost

| Way | Cost | Used for |
|---|---|---|
| Prebuilt in the image, pinned by sha256 per arch like `gh` (`tools` stage) | `github-mcp-server` 29.4 MB (9.4 MB tarball; sha256 `a442f9db…` amd64, `34d5a268…` arm64) | `github`: a Go binary, not on npm |
| `npx -y PKG@EXACT` through the proxy at first call, cached in the agent's `~/.npm` | 0 MB in the image; measured installs: notion 47 MB, brave 25 MB, mcp-remote 8.7 MB | `notion`, `linear`, `home-assistant`, `brave-search` |
| Remote URL | 0 | `context7` |

Recommended as in the table. Prebuilding the npm servers would add 81 MB for a first call that is
a few seconds faster. The cache being the agent's to change gives it nothing: the server runs as
the agent and holds the same stand-in the agent can read in `mcp.json`, usable only toward the
bound host, where the agent could send it with `curl` anyway. The exact version pins the package,
not its dependency ranges; that drift is accepted for the same reason.

#### 4.1.4 Adding one

**Host:** `zephyr mcp add --catalog NAME [--url URL] [--plain] [--host HOST]..` (`host/setup.rs`
`mcp`, a new arm before the `[add, name]` one; `zephyr mcp catalog` lists `NAME: about`):

1. Read and parse the entry (§4.1.1); build `Mcp` with `Entry::to_mcp(url)`.
2. Refuse, with the reason, an entry with `plain_reason` or an `http://` URL without `--plain`
   (`… sends its key in X-Subscription-Token, which zephyr swaps only in Authorization: add --plain
   to hand it the real key, which the agent can then read`). `--plain` writes `secrets = "plain"`.
3. Merge with an existing connector of that name as `mcp add` does today (D4): the owner's
   `hosts` stay unless `--host` is given; `classes` are the entry's with the owner's value winning
   per tool.
4. Then the existing path: `broker::creates_pair`, and for each `[[secret]]` print its `help` and
   ask hidden, unless secrets.env has it (`--secret NAME` asks again); save, `h.apply()`.

**Chat and TUI:** `/mcp catalog` (each entry, and whether a connector of that name exists),
`/mcp catalog NAME` (the `[[mcp]]` it would write, its secrets and why it is held as it is), and
`/mcp catalog add NAME [URL] [plain]`, arms in `drafts::command`:

- Like `/mcp ok` it is an approval (`may_approve`, so a web decision needs a passkey), and it
  needs a **strong channel** always: every entry names a secret or presets a `none` class, and
  §4.21 already requires one for a draft that names a `${SECRET}`. Elsewhere: `adding a catalog
  connector decides where its secret goes; from tui, chat or passkey`.
- It writes through `drafts::write_config` (validate, `creates_pair`) and returns `After::Reload`.
  It never takes a secret's value: the reply names what secrets.env lacks and the host command
  that sets it (`zephyr mcp add github --secret GITHUB_TOKEN`), and the connector stays left out
  until then, as today.
- The agent cannot run it (`gateway` refuses `/mcp`). It can read `/opt/zephyr/connectors/` and
  suggest one; a draft it writes goes through `/mcp ok` as before.

#### 4.1.5 doctor

`doctor` adds one note per connector whose name is a catalog entry and whose `command`, `args`
or `url` differ from the entry's (`a newer catalog entry for github ships: zephyr mcp add --catalog
github`), and checks `/opt/zephyr/connectors` and `/opt/zephyr/skills` are root's and not
writable by the agent (`zephyr __netcheck` tries, as for the trust locations).

### 4.2 Starter output skills

#### 4.2.1 How they reach `<state>/skills` without `/skill ok`

The skills ship in `deploy/skills/NAME/`, copied to `/opt/zephyr/skills/` (root's, 0755/0644).
The trust argument is the image's: those files arrive with the zephyr binary and the compiled-in
`## Tools` block, from a path the agent cannot write, so they need no more approval than the
binary does. The agent's copies never count:

- **`drafts::seed(cfg)`**, new, run once per daemon start before the first `drafts::prepare`,
  under `drafts::lock()`. For each folder in `Config::starter_skills` (hidden, default
  `/opt/zephyr/skills`), `read_skill` checks it exactly as a draft (regular files, ≤ 32 files,
  ≤ 256 KB, frontmatter, no `.git`), and `<state>/skills-seeded.json` (root's 0600, `{NAME:
  digest}`, `drafts::digest` over the files) decides:
  - not recorded, and no approved skill of that name: `commit(state, NAME, Some(files), "seed")`
    (`skill: seed NAME`), then recorded;
  - not recorded, but the owner already has one by that name: recorded, nothing written;
  - recorded with another digest (a new image), and the approved copy still hashes to the
    recorded digest (untouched): committed as `skill: seed-update NAME`, recorded anew;
  - recorded, and the approved copy differs or is gone (the owner changed or removed it): nothing.
- The record is written only after the commit, so a crash between them seeds again, which the
  digest check makes a no-op.
- One notice, the first time anything is seeded, to the owner last answered, as the broker's
  upgrade notice is: `[zephyr] added the starter skills report, slides, pdf, charts; /skills lists
  them, /skill rm NAME removes one`.
- **Removing one:** `/skill rm NAME` (or `zephyr skill rm NAME`) commits the removal; the record
  stays, so no later start or upgrade adds it back. `/skill undo NAME` brings it back, as for any
  skill (`skill: seed …` starts with `PREFIX`, so `undo` already walks it).
- **The agent editing one is not trusted:** `drafts::prepare` rewrites `~/.approved/skills/` from
  `<state>/skills` at every child start (unchanged), and `seed` never reads the agent's home. An
  improvement the agent writes in `skills/drafts/NAME/` is a draft: shown as a diff against the
  seeded copy, approved only by `/skill ok`.

#### 4.2.2 The skills and what they need

None publishes anything: each writes files in the agent's work folder and delivers with
`send_file`; the SKILL.md says never to upload a result anywhere to share it.

| Skill | What it does | Needs | In the image? |
|---|---|---|---|
| `report` | Markdown (headings, tables, code, local images) → PDF: `python ~/.approved/skills/report/render.py IN.md OUT.pdf` turns it into HTML with `markdown-it-py` (`commonmark` with `table` and `strikethrough`) and the skill's `style.css`, then prints it with `"$AGENT_BROWSER_EXECUTABLE_PATH" --headless --no-sandbox --disable-gpu --user-data-dir=$(mktemp -d) --no-pdf-header-footer --print-to-pdf=OUT file://…` | Chromium; markdown-it-py; a Korean font | Chromium yes (both arches). markdown-it-py 4.2.0 is in `/opt/zephyr/py` only through `rich`: add it to `deploy/python/pyproject.toml` (0 MB). Font: **no**, only `fonts-dejavu-core`, so Hangul prints as boxes: add `fonts-nanum` (+33.3 MB) |
| `slides` | Markdown with `---` between slides → 16:9 PDF (`render.py --slides`: one `<section>` per slide, `@page { size: 13.333in 7.5in }`); a `.pptx` only when the owner asks, with `uv run --with python-pptx==1.0.2 python make_pptx.py` | as `report`; python-pptx on demand | python-pptx no: fetched by uv through the proxy on first use (+14 MB in the agent's uv cache) |
| `pdf` | read, merge, split, rotate, fill forms, encrypt with pypdf; text with layout `pdftotext -layout`; look at a page with `pdftoppm -png -r 100 -f N -l N` and `read` the PNG | pypdf; poppler-utils | pypdf yes. poppler-utils **no**: add it (+4.3 MB with libpoppler134; `poppler-data` stays out with `--no-install-recommends`) |
| `charts` | a PNG for chat or an SVG for a report: `uv run --with matplotlib==3.11.2 python chart.py`, Korean labels in NanumGothic | matplotlib, the font | matplotlib no: on demand (+58 MB in the uv cache) |

Pandoc is not used: Ubuntu's is 194.6 MB, and markdown-it-py plus the Chromium already in the
image do the same job for these outputs. Image growth as recommended: `fonts-nanum` 33.3 MB,
`poppler-utils` 4.3 MB, `github-mcp-server` 29.4 MB, about **67 MB** on 6.1 GB (+1.1%). Baking
matplotlib and python-pptx would add 72 MB more (open question 2). `fonts-nanum` also fixes
Hangul in screenshots of Korean pages, and adds those fonts to the browser's font list, as on any
Korean Linux desktop.

### 4.3 Browser pacing

#### 4.3.1 The setting

`browser.pace_ms` (`[browser] pace_ms`, a new `Option<Browser>` on `Config`, `deny_unknown_fields`):

| Key | Section | Takes | Agent | Loosening needs a strong channel |
|---|---|---|---|---|
| `browser.pace_ms` | Browser | 0 to 60000; 0 is off; higher is stricter (`Stricter::Higher`); unset reads the default (open question 1) | tighten | no |

- The gate reads it from the live config each request (`Conn.cfg`), so `reload::diff` treats a
  change as read-in-place: no restart.
- **Rollback.** The default lives in code, and no upgrade writes `[browser]`. Only an owner who
  sets the key gets the table, which a 0.9.0 image pinned back by hand refuses (its `Config` is
  strict); `zephyr upgrade 0.9.0` restores the `zephyr.toml` it backed up and is unaffected. The
  README's upgrade notes say so, as for `[[mcp]] hosts` in 0.9.0.

#### 4.3.2 What counts, and where

Per **registrable domain** (eTLD+1): an account, its sign-in host and its CDN-less subdomains are
one site to the site's own bot checks (`www.amazon.co.uk`, `smile.amazon.co.uk`). Per origin would
let `accounts.` and `www.` go at twice the rate. Computed with the **`psl` crate** (2.1.x,
MIT/Apache, `no_std`, the public suffix list compiled in, one dependency `psl-types`): the wrapper
reads no config and no file, so the list cannot come from disk, and a hand-written "last two
labels" rule would make every `.co.kr` shop one site. An IP address or a name with no known
suffix is its own site; `file:`, `about:`, `data:` and `chrome:` URLs are not paced.

A new `browser_gate::paced(rest) -> Option<Paced>` reads the agent's command after `split`:

| Command (0.41.2's parser, `cli/src/commands.rs`) | Site |
|---|---|
| `open`/`goto`/`navigate URL` (a bare `example.com` read as `https://`, as 0.41 does), `tab new [--label X] URL`, `read URL` | the URL's |
| `back`, `forward`, `reload` | the current page's (`get url`) |
| `click`, `dblclick`, `tap`, `find … click`, and the key presses the wrapper already reads (`Act::Key`: Enter, Space, a line break typed) | the current page's (already read for classing) |
| `batch` | each command above in it, counted |
| everything else (`snapshot`, `get`, `screenshot`, `scroll`, `fill`, `type` without a line break, `eval`, `wait`, the wrapper's own reads) | not paced |

#### 4.3.3 State: in the gate, shared by every wrapper

The wrapper is a fresh process per command, main's and every task's, and holds no config. The
gate is already the one daemon-side place every wrapper talks to, as the agent's uid only, so the
state lives there: `gate::Pace`, a `Mutex<HashMap<String, Instant>>` of each site's next free
moment, made once in `gate::serve` and shared by its `Conn`s, as `desktop` is.

- **Request** on the gate socket: `{"v": 1, "pace": "amazon.co.uk", "count": 1, "session": …}`.
  **Answer, at once:** `{"verdict": "allow", "wait_ms": N}`, or `deny` with a text. The gate
  **reserves** the slot as it answers: `slot = max(now, next[site])`, `next[site] = slot +
  pace × (1 + r)` with `r` uniform in [0, 0.5) from `ring::rand::SystemRandom` (already a
  dependency), `wait_ms = slot − now`. Reservation, not a timestamp of the last action, is what
  makes two wrappers started together (two tasks on one site) queue instead of both going.
- The wrapper closes the connection, then sleeps `wait_ms`: a waiting command holds none of the
  gate's 32 places.
- `deny` when `count > 1` and pacing is on (`not run: with browser pacing on, a batch takes at most
  one navigation or click; run the others as separate commands`), and when the slot is over
  120 s away (`zephyr: too many browser actions are queued for SITE; wait, then try again`),
  reserving nothing. `pace_ms = 0` answers `wait_ms: 0` and stores nothing. Entries whose moment
  has passed are dropped on each request, so the map holds only sites in use.
- Not audited: a row per navigation would flood the audit, and pacing decides nothing about what
  may run.
- **No gate answering:** the command runs unpaced. Pacing is a courtesy to sites, not a guard; a
  navigation does not need the gate today, and refusing all of them when the gate is down would be
  a regression. Clicks the gate must judge are refused as before.

#### 4.3.4 Order in `decide`, with the desktop lock and the approvals gate

1. `held(env)` (unchanged): a held desktop refuses before anything else, so nothing is reserved.
2. Classing and the gate's verdict (unchanged). A click held for `/ok` waits there; its minutes of
   waiting count for nothing.
3. `taken` (unchanged), then **`pace`**, then `unchanged` (the page read again) and `go`. Pacing
   comes before the re-read, so what the owner approved is checked as close to the click as it is
   now. A command the gate lets through at once, and a paced `Act::Pass`, run `pace` then `go`.
4. `go` takes the place on the desktop (`place`, the shared `flock` on `desktop.lock`) only after
   the sleep. A pacing wrapper therefore holds no place, a take's `Control::drain` never waits for
   it or kills it, and when the owner takes the desktop during the sleep, `place` sees the flag and
   asks the gate as any wrapper does: the command does not run. Its reservation is spent, which
   only spaces the next command.

## Open questions for the owner

1. **Pacing default.** (a) On at 1500 ms (1.5 to 2.25 s per site). (b) Off until set.
   Recommend (a): the model's own turn already takes seconds between commands, so it bites only on
   loops, scripts and batches, which is exactly the bot-like pattern; it changes behaviour on
   upgrade, said in the release notes.
2. **Python libraries for slides and charts.** (a) On demand with `uv run --with` pinned
   (0 MB, needs the network on first use). (b) Baked into `/opt/zephyr/py` (+72 MB).
   Recommend (a): the fonts and poppler, which cannot come per script, are baked (67 MB in all).
3. **Google Calendar and Gmail.** (a) Leave them out: their refresh tokens cannot be stand-ins and
   live in the agent's home. (b) Add one as a `--plain` entry with that warning.
   Recommend (a); mail stays himalaya.
4. **Starter skills on an upgraded install.** (a) Seeded at the first start of this release, as on
   a new install, with the one notice. (b) Only offered (`/skill add NAME` from the image).
   Recommend (a): they are image content, removable for good with `/skill rm`.
5. **Remote connectors through mcp-remote.** (a) As in §4.1.2: the key is a stand-in, at the cost
   of an npm bridge per connector. (b) Plain `url` connectors, the key readable in `mcp.json`.
   Recommend (a) for every remote entry with a key.

## Tests

**Catalog**
- `catalog.rs` unit: every file in `deploy/connectors/` parses; its name is valid; `to_mcp` passes
  `Mcp::check` and `Config::validate`; no entry carries `secrets`; every command entry with a
  secret has hosts or a `plain_reason`; no secret is `ZEPHYR_*` or a channel's; `{url}` is
  required exactly where `needs_url` says, and an unknown key in a test file is refused.
- `tests/host.rs`: `mcp add --catalog github` (the fake docker serves the entry) writes the table,
  asks `GITHUB_TOKEN` once, and a second add keeps the owner's hosts and per-tool classes;
  `brave-search` and an `http://` home-assistant are refused without `--plain` and written with it.
- `tests/router.rs`: `/mcp catalog add github` from Discord is refused (strong channel), from
  `chat` it writes the table, restarts the child and names the missing secret; with the secret
  set, `/mcp` lists it published.
- **A command connector's secret never reaches the agent:** `drafts.rs` unit, beside the
  `BROKERED` cases: with secrets.env `GITHUB_TOKEN=real-gh-token` and the floor on, `prepare`
  writes `mcp.json` whose `env.GITHUB_PERSONAL_ACCESS_TOKEN` is `broker::surrogate(env,
  GITHUB_TOKEN)` and which holds `real-gh-token` nowhere; `/mcp catalog NAME` and `/mcp` print no
  value. Ignored real-container test (`tests/token_broker.rs`, `ZEPHYR_TEST_GITHUB_TOKEN`): as the
  agent, `mcp.json`, `/proc/<server pid>/environ` and a `grep -r` over `/zephyr` hold the stand-in
  and never the token; `mcp_call github get_me` succeeds; the audit has `swap allowed GITHUB_TOKEN
  -> api.github.com:443`; and the same stand-in sent with `curl` to `httpbin.org` gets
  `403 … not sent to httpbin.org`. Likewise `linear` through mcp-remote with
  `ZEPHYR_TEST_LINEAR_KEY`, which also records its tool names for the entry.

**Starter skills**
- `drafts.rs` unit for `seed`: a first start commits each skill once and records it; a second
  commits nothing; `/skill rm report` then a start does not bring it back, and `/skill undo
  report` does; a changed image updates an untouched copy (`skill: seed-update`) and leaves one the
  owner replaced with `/skill add`; a name the owner already had is left alone; a skill failing
  `read_skill` is logged and not seeded.
- **A skill the agent edits is not trusted:** the test writes, as the agent, a changed
  `~/.approved/skills/report/SKILL.md` and a `skills/drafts/report/`; the next `prepare` restores
  the seeded file, AGENTS.md's `## Skills` line keeps the seeded description, a second `seed`
  with those files in the home commits nothing (it reads only `Config::starter_skills` and
  `<state>`), and the draft waits for `/skill ok` with a diff against the seeded copy.
- Ignored container test: `report` renders a Korean markdown file with a table to a PDF whose
  `pdftotext` holds the Hangul; `slides` gives one page per slide; `pdf` merges two files; `doctor`
  shows `/opt/zephyr/skills` and `/opt/zephyr/connectors` not writable by the agent.

**Pacing**
- `gate.rs` unit for `Pace` (clock and random draw passed in): gaps on one site lie in
  [pace, 1.5 × pace); two sites do not wait on each other; `count: 2` and a slot past 120 s are
  refused without reserving; `0` reserves nothing; old entries are dropped.
- `browser_gate.rs` unit for `paced`: the table in §4.3.2, including `open example.com`,
  `tab new --label x URL`, `read` with and without a URL, `www.amazon.co.uk` and
  `smile.amazon.co.uk` as one site, `shop.co.kr` sites apart, an IP, `file:` not paced.
- **Pacing holds across separate wrapper processes** (`tests/cli.rs`, a daemon with `[browser]
  pace_ms = 300`, the fake real binary logging a timestamp per command): eight wrapper processes
  started at once, five on `a.example.com`/`www.example.com` and three on `other.test`; the log's
  gaps within `example.com` are each ≥ 300 ms, `other.test`'s first command does not wait behind
  them. Only lower bounds are asserted, which a slow machine cannot break.
- Desktop: a wrapper sleeping out its pace while the owner takes the desktop does not delay the
  take's drain, and then exits 1 with the owner-has-the-desktop text, the real binary not run.
- No gate: a navigation runs at once; a click is refused as today.
- `settings.rs`: the row parses; the agent raising `browser.pace_ms` applies at once and lowering
  it is a proposal. `config.rs`: no `[browser]` reads the default; `/set browser.pace_ms 2000`
  writes the table, and nothing else does.

## Decided with the owner (2026-11-12)

Every open question above takes its recommended answer. Where this section and the body differ, this section wins:

- **Pacing:** on by default at 1500 ms.
- **Slides and charts:** Python libraries are fetched on first use (`uv run --with`).
- **Gmail and Google Calendar:** left out of the catalog.
- **Starter skills:** seeded on an upgraded install at first start, with one notice.
- **Remote catalog connectors:** run through `mcp-remote`, so their keys are surrogates.
