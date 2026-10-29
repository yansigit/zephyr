# Zephyr

An always-on personal agent gateway over upstream `rpi --mode rpc`, running in one Docker container.
It talks to you through a terminal chat, the stock rpi TUI, Discord and iMessage. See [DESIGN.md](DESIGN.md).

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
listed in `[[account]]` in `~/.zephyr/zephyr.toml`; when one account is rate limited, the next takes over.

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
| `zephyr model [PROVIDER/MODEL]` | The default model; with no argument, pick from `rpi --list-models` |
| `zephyr channel add discord\|imessage` | Channel settings; secrets are asked for hidden |
| `zephyr config get\|set KEY [VALUE]` | Edit `zephyr.toml` (comments are kept; invalid keys are refused) |
| `zephyr chat` / `attach` | Line chat, or the stock rpi TUI attached to the live session |
| `zephyr pause` / `resume` | Stop and restart dispatch (`/pause` and `/resume` from any channel do the same) |
| `zephyr uninstall [--keep-data]` | Remove the container and image, and `~/.zephyr` unless kept |

Settings that change the daemon apply after `zephyr restart`. `ZEPHYR_CONTAINER` (default `zephyr`)
and `ZEPHYR_IMAGE` choose another container or image, for example to test beside a live install.

## Where things live

- `~/.zephyr/` (mode 0700), seen as `/zephyr` in the container: `zephyr.toml`, `state.db`, sessions,
  memory, `accounts/<name>/auth.json`.
- `~/.zephyr/secrets.env` (0600): `KEY=value` lines, such as bot tokens. The daemon reads it at start,
  and a variable already set in its environment wins.
- `~/.zephyr/tui.token` (0600): the `zephyr attach` token.
- `~/.zephyr/host.toml`: the projects folder, used only on the host.

## Claude subscription

The Claude provider runs Anthropic's official Claude Agent SDK, signed in with your own token from
`claude setup-token` (`zephyr login claude-code` runs it in the container). Anthropic's terms govern
using a subscription unattended; check that your use fits them.

## Building the image

`zephyr up --build` builds `deploy/Dockerfile` from pinned sources: this repo, `rpi-providers` from
`yansigit/rpi-package`, `rpi-cursor-lite` and `rpi-claude-agent` from `yansigit/rpi-provider-plugins`,
and the upstream `rpi` 0.12.0 release (sha256 checked). Your `gh auth token` goes in as the BuildKit
secret `gh_token`, which is never stored in a layer. The provider branches are build args
(`RPI_PACKAGE_REF`, `PLUGINS_REF`); local checkouts can stand in for them with
`--build-context rpi-package=<dir> --build-context provider-plugins=<dir>` (see the Dockerfile).
Pushing a `v*` tag publishes `ghcr.io/yansigit/zephyr:<version>` (`.github/workflows/image.yml`).
