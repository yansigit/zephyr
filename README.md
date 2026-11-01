# Zephyr

An always-on personal agent gateway over upstream `rpi --mode rpc`, running in one Docker container.
It talks to you through its own terminal UI (or the stock rpi TUI), a line chat, Discord and iMessage. See [DESIGN.md](DESIGN.md).

## Quick start

You need Docker and, for the image from GitHub, the `gh` CLI signed in to an account that can read `yansigit/zephyr`.

```sh
cargo install --path crates/zephyr   # the same binary runs on the host and in the container
zephyr init                          # the wizard; safe to run again, it offers the current values
```

`zephyr init` checks Docker, asks for the projects folder the agent may work in, pulls the image
(`docker login ghcr.io` with your `gh` token when needed; it builds locally when it cannot pull),
starts the container, signs in the providers you pick, sets the failover order and the channels,
then runs `zephyr doctor`.

Providers: OpenCode Go, Antigravity, ChatGPT/Codex, Claude (through the Claude Agent SDK), Cursor
(chat only) and the Anthropic API key. Each sign-in is an account under `~/.zephyr/accounts/<name>/`,
listed in `[[account]]` in `~/.zephyr-daemon/zephyr.toml`; when one account is rate limited, the next takes over.

Without prompts:

```sh
zephyr init --non-interactive --projects ~/Projects \
  --account codex-main=openai-codex/gpt-5.5 --model openai-codex/gpt-5.5
zephyr login openai-codex --account codex-main
```

## Everyday commands

| Command | What it does |
|---|---|
| `zephyr up [--build]` / `down` / `restart` / `upgrade` | Start (building the image locally with `--build`), stop, restart, pull the new image and restart |
| `zephyr status` / `logs [-f]` / `doctor [--fix]` | Container and daemon state, logs, checks (and repairs) |
| `zephyr login <provider> [--account NAME]` | Sign in an account, then make a small test call (`--no-verify` skips it) |
| `zephyr accounts [list \| order NAME.. \| remove NAME]` | The failover pool |
| `zephyr model [--account NAME] [PROVIDER/MODEL]` | An account's model (or the default without accounts); with no model, pick from `rpi --list-models` |
| `zephyr channel add discord\|imessage` | Channel settings; secrets are asked for hidden. Discord shows typing while the agent works, and a progress note after 20 s (`discord.progress`, `discord.progress_after_secs`) |
| `zephyr config get\|set KEY [VALUE]` | Edit `zephyr.toml` (comments are kept; invalid keys are refused) |
| `zephyr attach [--stock]` | Zephyr's terminal UI on the live session: history, tool calls, what the agent is doing, the queue and today's spend; `^D` detaches and the agent keeps running. `--stock` runs the stock `rpi --connect` client instead |
| `zephyr chat` | The same UI on a terminal; a line client (`< reply` per line) when piped or scripted |
| `zephyr pause` / `resume` | Stop and restart dispatch (`/pause` and `/resume` from any channel do the same) |
| `zephyr reload` | Apply hand edits now instead of within 5 s (`/reload` from any channel) |
| `zephyr uninstall [--keep-data]` | Remove the container and image, and `~/.zephyr` unless kept |

Sign in from the running gateway too, with no re-exec afterwards. In `zephyr attach`, `/login`
lists the providers and Enter starts the one you pick: an OAuth provider (Antigravity,
ChatGPT/Codex) prints a URL to open in a browser on your machine; nothing else is needed. The
others ask for a key or a token, which is typed masked, goes to the sign-in once and is never
shown or kept. From any channel, `/login PROVIDER [ACCOUNT] [MODEL]` can start a sign-in too, but
only an OAuth one: a `key` or a token provider is refused there, so use `zephyr login` on the host,
or the attach panel. `/login cancel [ID|PROVIDER]` stops a sign-in you have given up on, so its slot
in the eight that may wait at once is free again. The account joins the pool at once; `/model`
selects it.

## Changing settings

Changes apply by themselves; nothing needs `zephyr restart`. The commands above, and hand edits to
`zephyr.toml` or `secrets.env` (seen within 5 s), go to the running daemon, which says what it did:

| Change | Applies |
|---|---|
| Budget, owners, time zone, `[auth]`, the pool order | at once |
| The model, on the same provider | at once, or after the reply in progress (rpi's `set_model`) |
| A model on another provider, the active account, extensions, a re-login | the agent restarts after the reply in progress; the conversation carries on |
| Discord, iMessage, `[attach]`, `[agent]`, a secret | the daemon restarts itself in place once idle; waiting messages are kept |

A file that does not load is not applied, and the daemon keeps its current settings. From any
channel: `/model` shows the models, `/model PROVIDER/MODEL` switches (to the account on that
provider), `/status` shows the agent's account and model, `/reload` re-reads the files, and `/new`
starts a fresh conversation (memory and rules carry over; the old one stays in `sessions/`).
`/login` signs an account in from here: `/login` lists the provider ids, `/login PROVIDER [ACCOUNT]
[MODEL]` starts an OAuth sign-in and needs no re-exec (a key or a token is refused — it is typed in
`zephyr attach`, which masks it), and `/login cancel [ID|PROVIDER]` stops the ones you gave up on.
The first account of an empty install does restart the agent once, when idle: there is nothing for
it to start on until that account exists.

`ZEPHYR_CONTAINER` (default `zephyr`) and `ZEPHYR_IMAGE` choose another container or image, for
example to test beside a live install.

## What you are told

- After a turn that ran a command sending data off the machine (curl or wget with a body, ssh, scp,
  rsync to a host, nc, `git push`, ...), zephyr lists them: `[zephyr] outbound this turn: ...`.
  Jobs report theirs on the job's channel. Plain downloads are not listed.
- Notices go to the channel you last wrote from. Under `zephyr attach` they appear as messages in
  the conversation, or at your next attach if none is open.
- When the agent changes its memory (`memory/USER.md`, `MEMORY.md`), you get the diff. The change
  reaches the agent's context only after `/memory ok` (if it changed after you were sent the diff,
  you are shown the new one first); `/memory undo` drops it, `/memory diff` shows
  it again, `/memory log` lists commits. A web page cannot plant a lasting "preference" without you.
  For the same reason rpi skills are off, only the image's extensions load, and a `SYSTEM.md`, `APPEND_SYSTEM.md`, `CLAUDE.md` or
  extra `AGENTS*.md` the agent writes is renamed `*.ignored-by-zephyr` before the next start.

## Limits

`[budget]` in `zephyr.toml`, all optional: `daily_tokens` and `daily_cost_usd` pause the gateway
for the rest of the UTC day (`/resume` overrides); `run_turns` and `run_minutes` stop one run that
goes on too long, and you are told why. A job run also stops after 30 minutes.

## Standing rules

`/rule never push to main` adds a rule the agent sees first in every session, above its memory,
from the next message on. `/rules` lists them, `/rule rm 2` removes one. Rules live with the
daemon, out of the agent's reach, and do not fade when a long conversation is summarised the way
instructions given in chat can. `/new` starts a fresh conversation; rules and memory carry over.

## Where things live

- `~/.zephyr/` (mode 0700), seen as `/zephyr` in the container: the agent's home, with sessions,
  memory and `accounts/<name>/auth.json`. The agent runs as you, so what it makes is yours.
- `~/.zephyr-daemon/` (root's, 0700), seen as `/var/lib/zephyr`: the daemon's own files, which the
  agent cannot read or change. The daemon runs as root without file overrides (only SETUID, SETGID
  and KILL), `no-new-privileges`, at most 4096 processes. `zephyr config`, `model`, `channel` and
  the rest edit these files through the container; to read one yourself, use `sudo`.
  - `zephyr.toml`: the settings.
  - `secrets.env` (0600): `KEY=value` lines, such as bot tokens. The daemon reads it at start and
    again after a change, and a variable already set in its environment wins. The agent never sees
    them: they are removed from rpi's environment and every other process zephyr starts for it. To
    give the agent one (an API key a provider reads, say), list its name in `[agent] env = ["NAME"]`.
  - `tui.token`: the `zephyr attach` token.
  - `host.toml`: the projects folder, optionally `memory = "8g"` to cap the container's memory, and `offered_extensions`: the image extensions `init`, `up` and `upgrade` have already added to `zephyr.toml`, so one you remove stays removed. The first `up` after upgrading to a zephyr that keeps this record adds once any image extension removed earlier, or every one if the config lists none; after that, a removal sticks.
    Used only on the host, at `zephyr up`.
  - `state.db`: inbox, outbox, jobs, spend.

An install from before this layout is moved over by the next `zephyr up`.

## Claude subscription

The Claude provider runs Anthropic's official Claude Agent SDK, signed in with your own token from
`claude setup-token` (`zephyr login claude-code` runs it in the container). Anthropic's terms govern
using a subscription unattended; check that your use fits them.

## Building the image

`zephyr up --build` builds `deploy/Dockerfile` from pinned sources: this repo, `rpi-providers` from
`yansigit/rpi-package`, `rpi-cursor-lite` and `rpi-claude-agent` from `yansigit/rpi-provider-plugins`,
and the upstream `rpi` 0.15.0 release (sha256 checked). Your `gh auth token` goes in as the BuildKit
secret `gh_token`, which is never stored in a layer. The provider sources are pinned to commit SHAs
by the build args (`RPI_PACKAGE_REF`, `PLUGINS_REF`); local checkouts can stand in for them with
`--build-context rpi-package=<dir> --build-context provider-plugins=<dir>` (see the Dockerfile).
Pushing a `v*` tag publishes `ghcr.io/yansigit/zephyr:<version>` (`.github/workflows/image.yml`).
