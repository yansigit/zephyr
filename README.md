# Zephyr

An always-on personal agent gateway over upstream `rpi --mode rpc`, running in one Docker container.
It talks to you through its own terminal UI (or the stock rpi TUI), a line chat, Discord, iMessage, Telegram and Signal. See [DESIGN.md](DESIGN.md).

## Quick start

You need Docker and, for the image from GitHub, the `gh` CLI signed in to an account that can read `yansigit/zephyr`.

From a clone, as your own user (no `sudo`):

```sh
deploy/install.sh                    # installs the CLI, then runs zephyr init
```

It copies the CLI out of `ghcr.io/yansigit/zephyr:latest` into `~/.local/bin/zephyr` (Linux
x86_64; a failed pull logs in to ghcr.io with your `gh` token), or builds it with
`cargo install --locked --path crates/zephyr` when the pull fails and `cargo` (1.91 or newer) is
there; `--from-image` or `--from-source` picks one, `--no-init` stops before the wizard, and
arguments after `--` go to `zephyr init`. It checks Docker answers as you and warns when Docker's
disk has under 20 GB free. It installs no Docker or Rust (it names their install pages), never
runs `sudo`, `curl` or `wget`, and writes nothing but the one binary. On a machine that already
has zephyr (the container, `~/.zephyr-daemon`, `~/.zephyr` or a `~/.local/bin/zephyr`), it refuses:
`zephyr upgrade` is the way there. With `ZEPHYR_REPO` set, a CLI built from source upgrades from that
repository; one copied from the image upgrades from the repository the image was built for, so
keep `ZEPHYR_REPO` set (install.sh says so) unless that image was built with `ZEPHYR_BUILD_REPO`.

Or by hand, either of:

```sh
cargo install --path crates/zephyr   # the same binary runs on the host and in the container
zephyr init                          # the wizard; safe to run again, it offers the current values
```

```sh
# Without Rust (Linux x86_64; while the package is private, `docker login ghcr.io` first)
docker run --rm --entrypoint cat ghcr.io/yansigit/zephyr:latest /usr/local/bin/zephyr > ~/.local/bin/zephyr && chmod +x ~/.local/bin/zephyr
```

`zephyr init` checks Docker, asks for the projects folder the agent may work in, pulls the image
(`docker login ghcr.io` with your `gh` token when needed; it builds locally when it cannot pull),
starts the container, signs in the providers you pick, sets the failover order and the channels,
asks which approvals preset to start in (`manual`, `semi` or `full`; `[approvals] preset` in
`zephyr.toml`), whether to add private facts (hidden prompts, as `zephyr facts add`) and whether to
set up the [web app](#web-app) (it needs Tailscale with HTTPS certificates), then runs `zephyr
doctor`. Adding iMessage offers the Tailscale forward to its webhook, and a port init added is
published by starting the container again. It prints the network mode (`--network host` turns
the agent's network floor off). Run again with Enter at every prompt, it changes nothing. A new install without a preset starts in `manual`. An install made before approvals
keeps sending as it did: its first start on this version writes `preset = "full"` and tells you once;
`/approvals manual` turns them on.

Providers: OpenCode Go, Antigravity, ChatGPT/Codex, Claude (through the Claude Agent SDK), Cursor
(through its agent service, tools included) and the Anthropic API key. Each sign-in is an account
under `~/.zephyr/accounts/<name>/`, listed in `[[account]]` in `~/.zephyr-daemon/zephyr.toml`; when one account is rate limited, the next takes over.

Without prompts:

```sh
zephyr init --non-interactive --projects ~/Projects \
  --account codex-main=openai-codex/gpt-5.5 --model openai-codex/gpt-5.5 --preset manual
zephyr login openai-codex --account codex-main
```

`--preset manual|semi|full` sets the approvals preset as `/approvals PRESET` does (a class you set
by hand that is looser than the preset is cleared); without it a non-interactive `init` leaves
`[approvals]` out (which is `manual`) and asks for no facts.

## Self-hosting

Zephyr runs on your own machine with nothing of this project's owner in it but the defaults below,
each of which you can change.

**What you need.** Linux on x86_64 (arm64 only from source: the published image is amd64, and the
Claude Agent SDK path in the image is the x64 one), Docker with BuildKit that you run as your own
user (`docker version` works without `sudo`), and about 20 GB of disk. Optional: Tailscale (for the
web app and iMessage) and `gh` signed in (for a private image on ghcr.io).

**Get the CLI.** From a clone, `deploy/install.sh` (see [Quick start](#quick-start)), or
`cargo install --locked --path crates/zephyr`.

**Get the image.** `zephyr init` pulls `<repo>:<CLI version>` if you can read the registry.
Otherwise build it: `zephyr up --build` builds `deploy/Dockerfile` here, and since the provider
sources are public it needs no token. For a smaller image without the owner's provider sources, see
[Building the image](#building-the-image) and what it leaves out. To use your own registry, build
the CLI with it (`ZEPHYR_BUILD_REPO=registry.example.com/you/zephyr cargo install --locked --path
crates/zephyr`), build the image with `--build-arg ZEPHYR_BUILD_REPO=registry.example.com/you/zephyr`,
and push it there as `<repo>:<version>` and `<repo>:latest`. Run `docker login` for that registry
yourself: your `gh` token only ever goes to ghcr.io.

**`zephyr init`, step by step.**

1. *Projects folder*: the one folder of yours the agent may work in (besides its own home).
2. *Providers and failover*: pick the providers to sign in. Each sign-in is an account, and when
   there are several you set the order they take over in.
3. *Discord*: create an application in the Discord Developer Portal, add a bot, and copy its
   token (init asks for it at a hidden prompt). Invite the bot to a server you are in, so you can
   send it direct messages; it reads DMs only. Your user id comes from Developer Mode, then Copy
   User ID. DMs you send while zephyr is down are picked up when it is back (as on iMessage:
   the last day, at most 200; `[discord] lookback_hours` and `backfill_limit`),
   except that an `/ok`, `/no`, `/stop`, `/pause` or `/vault grant` sent over 10 minutes before
   is not carried out: zephyr says it skipped it.
4. *iMessage*: a Mac running the BlueBubbles server, on your tailnet. Init asks for the webhook
   address (this machine's Tailscale IP and a port), the server URL, your iMessage handles and the
   server password, then offers the Tailscale forward to the webhook and prints the webhook URL to
   register in BlueBubbles.
5. *Approvals preset*: how much the agent may send without asking you ([Approvals and
   presets](#approvals-and-presets)).
6. *Private facts*: values the agent may not send unasked.
7. *Web app*: needs Tailscale with HTTPS certificates; a yes runs `zephyr web setup`
   ([Web app](#web-app)).
8. *Network*: the agent's network floor is on by default; `zephyr init --network host` turns it
   off, and `--network bridge` turns it back on ([The agent's network](#the-agents-network)).

**Upgrading.** With a registry: `zephyr upgrade`. From source: `git pull && cargo install --locked
--path crates/zephyr && zephyr up --build`.

**Your data, backups, removal.** See [Where things live](#where-things-live), [When something goes
wrong](#when-something-goes-wrong) and `zephyr uninstall [--keep-data]`.

**The knobs**, each with its default:

| Knob | Where | Default |
|---|---|---|
| `ZEPHYR_REPO` | environment, at run time | the build-time value, else `ghcr.io/yansigit/zephyr` |
| `ZEPHYR_BUILD_REPO` | environment of `cargo install`, baked into the CLI | unset: `ghcr.io/yansigit/zephyr` |
| `ZEPHYR_IMAGE` | environment | unset: `<repo>:<CLI version>`. Set, one exact image, with no versioned upgrades |
| `ZEPHYR_CONTAINER` | environment | `zephyr` |
| `ZEPHYR_SRC` | environment, for `zephyr up --build` | the source tree the CLI was built from |
| `RPI_PACKAGE_REPO`, `RPI_PACKAGE_REF` | `deploy/Dockerfile` build args | `yansigit/rpi-package` at a pinned commit; an empty repo leaves it out |
| `PLUGINS_REPO`, `PLUGINS_REF` | `deploy/Dockerfile` build args | `yansigit/rpi-provider-plugins` at a pinned commit; an empty repo leaves it out |
| `ZEPHYR_BUILD_REPO` | `deploy/Dockerfile` build arg, baked into the image's CLI | unset: the CLI's default (image.yml sets `ghcr.io/<repository>`) |
| `RPI_BIN` | `deploy/release.sh` and the ignored real-rpi tests | the pinned rpi path in `release.sh` |
| `ZEPHYR_GH_REPO` | `deploy/runner/start.sh` | `yansigit/zephyr` |

## Everyday commands

| Command | What it does |
|---|---|
| `zephyr init [--non-interactive --projects DIR --provider ID.. --account NAME=PROVIDER/MODEL.. --model P/M --preset manual\|semi\|full --network bridge\|host --no-start]` | The setup wizard (see [Quick start](#quick-start)); `--preset` picks the [approvals](#approvals-and-presets) preset to start in, and a re-run keeps the one you have unless it is given; `--network host` turns [the agent's network floor](#the-agents-network) off and `--network bridge` back on (a re-run without it keeps the mode, and init prints which one runs) |
| `zephyr up [--build]` / `down` / `restart` | Start (building the image locally with `--build`), stop, restart |
| `zephyr upgrade [VERSION]` | Move to the newest release (updating this CLI from the image first), once the agent is idle, after backing up `state.db`, `zephyr.toml`, `vault.toml` and `host.toml` to `~/.zephyr-daemon/<file>.<old version>.bak` (the newest 5 versions are kept). With an older VERSION, go back to it by restoring that version's backups. A release is always its published image: a local copy is used, else it is pulled, never built |
| `zephyr status` / `logs [-f]` / `doctor [--fix]` | Container and daemon state (heartbeat, whether the agent is up, the oldest waiting message, replies pending and failed, and counts of job runs, tasks and processes over the last 90 days; exits non-zero when the daemon is down or hung), logs, checks (and repairs) |
| `zephyr alert setup TOPIC [--server URL] \| check \| test \| off` | Outage alerts: a systemd user timer on this machine checks every 5 minutes and posts to an [ntfy](https://ntfy.sh) topic when zephyr goes down, comes back, or more replies fail to send. Run `loginctl enable-linger $USER` so it runs while you are logged out |
| `zephyr --version` | This CLI's version and the running container's, with a hint when they differ |
| `zephyr login <provider> [--account NAME]` | Sign in an account, then make a small test call (`--no-verify` skips it). A new NAME is `[a-z0-9][a-z0-9_-]*`, as `/login` takes it (also for `init --account NAME=MODEL`; an account already in zephyr.toml keeps its name), and `--model` is PROVIDER/MODEL; what is not is refused before anything is signed in or written |
| `zephyr accounts [list \| order NAME.. \| remove NAME]` | The failover pool |
| `zephyr model [--account NAME] [PROVIDER/MODEL]` | An account's model (or the default without accounts); with no model, pick from `rpi --list-models` |
| `zephyr channel add\|remove discord\|imessage\|telegram\|signal \| list` | Channel settings (secrets are asked for hidden); `remove` drops the channel, its owners and secrets, `list` shows them. Discord and Telegram show typing while the agent works, and a progress note after 20 s (`discord.progress`, `discord.progress_after_secs`, the same keys under `[telegram]`). `add telegram` asks for the bot token from @BotFather and `add signal` registers or links zephyr's own number in the container; both then ask the daemon for a `/pair` code and print it, which you send from your own Telegram or Signal account. `remove signal` also unregisters the number and deletes its keys |
| `zephyr channel signal trust ACI` | After an owner's Signal safety number changed (a new phone) and you compared it: signal-cli sends to them again |
| `zephyr web setup [--yes]` / `web enrol [NAME]` | Put the [web app](#web-app) behind `tailscale serve` and add a first device; later, a one-time code for another device |
| `zephyr skill add DIR \| list \| rm NAME` | Skills the agent may use (see [Skills and connectors](#skills-and-connectors)); one you add is approved at once |
| `zephyr mcp add NAME (--url URL [--header K=V].. \| -- CMD ARGS..) [--secret NAME].. [--host HOST].. [--plain] \| list \| rm NAME` | Connectors (MCP servers); each `--secret` is asked for hidden and fills in `${NAME}`; `--host` names where a secret is sent, `--plain` hands the connector the real one |
| `zephyr mcp add --catalog NAME [--url URL] [--host HOST].. [--plain] \| zephyr mcp catalog` | A ready-made connector from the image's catalog, its secret asked for hidden; `catalog` lists them |
| `zephyr packages add NAME.. \| rm NAME.. \| list` | Ubuntu (apt) packages for the agent, baked into a local image (`zephyr-local:<version>-<hash>`) on the release one; `up` and `upgrade` build it, and `add`/`rm` rebuild and recreate the container once the agent is idle |
| `zephyr facts list \| add NAME --kind text\|digits\|email \| rm NAME \| allow NAME TARGET \| disallow NAME TARGET` | Private facts (a phone number, an address) that must not leave in an outgoing call; the value is asked for hidden, or read from one stdin line when piped, never taken as an argument. They are kept in `facts.toml` (0600, with the daemon's files). `/facts` in a chat lists names, kinds and allowed targets and, from `tui` or `chat` only, `rm NAME` and `allow NAME TARGET`; a chat never takes a value. The agent may add a fact (you are told its name); removing one or allowing a target is only proposed |
| `zephyr vault list \| add login NAME --site URL [--user U] [--totp] \| add token NAME [--host HOST].. \| edit NAME [--host HOST].. \| rm NAME \| revoke NAME [PROGRAM]` | Logins and tokens the agent uses by name (see [The vault](#the-vault)); values are asked for hidden, or read one per line from stdin when piped, never taken as arguments |
| `zephyr subagent list \| add NAME [--model P/M] [--prompt TEXT] \| edit NAME [--model P/M\|default] [--prompt TEXT] \| rm NAME` | The named subagents the agent starts background tasks as (see [Background tasks](#the-agents-browser-and-desktop)); without `--prompt`, the prompt is read from stdin when piped |
| `zephyr config get\|set KEY [VALUE]` | Read or edit `zephyr.toml` (comments are kept; a change that would make it invalid is refused), e.g. `config set tasks.parallel 16` or `config set tasks.model PROVIDER/MODEL`. A key in the [settings list](#changing-settings) goes through the running daemon, which checks it, logs it and applies it. Any other key prints a warning that it is saved without checking, and is saved. With no daemon running, the file is edited directly, a listed key edited as the daemon edits it (`none` clears it, `model` changes the account's model as `/model` does, a preset drops looser hand-set classes; a time limit it changes is not put back), and applies at the next start |
| `/settings [SECTION\|KEY]` | From any channel: every setting by section with its value and where it came from (`default`, `you`, `agent (#N)`, `until HH:MM`); with a section or a key, its help line, what it takes and who may change it. In `zephyr attach` it opens the settings panel |
| `/set KEY VALUE [for DURATION]` | From any channel: change a setting, e.g. `/set tasks.parallel 4` or `/set budget.daily_cost_usd 10 for 1d` (`zephyr set KEY VALUE` does the same inside the container) |
| `/approvals [manual\|semi\|full\|on\|off] [for D]`, `/approvals messages\|posts\|money\|facts MODE [for D]`, `/approvals forget N` | From any channel: what waits for you before the agent sends a message, posts or pays (see [Approvals and presets](#approvals-and-presets)). Alone it shows the modes in effect, `preset semi (modified) · messages ask-new · posts delay 5 · money ask · facts hold`; `off` is `full`, `on` is `manual`. Loosening is accepted only from a strong channel (`zephyr attach` or the chat socket, by default). `forget N` makes zephyr forget remembered recipient #N (`/activity grants` lists them), so under `ask-new` the next send to it waits for you again |
| `/held` | The messages, posts and payments waiting for you or for their delay, newest first |
| `/hangup [N]` | End the phone call in progress, at once |
| `/activity [today\|6h\|7d] [KIND]` | Zephyr's own record of what ran, what left the machine, what waited and what was refused, newest first (the last 24 h unless a time is given, up to 90 d; at most 30 lines). KIND narrows it: `out`, `gate`, `refused`, `setting`, `proposal`, `job`, `task`, `process`, or `tool` for every tool call, which the bare list leaves out. `/activity grants` lists what still goes without asking (remembered recipients, time limits, turned-on jobs), each with the command that takes it back. Read from zephyr's tables, never written by the agent, and scrubbed of vault and fact values as it is read |
| `/web` | The [web app](#web-app): its address, passkeys, sessions and push devices |
| `/web enrol [NAME]` | From the terminal chat or `zephyr attach` only: a one-time code (10 minutes) that adds a passkey for a phone or laptop |
| `/web rm NAME` / `/web signout` | From any channel: remove a passkey with its sessions, or end every web session (a lost phone) |
| `/pair telegram\|signal` | From the terminal chat or `zephyr attach` only: a one-time code (10 minutes). Send `/pair CODE` to the bot or to zephyr's Signal number, and whoever sent it becomes an owner there; every owner channel is told who. `zephyr channel add` asks for it at its end. Wrong codes get no answer, and five close pairing for 15 minutes |
| `/unpair telegram\|signal ID` | From any channel: that Telegram user id or Signal ACI is no longer an owner |
| `/ok N [once]` / `/no N` | Approve or drop proposal or held action #N. `/ok N` runs exactly what its card showed; under `ask-new` it also remembers the new recipients, and `/ok N once` approves without remembering them |
| `/cancel N` | Stop an action waiting out its delay (`sending at 14:05 unless /cancel 7`) before it goes |
| `/facts [rm NAME \| allow NAME TARGET \| disallow NAME TARGET]` | From any channel: your private facts' names, kinds and allowed targets, never a value. `rm` and `allow` loosen the check, so only a strong channel may; a value is entered only in `zephyr attach` or with `zephyr facts add` |
| `zephyr attach [--stock]` | Zephyr's terminal UI on the live session: history, tool calls, what the agent is doing, the queue and today's spend; `^D` detaches and the agent keeps running. It never sends the attach token: the daemon has to prove it holds it first, so if something else holds the attach port the UI stops and says so. `--stock` runs the stock `rpi --connect` client instead, which does send the token to whatever answers: use it only while zephyr is up |
| `zephyr shell [bash\|zsh]` | A login shell in the container as the agent, in its home (`/zephyr`) and on its desktop; needs a terminal |
| `zephyr chat` | The same UI on a terminal; a line client (`< reply` per line) when piped or scripted |
| `zephyr pause` / `resume` | Stop and restart dispatch (`/pause` and `/resume` from any channel do the same) |
| `zephyr reload` | Apply hand edits now instead of within 5 s (`/reload` from any channel) |
| `zephyr uninstall [--keep-data]` | Remove the container and its images (the local package ones too), and `~/.zephyr` unless kept |

Sign in from the running gateway too, with no re-exec afterwards. In `zephyr attach`, `/login`
lists the providers and Enter starts the one you pick: an OAuth provider (Antigravity,
ChatGPT/Codex) prints a URL to open in a browser on your machine. After you sign in there, the
browser is sent to an address on `127.0.0.1` that it cannot open (the sign-in runs in the
container, behind the [network floor](#the-agents-network)): copy that address from the address
bar and paste it into the panel's input row, or at the prompt of `zephyr login` on the host, or
send `/login paste ADDRESS` on the channel you started it from. The
others ask for a key or a token, which is typed masked, goes to the sign-in once and is never
shown or kept. From any channel, `/login PROVIDER [ACCOUNT] [MODEL]` can start a sign-in too, but
only an OAuth one: a `key` or a token provider is refused there, so use `zephyr login` on the host,
or the attach panel. `/login cancel [ID|PROVIDER]` stops a sign-in you have given up on, so its slot
in the eight that may wait at once is free again. The account joins the pool at once; `/model`
selects it.

Copying in `zephyr attach`: drag over any text and it is copied when you let go. Each code block in
a reply carries a `#n` at its right edge; click the block to copy it, `/copy N` copies block N,
`/copy` the whole last reply, and `^X` the newest block (press again for the one before; with a
drag selection, `^X` copies that). What is copied is the block's source, without the wrapping or
the indent. Copying goes through the terminal (OSC 52), so it reaches the clipboard of the machine
you are typing on, even over ssh. It works in iTerm2, Ghostty, kitty, WezTerm and Alacritty, and
inside tmux only with `set -g set-clipboard on`; macOS Terminal.app does not support it and copies
nothing. Under `NO_COLOR` a click may not find the block; `/copy N` and `^X` still work. `/quit` or
`/exit` leaves the UI like `^D`, and the agent keeps running. A double-click copies the word under
it, a triple-click the row, and Shift+click copies from your last click to this one (some terminals,
kitty and WezTerm among them, keep Shift+click for their own selection and never pass it on).

## Typing and moving around

In `zephyr attach` (`?` lists all of these):

| Key | What it does |
|---|---|
| `↑` / `↓` | Your earlier messages, from the top or bottom row of the box; editing one makes it your draft. `↑` on an empty box first brings back what `^C` cleared |
| `^R` | Search your earlier messages; Enter puts one in the box, unsent |
| `Alt+S` | Put the draft aside and clear the box; on an empty box, bring the last one back |
| `@path` | A file in the projects folder (`@~/x` in `/zephyr`, `@/x` as written); Tab completes. Sent as its full path, and an image (png, jpg, gif, webp) goes along for the agent to look at. Dragging a file from the projects folder onto the terminal types its `@path`; one from anywhere else can't be seen in the container |
| `^G` | Edit the draft in `$VISUAL` or `$EDITOR` (`nano` by default); it comes back to the box, unsent |
| `/p [name] [args]` | Saved prompts: `/p` lists them, `/p name args` puts one in the box to read and send, with `$ARGUMENTS` and `$1`..`$9` filled in. Each is a markdown file in `~/.zephyr/prompts/` (`/zephyr/prompts/` in the container), so you can ask the agent to write one |
| `Enter` mid-reply | Steers your message into the running turn |
| `Alt+Enter` | Holds the message until the reply ends, then sends it as a request of its own (when idle, the same as Enter) |
| `Alt+Up` | Take back your messages that haven't reached the agent yet (held follow-ups, or ones waiting behind `/pause`) to edit; a steered one is already the agent's |
| `Shift+Tab` | The next thinking level |
| `Ctrl+P` | The next model of the account (`Ctrl+Shift+P` the one before, where the terminal tells them apart) |
| `Esc` `Esc` | On an empty box: pick one of your messages to rewind to; it and everything after leave the agent's context (`/undo N`) and it comes back to the box to edit |
| `Ctrl+F` | Search the conversation: the newest match first, `Enter` or `Ctrl+G` the one above, `Shift+Enter` or `Ctrl+Shift+G` the one below, `Esc` closes |
| `Ctrl+Up` / `Ctrl+Down` | Jump to your previous or next message |
| `Home` / `End` | Top or bottom of the conversation, when the composer is empty |
| `/focus` | Hide tool calls and thinking (again to show them); `focus` shows in the footer |

In the settings panel (`/settings`; `?` lists these). Its Approvals section holds the preset, each
class's mode, the currency, what needs a strong channel and a row that opens Private facts, the
section after it: each fact as `name  kind · allow: targets`, never its value. Waiting lists what
waits for you, newest first, a held action as its class and summary (`message · email to
alice@example.com`) and, waiting out a delay, when it goes.

| Key | What it does |
|---|---|
| `↑` / `↓`, `→` / `←` | Choose a section, then a setting in it; `←` goes back to the sections |
| `Enter` | Edit the setting: a list for a choice or a true-or-false setting, the model picker for a model, a one-line field for a number or text. On a proposal under Waiting, the whole card; on the Approvals section's Private facts row, that section |
| `Space` | Turn a true-or-false setting over at once |
| `t` (or `Tab` in an editor) | Add a time limit (`30m`, `2h`, `1d`); the setting goes back by itself |
| `o` / `1` / `d` | Under Waiting: approve, approve once, or drop. `1` is always offered on a setting or a command, where it is the same as `o`; on an action it is offered where once differs from ok (`ask-new`), and on a message or post it approves once, without remembering its recipients, whatever mode its class was in when the list was read; on a payment or any other call it is the same as `o` |
| `c` | Under Waiting: cancel an action waiting out its delay, before it goes |
| `a` / `e` / `d` | Under Private facts: add a fact, edit one (`Enter` too; an empty value keeps the saved one), or delete one (`y` confirms); the value is typed masked, checked before it leaves, and never shown again |
| `r` | Under Grants: send the command that takes the grant back (`/approvals forget N`, `/set KEY VALUE`, `/job rm N`), as if typed here; the list reloads. Activity, beside it, is zephyr's own record (`/activity`), and moving to its last row loads older entries |
| `Esc` | Back, or close the panel |

Drafts put aside are gone when you detach; the history starts again from the messages in the conversation. The footer shows `ctx N%`, how full the model's context is after the last reply.

The window title shows `◇` idle, `✦` working and `✋` when something waits for your OK, and the tab
shows a progress bar while the agent works. When you are in another window and a reply finishes or
an OK is asked for, the terminal gets a notification (`zephyr: reply ready` or `needs your OK`, at
most one every 10 s). iTerm2, WezTerm, kitty, Ghostty and Windows Terminal show these; other
terminals just ring the bell, or nothing.

## Changing settings

Changes apply by themselves; nothing needs `zephyr restart`. The commands above, and hand edits to
`zephyr.toml` or `secrets.env` (seen within 5 s), go to the running daemon, which says what it did:

| Change | Applies |
|---|---|
| Budget, owners, time zone, `[auth]`, `[tasks]`, the pool order | at once |
| The model, on the same provider | at once, or after the reply in progress (rpi's `set_model`) |
| A model on another provider, the active account, extensions, connectors (`[[mcp]]`), subagents (`[[subagent]]`), a re-login | the agent restarts after the reply in progress; the conversation carries on |
| Discord, iMessage, Telegram, Signal, `[attach]`, `[web]`, `[agent]`, a secret | the daemon restarts itself in place once idle; waiting messages are kept |
| The attach port, the iMessage webhook's port | `zephyr config set` and `zephyr channel` start the container again to publish it; after a hand edit you are told, a minute later, to run `zephyr up` |

A file that does not load is not applied, and the daemon keeps its current settings.

Most settings are in one registry, so the same change works from a command, the panel in
`zephyr attach` (`/settings`), `zephyr config set` and the agent. `/settings` lists them; `/set KEY
VALUE` changes one, and `/set KEY VALUE for 2h` changes it for a while (30 seconds to 30 days): it
goes back by itself and you are told. The agent can read all of them. What it may change:

| Setting | The agent may |
|---|---|
| `tasks.parallel`, `browser.pace_ms` (raise it), `mcp.NAME.enabled` (a connector off), `settings.strong_channels` (remove a channel), the `budget.*` limits, the `approvals.*` modes and preset, `approvals.strong_only` (add `money`), `mcp.NAME.classes.TOOL` (put a tool in a class) | make it stricter at once (lower, off, fewer channels, a limit where there was none, a stricter mode or preset); you are told and `/set KEY OLD` puts it back. Its `/set` takes one word, so it can tighten approvals to `ask` but not to `delay 10` |
| `model`, `thinking`, `tasks.model`, `timezone`, `auth.*`, `approvals.currency`, `network.allow`, subagents, rules, removing a private fact or allowing it a target, and any change of the above that loosens | propose it: a card shows `KEY old → new` (or the exact `/subagent` or `/rule` text, up to 1500 characters), and `/ok N` applies it, `/no N` drops it |
| `owners`, `[discord]`, `[imessage]`, `[telegram]`, `[signal]`, `[attach]`, `[web]`, `[agent]`, `extensions`, `policy_plugin`, `rpi`, `home`, `state`, the `[[account]]` pool | nothing; edit `zephyr.toml` |

The agent may also save a new private fact at once (it only makes the check stricter); you are told
its name, and `/facts rm NAME` removes it.

`network.allow` lists what the agent may reach beyond public addresses on ports 80 and 443, which
need no entry: exact `HOST:PORT` pairs, comma separated, with no wildcards, e.g.
`/set network.allow imap.gmail.com:993, github.com:22, homeassistant.lan:8123`. A listed name may
be on your LAN or tailnet; loopback, link-local addresses (cloud metadata) and the container itself
are refused even when listed. The value is the whole list, so include the entries you keep. A
change applies to the next connection.

The `budget.*` limits, `network.allow`, `settings.strong_channels`, the `approvals.*` settings (the currency too: it
changes what an `auto N` cap lets through), the connector tool classes and the private facts are
protected: raising a limit, adding a channel, changing the currency, or loosening an approval or a
fact is accepted only from a strong channel, by default `zephyr
attach` and the chat socket that `zephyr chat` and the host CLI use (`[settings] strong_channels`;
the TUI can't be removed).
From Discord or iMessage it is refused, and a proposal that needs one stays waiting for it, so a
stolen chat session can lower a limit but not lift it. An approval is checked when you give it: if
the setting changed since the card was made, `/ok` says so and drops the proposal, and a waiting
`/subagent` or `/rule` proposal goes stale when the subagents or the rules change. The agent never
removes or replaces a time limit you set.

From any channel: `/model` shows the models, `/model PROVIDER/MODEL` switches (to the account on that
provider), `/thinking` shows how hard the model thinks and `/thinking LEVEL` or `/thinking next`
changes it from the next request (kept until the daemon restarts), `/status` shows the agent's account and model, `/reload` re-reads the files, and `/new` (or `/clear`)
starts a fresh conversation (memory and rules carry over; the old one stays in `sessions/`), and
`/switch` lists the earlier ones (with their names and first words) and `/switch ID` goes back to
one. `/rename NAME` names the current one; `zephyr attach` shows the name in its header.
`/fork` copies the conversation as it stands and carries on in the copy (the original stays, for
`/switch`). `/export` writes the conversation as markdown to
`<projects>/zephyr-exports/<session>-<YYYYmmdd-HHMM>.md`, owned by you on the host.
`/btw QUESTION` asks a side question about the conversation, even mid-reply: a throwaway agent with
no tools answers it as `[btw] …` from a copy of the conversation, which is left as is.
`/undo` takes your last message and its reply out of the agent's context (files it changed stay
changed, see `/rollback`; another `/undo` goes one further back), `/undo N` the last N (up to 20),
`/retry` does that and sends the message again, and `/compact [INSTRUCTIONS]` has rpi summarise the
conversation so far to make room and tells you how much it kept. These and `/fork` wait for the
agent to finish (or `/stop` it). `/usage [DAYS]` lists
the spend per day and per account over the last 7 days, or `DAYS`.
`/login` signs an account in from here: `/login` lists the provider ids, `/login PROVIDER [ACCOUNT]
[MODEL]` starts an OAuth sign-in and needs no re-exec (a key or a token is refused — it is typed in
`zephyr attach`, which masks it), `/login paste ADDRESS` finishes yours with the `127.0.0.1` address
the browser could not open (never a token), and `/login cancel [ID|PROVIDER]` stops the ones you
gave up on.
The first account of an empty install does restart the agent once, when idle: there is nothing for
it to start on until that account exists.

In `zephyr attach` only: `!COMMAND` runs `bash -c COMMAND` as the agent in its home (60 s at
most, the first 50 KB of output kept) and shows the output and exit code; it goes to the agent in
front of your next message (not a `/` command), and Esc on an empty composer drops it instead.
`/shell [zsh]` opens an interactive login shell as the agent in place of the UI; exit it to come back.
Esc twice on an empty composer, with the agent idle, opens a rewind list of your messages in this
conversation, newest first: picking one sends `/undo N` back to before it and puts it in the
composer to edit and send again.

`ZEPHYR_CONTAINER` (default `zephyr`) and `ZEPHYR_IMAGE` choose another container or image, for
example to test beside a live install.

## Approvals and presets

Messages, posts and payments the agent makes wait for you, as far as you want; everything else it
does on its own, and what went off the machine is listed after the turn (DESIGN §4.16). What
counts:

- **Messages**: mail sent with `himalaya`; a connector tool named for sending (`send`, `reply`,
  `message`, `email`, `dm`); in the agent's browser, a Send or Reply button, or Enter in a message box.
- **Posts**: GitHub writes through `gh` (`pr create`, `issue comment`, `release create`, `gh api -X
  POST`, ...); a connector tool that posts (`post`, `publish`, `comment`, `tweet`, `share`,
  `create_issue`, `create_pull`); a Post, Publish, Tweet, Share or Comment button, or Submit on a
  form with a text area.
- **Payments**: a connector tool that pays (`pay`, `purchase`, `order`, `checkout`, `transfer`,
  `refund`, `charge`, `subscribe`); a Buy, Pay, Place order, Checkout, Subscribe, Donate or Transfer
  button, whatever form it is in.

Buttons are read in English and Korean (`보내기`, `전송`, `게시`, `결제`, `구매`, `주문`, ...). A
search box, Enter in a login form and links are not caught (clicking a login form's "Send code" is,
as a message), and neither is a label zephyr does not know or one in another language. A click
zephyr cannot read at all waits for you whatever the mode. A connector tool goes by its name (one
whose name starts with `get`, `list`, `search` and the like is a read); `/set
mcp.NAME.classes.TOOL messages|posts|money|none` puts it in another class.

Each class has a mode:

| Mode | What happens |
|---|---|
| `ask` | it waits for your `/ok` |
| `ask-new` | messages and posts: it goes at once to the recipients (or repos, or sites) you approved before, and waits for anyone new or anyone zephyr cannot read |
| `delay N` | it goes after N minutes (1 to 60) unless you `/cancel` it; in the browser the page waits at most 10 minutes |
| `auto` | it goes |
| `auto N` | payments: it goes when the amount is known, in `approvals.currency` (USD unless set), and at most N; otherwise it waits |

A preset sets them all:

| Preset | messages | posts | payments | private facts |
|---|---|---|---|---|
| `manual` (new installs) | ask | ask | ask | hold |
| `semi` | ask-new | delay 5 | ask | hold |
| `full` | auto | auto | auto | warn |

`/approvals` shows what is in effect; `/approvals manual|semi|full` picks a preset, `/approvals off`
is `full` and `/approvals on` is `manual`, and `/approvals messages|posts|money|facts MODE` sets one
class (`/approvals money auto 50`, `/approvals posts ask`). Add `for 2h` to any of them to change it
for a while. A preset never loosens a class you set yourself: picking one drops the classes you set
looser than it and keeps the stricter ones, and `/approvals` says `semi (modified)` while one
differs. These are settings like the rest (`approvals.*` in `/settings`, `[approvals]` in
`zephyr.toml`, the Approvals section of the settings panel): loosening one needs a strong channel,
and the agent can make one stricter at once but only propose the reverse. An install made before
approvals keeps sending as it did: its first start on this version writes `preset = "full"` and
tells you once.

When something waits, you get a card on the channel the agent was answering, and under Waiting in
the settings panel:

```
[zephyr] approve #7: message (email)
to: alice@example.com (new) · subject: "Invoice March"
first lines: "Hi Alice, attached is the invoice for March."
command: "himalaya message send <<'EOF' To: alice@example.com Subject: Invoice March Hi Alice, attached is the invoice for March. EOF"
the agent says: "the owner asked me to send the March invoice"
/ok 7 · /ok 7 once · /no 7 · expires in 24 h
```

The card is zephyr's reading of the action, not the agent's account of it; only the quoted line is
the agent's. `/ok 7` carries out exactly that, once: zephyr runs the command itself and tells the
agent how it went, lets that exact connector call through once in the next 30 minutes, or lets the
browser click go on after checking the page is still the one you approved (if it changed, nothing
is clicked and you are told). Under `ask-new`, `/ok 7` also remembers the new recipients (marked
`(new)`), and `/ok 7 once` approves without remembering them. `/no 7` drops it, `/cancel 7` stops
a delayed one (`sending at 14:05 unless /cancel 7`) before it goes, and `/held` lists what waits.

- An action nobody answers expires after 24 hours (a browser click after 10 minutes), and the
  agent is told. The agent never retries a held action: it is told `held for the owner as #7`, or
  for a delay `delayed as #7 … it goes by itself in 5 min unless the owner cancels it`.
- Nothing goes late on its own: a delay that comes due while zephyr is paused, after `/stop`, or
  while it is down waits for your `/ok` again, with its card sent once more. A delay also waits
  for you again when it comes due under stricter approvals than it was held under (you typed
  `/approvals on`, or saved a private fact it holds): you are told why and get its card. `/stop`
  also refuses every browser click still waiting, and a restart ends the ones that were waiting.
  Two `/ok` for the same action, from two channels, run it once.
- At most 3 actions wait from one run, and at most 20 held in a day that you did not approve
  with `/ok` (waiting, declined, expired, or delays that went by themselves); past either, the
  agent's further ones that would wait are refused, and you get one line about it. Sends you
  approve never count toward the day.
- A command too long for the card is cut there, and the card says how much is not shown, that
  `/ok` runs all of it, and everything the whole command sends. A command that gets vault tokens
  names them on its card.
- `[approvals] strong_only = ["loosen", "money"]` lets a payment be approved only from a strong
  channel. It is off by default, so you can approve a purchase from your phone. A loosening (of an
  approval, the currency or a private fact) needs a strong channel whatever the list says: the
  `loosen` entry names that rule and changes nothing on its own. A payment set to
  `delay N` still goes when its delay ends.
- **Private facts** (`zephyr facts`, `/facts`, the settings panel): a call that holds one waits for
  you (`hold`; the card names the fact, never its value), goes and tells you (`warn`), or is not
  checked (`off`); `/approvals facts MODE` sets which. A fact allowed for a recipient (`zephyr facts
  allow phone mum@example.com`) goes to that recipient without a check, but only when the
  command does nothing but that send: anything else beside the mail (a `curl`, a script, even an
  `echo`) is checked as if it went somewhere unknown.

What it does not stop: an agent set on getting around it, which can run a copy of the real browser,
send from a Python script or reach a mail API with `curl`. It stops a confused or misled agent
using its usual tools; general network traffic is not limited. A fact's allowed recipient is the
same: himalaya's and gh's config files are in the agent's home, so an agent set on it can point
either elsewhere.

## What you are told

- After a turn that ran a command sending data off the machine (curl or wget with a body, ssh, scp,
  rsync to a host, nc, `git push`, ...), zephyr lists them: `[zephyr] outbound this turn: ...`.
  Jobs report theirs on the job's channel. Plain downloads are not listed. In the agent's browser,
  typing, uploading and running script on a page count (`agent-browser fill`, `type`, `keyboard`,
  `upload`, `eval`); plain browsing does not.
- When the agent sends a message, posts or pays and your approval modes say to wait, you get its
  card (see [Approvals and presets](#approvals-and-presets)); under `warn`, a call holding a private
  fact tells you `[zephyr] the agent sent your private fact NAME to …`.
- When the agent repeats a tool call that keeps failing the same way, zephyr tells it after 3 tries
  to change approach, and after 5 stops the run: `[zephyr] stopped a run that repeated a failing …
  call 5 times`. Jobs and background tasks end `failed` the same way.
- Notices go to the channel you last wrote from. Under `zephyr attach` they appear as messages in
  the conversation, or at your next attach if none is open.
- When the agent changes its memory (`memory/USER.md`, `MEMORY.md`), you get the diff. The change
  reaches the agent's context only after `/memory ok` (if it changed after you were sent the diff,
  you are shown the new one first); `/memory undo` drops it, `/memory diff` shows
  it again, `/memory log` lists commits. A web page cannot plant a lasting "preference" without you.
- Every memory change waits for `/memory ok`, and its notice says where it came from (`from
  your message`, `from task #7`, `from this conversation, which used exec at 14:02`). You can
  have zephyr save a change at once when it saw the change came from you alone: `/set
  memory.auto owner`, from the TUI or `zephyr chat` (Discord and iMessage cannot turn it on; the
  agent can only turn it off). We recommend leaving it off. With it on, a change is saved without
  asking only in a conversation started with `/new` that has read nothing but your messages (no
  web page, command, file you sent, task report or tool other than reading and editing
  `memory/USER.md` and `MEMORY.md`), with nothing else of the agent's running or alive, and
  nothing waiting; you are still told (`[zephyr] memory saved from your message; /memory undo
  drops it`), and `/memory undo` takes it back. Once a conversation has read anything else it
  stays that way until the next `/new`; after the agent used its browser, `/stop` then `/new`.
  What it cannot see: a program you start as the agent yourself (a `zephyr shell`, which reads
  the agent's `~/.profile`; `docker exec -u agent x11vnc`, which reads `~/.x11vncrc`) runs what
  the agent left there, and on a host whose `kernel.yama.ptrace_scope` is 0 an agent process can
  attach to the desktop's programs. Leave it off if either applies.
  For the same reason rpi skills are off, only the image's extensions load, and a `SYSTEM.md`, `APPEND_SYSTEM.md`, `CLAUDE.md` or
  extra `AGENTS*.md` the agent writes is renamed `*.ignored-by-zephyr` before the next start.
- `/forget TEXT` removes every memory line holding TEXT (at least 3 characters, any case), saves
  that at once and lists what went; `/memory undo` puts it back. The reply says where the text is
  still kept (the memory's git history, past conversations, the message log, your rules, goals or
  persona). `/memory squash yes` then drops the memory's git history, keeping the files as they
  are; `/memory undo` cannot go back past it.
- When the agent drafts a skill or a connector, you get its diff the same way (below).

## Skills and connectors

**Skills** are how-tos the agent keeps: a folder with a `SKILL.md` (the agentskills.io format
Claude, Hermes and OpenClaw use: YAML frontmatter with `name` and a one-line `description`, then
the steps) and any files beside it. After a task that took many steps, the agent may draft one in
`skills/drafts/<name>/`; you are sent it once, and it is used only after `/skill ok NAME`. If it
changed after you were sent it, you are shown the new version first. The agent then sees each
approved skill's name and description, and reads the rest when a task calls for it.
`/skills` lists approved skills and drafts waiting; `/skill no NAME` drops a draft, `/skill diff
NAME` shows it, `/skill rm NAME` removes an approved skill, and `/skill undo NAME` takes back the
last change to it (again to go further back). On the host, `zephyr skill add DIR` adds a skill
folder of yours, approved at once (at most 32 files and 256 KB, regular files only).

**Starter skills** come with the image and are approved at the first start that sees them, on a
new install and on an upgrade alike, with a message saying so (and another when a later image adds or updates one): `report` (Markdown to a PDF, Korean
included), `slides` (Markdown to a 16:9 PDF, or a `.pptx` when you ask), `pdf` (read, look at,
merge, split, fill and encrypt PDFs) and `charts` (a PNG or SVG chart). Each writes files and sends
them to you with `send_file`; none uploads anything. `slides`' PowerPoint and `charts` fetch their
Python library on first use. `/skill rm NAME` (or `zephyr skill rm NAME`) removes one for good: no
later start or image brings it back, though `/skill undo NAME` does. A newer image updates a starter
skill you left as it was, never one you changed or replaced; a skill of yours with the same name
is left alone. The agent's own edits to one are drafts, approved only with `/skill ok`.

**Connectors** are MCP servers the agent may call. Add one on the host:

```sh
zephyr mcp add docs --url https://example.com/mcp --header 'Authorization=Bearer ${DOCS_TOKEN}' --secret DOCS_TOKEN --plain
zephyr mcp add files -- npx -y @modelcontextprotocol/server-filesystem /zephyr/projects
```

This writes an `[[mcp]]` table to `zephyr.toml` (`name`, then `url` and `headers`, or `command`,
`args` and `cwd`; also `env`, `timeout`, `enabled`), and the agent restarts after the reply in
progress. Adding a connector of the same name again replaces it and keeps the classes you gave its
tools. `${NAME}` is filled in from `secrets.env`; a connector whose secret is missing is left
out, and `zephyr doctor` and `/mcp` say so. zephyr's own secrets (`ZEPHYR_*`) and the channels' tokens can never be filled into a connector. The agent may also propose one in
`connectors/drafts/<name>.json`: `/mcp ok NAME` adds it, `/mcp no NAME` drops it, `/mcp rm NAME`
removes a connector, and `/mcp` lists them. The agent can use only these connectors: an MCP config
it writes itself, or a server named by URL, is refused.

A connector that runs a command does not hold its secrets: `mcp.json` has a stand-in for each
`${NAME}`, and zephyr's proxy sends the real one only to the connector's hosts, in the
`Authorization` header. A remote (`--url`) connector cannot work that way: its requests are made
by rpi's own MCP client, which does not trust zephyr's certificate. So a remote connector with a
secret holds the real one, which the agent can read, and only once you say so: `--plain` when you
add it (`zephyr mcp add docs --url … --header 'Authorization=Bearer ${DOCS_TOKEN}' --secret
DOCS_TOKEN --plain`), or `/set mcp.NAME.secrets plain` from a strong channel. A connector the
agent drafts that uses a secret, or changes one that does, is approved (`/mcp ok`) from a strong
channel only, and approving it never hands out the real secret by itself. Until then it is left
out, and `/mcp` and `zephyr doctor` say why. Remote connectors you had before this version keep
working: the upgrade writes `secrets = "plain"` for them and tells you. One secret cannot be a
plain connector's and another connector's stand-in at once; give each its own. A connector that runs a command and uses a secret must say where it goes, or it
is left out:
`zephyr mcp add NAME --host api.example.com ...`, or `/set mcp.NAME.hosts api.example.com`. For a
secret that is not an HTTPS header (a database password, a signing key), `--plain` or
`/set mcp.NAME.secrets plain` hands the connector the real one, which the agent can then read.

**Ready-made connectors.** The image carries a catalog of six, each with its command, the secret
it needs, the hosts that secret is sent to and the approval classes of its tools filled in
(`zephyr mcp catalog` lists them):

```sh
zephyr mcp add --catalog github          # asks for GITHUB_TOKEN, hidden
zephyr mcp add --catalog home-assistant --url https://home.example.com/api/mcp
zephyr mcp add --catalog brave-search --plain
```

`github` (the image's github-mcp-server), `notion` and `linear` hold only a stand-in for their
token; `linear` and `home-assistant` reach their remote server through `mcp-remote`, so their key
is a stand-in too. `brave-search` sends its key in a header zephyr does not swap, and a
`home-assistant` at an `http://` address is reached without TLS, so those need `--plain`, which
hands the server the real key, readable by the agent. `context7` needs no key. The npm servers are
fetched at their pinned version on first use. Adding an entry again keeps the class you gave any tool, and nothing else: its hosts are the
entry's (set others with `/set mcp.NAME.hosts`), and it holds its real key only if you say
`--plain` (or `plain`) in that add, even when it did before;
`zephyr doctor` tells you when a newer entry ships than the one you added. The agent can read the
catalog (`/opt/zephyr/connectors/`) and suggest an entry, but only you add one. In a chat or the
TUI, `/mcp catalog` lists the entries, `/mcp catalog NAME` shows the table one writes and where
its secret goes, and `/mcp catalog add NAME [URL] [plain]` adds it, from a strong channel only (it
decides where a secret goes); it never takes the secret itself, and names the host command that
sets it.

## Web app

A phone-first app the daemon serves itself, reached over your tailnet through `tailscale serve`
(never `tailscale funnel`). It has:

- **Chat** with the live status line (`working · 0:24 · bash · 3 tools`), the reply as it is
  written, a turn's tools folded into one line, "take back" for a message still waiting, and Stop.
- **Approvals**: the same cards your other channels get, in full. Approvals are made on the
  card: a `/ok` typed into the app's chat is refused. Approve and Approve once take
  your passkey, and what you sign is that exact card: if it changes before you sign, the signature
  is refused and you are shown the new one. Deny needs no passkey. The agent's own stated reason
  is folded away under "the agent's reason (not checked)".
- **Activity**: zephyr's own record of what ran, what left the machine, what waited and what was
  refused, and the standing grants, each with a Revoke button.
- **Upcoming**: scheduled jobs and delayed sends.
- **Goals**: your goals, with Add, Done and Remove.
- **Settings**, with an optional time limit; a protected setting is saved with your passkey.
- **Private facts** and the **Vault**, names only. Secrets are never typed in the app.
- **Devices**: notifications, sign out, remove a passkey.

**Setting it up.** The web app needs the network floor (the default; `network = "host"` in
`host.toml` turns the app off, and `zephyr status` says so). You need
[Tailscale](https://tailscale.com/download) on this machine, with
HTTPS certificates turned on for your tailnet (admin console, DNS, HTTPS Certificates). Then:

```sh
zephyr web setup
```

It asks before each step: it writes `[web]` to `zephyr.toml` with this machine's tailnet name, runs
`tailscale serve --bg http://127.0.0.1:7793` (if that is refused, it prints the `sudo` form, or
`sudo tailscale set --operator=$USER` to let your user manage serve), restarts zephyr, and prints
a link with a one-time code. Open the link on your phone to add it. Once a passkey exists it
offers to make payments wait for a passkey or the terminal (`approvals.strong_only =
loosen,money`), so Discord or iMessage alone can no longer approve one.

**Never point `tailscale funnel` at this port.** Serve keeps the app on your tailnet; a funnel
would put it on the internet. The app refuses funnel traffic, but do not rely on that alone.
`zephyr status` says whether `tailscale serve` forwards to the app.

By hand, the app is off until `zephyr.toml` has a `[web]` table:

```toml
[web]
host = "box.tail1234.ts.net"       # this machine's tailnet name, alone: no https://, port or path
port = 7793                        # the default; the daemon listens on 127.0.0.1 only
# tailnet_users = ["owner@github"] # optional: only these tailnet logins get past the front door
```

The daemon refuses any request whose `Host` is not that name, that did not come through
`tailscale serve`, or (for anything that changes something) that does not come from the app's own
page. `zephyr status` says `web: listening`, and `zephyr doctor` checks the listener and the
push key, and in its `desktop` line whether the app can take the agent's desktop over.

**Signing in.** The app has no password. Each device signs in with a passkey (Face ID, a
fingerprint, the device PIN), and a new device needs a one-time code that only you can make:

1. Type `/web enrol phone` in `zephyr chat` or `zephyr attach` (not Discord or iMessage), or run
   `zephyr web enrol phone` on the host. The reply is a link, `https://HOST/#enrol=ABCDE-FGHJK`.
2. Open the link on the phone within 10 minutes, or open the app and type the code. It works once.
3. You are told on your usual channel that `passkey phone was added`. If you did not do that,
   `/web rm phone` at once.

A session lasts 30 days, or 7 days unused. `/web` lists the passkeys and sessions, `/web rm NAME`
removes a passkey and signs its devices out (from any channel: use it for a lost phone), and
`/web signout` ends every session. Five failed sign-in attempts in ten minutes close sign-in for
15 minutes and you are told; devices already signed in keep working.

**Notifications.** On the app's Devices screen, "Turn on notifications" subscribes that device
(on an iPhone, add the app to the Home Screen first: iOS offers notifications only there). You
then get a push when something waits for your approval, and when a reply arrives while the app
is closed. A push says no more than a Discord message would: the kind of action and its summary,
or the first 120 characters of a reply; never the agent's stated reason. On Android and desktop a
card's notification has buttons: **Deny** always, and **Approve** only for a message or a post
that your other channels could approve too. A payment, or anything that loosens a protected
setting, opens the app, where approving takes your passkey. Up to 10 devices; `/web rm NAME`
removes a passkey's devices with it. Swiping a card's notification away denies it, on Android only.

**Take over the desktop.** The menu's **Desktop** screen (there when the agent has a desktop)
lets you use the agent's screen yourself: tap **Take over** and approve with your passkey. While
you hold it the agent's browser and `xdotool` commands wait, and each one that waited is refused
when you hand back, since the page may have changed under it; the agent is told to take a fresh
look first. **Hand back** returns the desktop. It also goes back by itself after 10 minutes
without a key or a tap, after 60 minutes, or a minute after the app closes, and you are told. On
a phone, **Keyboard** opens a field, and what you type in it is typed on the agent's screen. One
device at a time; another can still hand it back.

The screen is the agent's: it sees what is shown and can read what you type there. Sign in to
sites through [the vault](#the-vault) rather than by typing a password. The picture comes through
the app's own connection; nothing listens for a VNC viewer, inside the container or out.

**Before a release that touches `crates/zephyr/assets/web/`**, check by hand, on iOS Safari as a
Home Screen app and on Android Chrome (a real browser is not part of the tests): enrol with
`/web enrol`, sign in, chat, approve a held message with the passkey, deny one from a
notification, swipe one away on Android (it is denied), take over the desktop, type into the
agent's browser with **Keyboard** (English and Korean), hand back, and `/web rm` from another
channel signs the phone out. The browser's console must show no Content-Security-Policy errors,
on the Desktop screen too, with a photo on the agent's screen.

## Telegram

Zephyr can be a Telegram bot that talks to you in a private chat. Make a bot of zephyr's own with
@BotFather (`/newbot`), and send it `/setjoingroups` → Disable, so nobody can add it to a group (if
one does, it leaves at once). Then `zephyr channel add telegram` asks for the token (hidden, into
`secrets.env`), writes the table and prints a pairing code: send `/pair CODE` to the bot from your
own Telegram account within 10 minutes, and you are its owner. A code is good once; ask for another
with `/pair telegram` in `zephyr chat` or `zephyr attach`. Every owner channel is told who paired,
and `/unpair telegram ID` takes it back. Anyone else who messages the bot gets no answer, and their
files are never fetched.

```toml
[telegram]
token_env = "ZEPHYR_TELEGRAM_TOKEN"   # the variable in secrets.env that holds the bot token
# progress = true                     # as [discord]: typing, and a progress note after 20 s
# progress_after_secs = 20

[owners]
telegram = ["123456789"]              # Telegram user ids, written by /pair; never a name
```

`zephyr doctor` checks that the bot answers. Replies are plain text: Markdown shows as typed. A
message you edit or delete after sending is not seen; send it again. One bot token serves one
zephyr: a second one polling the same bot (a side container) waits until the first stops.

## Sending the agent files

Images and files you send on Discord, iMessage, Telegram or Signal are saved for the agent in
`~/.zephyr/inbox/` (`/zephyr/inbox/` in the container), and your message tells it each file's path;
it opens images with `read`. A message with only a photo works. Up to 10 files a message, 25 MB
each (20 MB on Telegram, its limit for bots): anything beyond is listed but not downloaded. Only
owners' files are downloaded. iPhone photos arrive as JPEG (BlueBubbles converts HEIC). Files are deleted after 30 days.

## Voice

A voice note you send on Discord, iMessage, Telegram or Signal is saved like any file and transcribed on the
machine itself, by whisper.cpp (the `small` model; nothing leaves the container). The agent
reads it as two lines under your caption, if you typed one:

```
[attached audio: /zephyr/inbox/discord-55-1.ogg (voice note, audio/ogg, 0:12, 18 KB)]
[transcript of the voice note above, by zephyr (ko), may be wrong: "내일 아침 9시 회의 알려줘"]
```

- A transcript is not your typed text. Speech never runs a command: saying "slash ok 3"
  approves nothing, so approvals stay typed. A command typed as a voice note's caption is not
  run either (the whole message goes to the agent): send the command as a message of its own.
- A note longer than `voice.max_secs` (300) is saved but not transcribed. One note is
  transcribed at a time; a minute of speech takes about 11 s on CPU. A phone call with
  `calls.speech = local` has a voice of its own, so a note sent during one is heard at once.
- A conversation that took in a voice note is no longer yours alone: with `memory.auto owner`,
  its memory changes wait for `/memory ok` until `/new`.
- Settings (`[voice]` in `zephyr.toml`, or `/set`): `voice.transcribe` (`true`),
  `voice.languages` (`ko, en`; whisper's codes, and a note heard as another language is
  transcribed again as the first), `voice.max_secs` (10 to 900). The agent can only propose a
  change to them.
- The reply to a voice note is also spoken (Piper, also on the machine): the text comes first,
  then a voice message. `/set voice.reply always` speaks every reply on Discord, iMessage, Telegram and Signal,
  `off` none. Notices, approval cards, replies with code and replies over 1500 characters are
  never spoken. Korean replies are spoken only by an image built with `--build-arg KO_VOICE=1`
  (that voice's licence is CC BY-NC-SA 4.0, non-commercial).
- `zephyr status` says `voice: whisper small on CPU`, and `zephyr doctor` checks the models and
  transcribes a silent sample.
- Voice needs the network floor (`network = "bridge"` in host.toml, the default): the programs
  that read your audio get no network there. With `network = "host"` notes are saved but not
  transcribed, nothing is spoken, and `zephyr status` says so.
- An image built with `--build-arg VOICE=0` has no voice: notes are saved as files only.

## Phone calls

The agent can ask zephyr to phone a Canadian business for you (book a table, ask a question). You
approve every call; zephyr places it, tells the other party an AI is calling for you, and sends you
what is said as it is said.

- **Every call waits for you**, whatever your approvals preset. The card shows the number and its
  province, the line the call opens with, and the agent's stated purpose; `/ok N` places it and
  `/no N` drops it. Outside the calling hours where the number is (9:00 to 21:30 on weekdays,
  10:00 to 18:00 on weekends, their time) `/ok` answers when it can go and the call keeps waiting.
- **Only Canadian local and toll-free numbers.** Never an emergency or short code (911, 811, 988),
  a 900 number, a US or Caribbean number, or anything the agent says is a hospital, clinic or
  pharmacy: make those calls yourself. Toll-free numbers are refused too (they are shared with
  the US and many are crisis or health lines) unless you turn them on yourself:
  `/set calls.toll_free true` in the TUI or `zephyr chat`. Calls are in English, French or Korean.
- **During the call** you get the transcript on the channel you asked on, a few lines at a time:
  `[call #9 0:42] them: "Seven is full, we have seven thirty" · agent: "Seven thirty, please."`.
  `/hangup` (or `/hangup 9`) ends it at once from any channel; so do `/stop` and `/pause`. A call
  also ends by itself after `calls.max_minutes` (5 by default), after 20 seconds of silence, and
  when the other side hangs up.
- **Private facts are not said.** A sentence holding one of your private facts is replaced by
  "That is something I can't share on this call", and you are told which fact. To let one be said
  to a number: `/facts allow NAME tel:+14165550123`.
- **Speech is Telnyx's by default** (`calls.speech = provider`): it transcribes the other party
  and speaks zephyr's lines, quickly, and so hears the call. Every line still passes through
  zephyr as text first. `/set calls.speech local` keeps speech on this machine (whisper and
  Piper: English, Korean with `KO_VOICE=1`, no French). Each call keeps its own whisper and Piper
  loaded, so a turn takes about 1.7 to 1.9 s on a quiet machine, before the model's own time.
- **Nothing is recorded.** zephyr keeps no audio and never asks Telnyx to record; only the text is
  kept, for `calls.keep_days` (30). What the other party said is treated like a file someone sent
  you: the agent reads it as data, and a memory change after a call waits for `/memory ok`.

Setting it up, once:

1. A [Telnyx](https://telnyx.com) account, a Canadian number on it, and a Voice API (Call
   Control) application. In Telnyx's portal, forward the number's incoming calls to your own
   phone, so the caller id the business sees reaches you.
2. On the host: `zephyr vault add service calls --provider telnyx --account CONNECTION_ID --from
   +1NUMBER --public-key KEY` (the application's connection id, your number, and the account's
   public key from the portal's API keys page; the API key is asked for at a hidden prompt).
   To change one of them later, give only that flag; `zephyr vault add service calls` with none
   changes the key alone.
3. Telnyx connects to zephyr from the internet, so one port must be reachable: `zephyr config set
   calls.port 7794`, front it on the host with `sudo tailscale funnel --bg 7794` (or your own
   HTTPS front), and `zephyr config set calls.media_url wss://YOUR-HOST.ts.net`. That port answers only Telnyx's signed requests.
4. `/set calls.provider telnyx`, `/set calls.caller_name Your Name`, `/set calls.contact_phone
   416-555-0199` and `/set calls.contact_email you@example.com` (or `calls.contact_address`):
   every business hears them in the opening line. `zephyr doctor` lists what is still missing.

## Signal

Zephyr can talk to you over Signal on **a number of its own**: a second SIM or eSIM, or a VoIP or
landline number (`--voice` takes the code by call). **Never your own Signal account**: a device
linked to it receives every conversation you have, and its keys speak as you to all of your
contacts. (Linked to your own account by mistake, zephyr hears nothing: it takes only direct
messages from other accounts.)

`zephyr channel add signal` asks for the number and whether to register it (you paste the
`signalcaptcha://` token from signalcaptchas.org, then type the SMS or call code) or to link it to
an account on a spare phone (it shows a QR code to scan). It then writes the table and prints a
pairing code: send `/pair CODE` to zephyr's number from your own Signal account within 10 minutes,
and that account is the owner. `/pair signal` in `zephyr chat` or `zephyr attach` gives a new code.

```toml
[signal]
account = "+821012345678"   # zephyr's own number; not a secret

[owners]
signal = ["7f3c…"]          # your ACI (Signal's account UUID), written by /pair; never a number
```

Group messages, stories, reactions and edits are ignored; files you send are saved as on Discord.
signal-cli runs as root in the container, its keys in `/var/lib/zephyr/signal` (0700, out of the
agent's reach, not backed up: if it is lost, register or link again). `zephyr doctor` checks that
it is running and holds the account. When your safety number changes (a new phone), replies to you
stop and you are told once on your other channels; compare it, then
`zephyr channel signal trust <ACI>`. `zephyr channel remove signal` unregisters the number and
deletes the keys. In an image built with `SIGNAL=0`, or on arm64, there is no Signal.

**A gap:** signal-cli confirms a message to Signal's server when it reads it, before zephyr has
saved it, so a message that arrives in the instant the daemon dies is lost. Send it again if no
reply comes.

## Limits

`[budget]` in `zephyr.toml`, all optional: `daily_tokens` and `daily_cost_usd` pause the gateway
for the rest of the UTC day, also when you lower one under what the day has spent already.
`/resume` of that pause overrides the limits it was paused over, as they are, until midnight UTC;
changing one of them ends its override, and raising a limit over the day's spend ends the pause
by itself; `run_turns` and `run_minutes` stop one run that
goes on too long, and you are told why. A job run also stops after 30 minutes.

## Standing rules

`/rule never push to main` adds a rule the agent sees first in every session, above its memory,
from the next message on. `/rules` lists them, `/rule rm 2` removes one. The agent can propose a
rule or a removal; it never takes effect until you `/ok` the card, which shows the rule whole. Rules live with the
daemon, out of the agent's reach, and do not fade when a long conversation is summarised the way
instructions given in chat can. `/new` starts a fresh conversation; rules and memory carry over, and `/switch` goes back to an earlier one.

## Goals and persona

`/goal add Ship zephyr 0.8` gives the agent a goal it keeps in mind in every run, from the next
message on (at most 20, each up to 500 characters). `/goals` lists them; `/goal done 2` marks one
done and `/goal rm 2` removes one, and both take it off the list. `/persona set TEXT` (several
lines are fine, up to 4000 characters) says who the agent is with you; `/persona` shows it and
`/persona clear` removes it. The persona comes before your rules, which still win. The agent can
propose a goal, that one is done, or a persona, through a card you `/ok`; it never changes them
itself. Both live with the daemon, beside the rules.

`/set review.at 21:30` turns on a daily review at that time (in `timezone`): zephyr sends your
goals, what was done since the last review, the day's activity and the memory changes, then the
agent's short check-in on the goals (none when you have no goals). It goes to the channel you last
wrote from. `/job list` shows it as a built-in job; `/job run N` sends one now, and
`/set review.at none` turns it off.

`/set memory.notify digest` (with `review.at` set) stops the memory notice after each run: the
daily review lists every memory change since the last one, where each came from, and what is still
waiting, and `/memory ok` after it keeps that. `/set memory.notify each` goes back.

## The agent's browser and desktop

The image gives the agent a browser and a screen of its own (DESIGN §4.11):

- **Browser.** fingerprint-chromium (ungoogled-chromium with fingerprint patches; Playwright's
  Chromium on arm64), driven by the `agent-browser` CLI. The `agent-browser` the agent runs is
  zephyr's wrapper, which holds a click on a send, post or pay button for your approval (see
  [Approvals and presets](#approvals-and-presets)). Its profile is `~/.zephyr/browser`, kept
  across restarts. It is the agent's, never yours, and holds only the sign-ins you give it: through
  [the vault](#the-vault), or by hand (below).
  A site that still blocks it gets Camoufox, a stealth Firefox, which keeps no sign-ins.
- **Pacing.** The agent's navigations and clicks on one site (`www.amazon.co.uk` and
  `smile.amazon.co.uk` are one) go at least `browser.pace_ms` apart, 1500 ms unless you set it,
  plus up to half again at random, however many of its tasks browse at once: a burst of commands
  is what gets accounts flagged as bots. `/set browser.pace_ms 3000` slows it, `0` turns it off;
  the agent may slow it itself, and only propose speeding it up. With pacing on, a `batch` holds
  at most one navigation. Upgrading writes nothing for it: `[browser]` is in `zephyr.toml` only
  once you set the key, and a 0.9.0 image pinned back by hand refuses a file that has it
  (`zephyr upgrade 0.9.0` restores the file it backed up).
- **Python.** A read-only environment first on the agent's PATH (`/opt/zephyr/py`: requests,
  beautifulsoup4, pandas, pypdf, pillow, Camoufox), locked in `deploy/python/uv.lock`, plus `uv` for
  anything else (`uv run --with`, or a venv of its own; uv's cache lives in `~/.zephyr/.cache/uv`).
  To add a package for every run, edit `deploy/python/pyproject.toml`, run `uv lock` there, and
  rebuild.
- **Command-line tools.** `gh` (GitHub, once you give the agent a token), `himalaya` (mail, below),
  `jq`, `rg`, `fd`, `sqlite3`, `ssh`, `zip`/`unzip`, `tree`, `file`, `zsh`, and node with `npm` and
  `npx`. Writes through `gh` (`pr create`, `issue comment`, `api -X POST`, ...) and mail
  `himalaya` sends are posts and messages: they wait for you as your
  [approval modes](#approvals-and-presets) say, and are in the turn's outbound list.
- **Installing.** The agent has no sudo. What it installs lands in its home and survives upgrades:
  `uv tool install`, `npm i -g` (`NPM_CONFIG_PREFIX` is `~/.zephyr/.local`) and any binary in
  `~/.zephyr/.local/bin`, which is first on its PATH. An apt package needs you:
  `zephyr packages add NAME`.
- **Mail.** The agent reads and sends mail with `himalaya`, configured in
  `/zephyr/.config/himalaya/config.toml` in its home (`~/.zephyr/.config/himalaya/config.toml` on
  the host); the agent can write that file itself. Keep the password out of it: see Vault. There
  is no mail channel: mail does not reach the agent by itself.
- **Desktop.** An Xvfb screen (1920x1080) with openbox, where the browser runs. The agent sees it
  through screenshots and drives it with `xdotool`.
- **GPU.** When Docker has the NVIDIA runtime, `zephyr up` passes the GPU in, so the browser's
  WebGL is the real card instead of software rendering, which bot checks notice.
- **Watching it, or signing it into a site.** Open the [web app](#web-app)'s **Desktop** screen and
  tap **Take over** ("Take over the desktop" below that heading). No VNC server listens, and none
  needs starting by hand. With `network = "host"` in host.toml there is no web app, so no takeover.

- **Files and jobs.** The agent's `send_file` tool sends you a file (up to 10 MB: a screenshot, a
  PDF) with its reply. Its `schedule` tool only proposes a job: you are shown it, and it runs after
  `/job ok N` (`/job rm N` drops it, though not while one of its runs is in progress; `/job list`
  marks the ones waiting).
- **Commands.** The agent's `gateway` tool runs `/status`, `/model`, `/switch`, `/rules`,
  `/goals`, `/persona`, `/memory diff`, `/memory log`, `/job list`, `/settings`, `/approvals` or `/facts` (the facts'
  names and kinds, never a value) and reads the output in the same turn. It can save a private fact
  (`/facts add NAME KIND VALUE`; you are told its name) and propose removing one or allowing it a
  target. It
  can change a setting with `/set KEY VALUE [for D]` (stricter at once, else proposed; see
  [Changing settings](#changing-settings)), and propose `/model PROVIDER/MODEL`, `/new`, `/switch ID`,
  `/pause`, `/reload`, `/subagent …`, `/rule …`, `/goal add|done|rm …` or `/persona set …|clear`: you are asked
  (`the agent asks to run /new (#3)`), `/ok 3` runs it exactly as if you had typed it, and `/no 3`
  drops it (a proposal lapses after 24 hours). Approvals (`/ok`, `/no`, `/cancel`, `/held`,
  `/memory ok`, `/job ok`, `/login`, `/resume`) and `/stop` are never proposable, so text the agent read cannot approve
  itself. In a scheduled job it gets no answer and proposes nothing.
- **Background tasks.** You can keep talking while the agent works. It hands anything that takes
  more than a minute to a task (its `task` tool): a fresh session running beside the conversation,
  2 at once by default (`[tasks] parallel` in zephyr.toml, up to 32). The agent can run a task on
  another model of a provider you have an account on, e.g. a cheaper one for a simple lookup;
  `/task list` shows which. `[tasks] model` sets what a task runs on when the agent names no model.
  You can also name subagents for it to start tasks as, each a prompt that leads the task's brief
  and, optionally, a model, e.g. `explorer` on `claude-code/claude-haiku-5-5` told it explores and
  never edits, `implementer` on `opencode-go/deepseek-4.1-flash`:

  ```toml
  [tasks]
  parallel = 16
  model = "claude-code/claude-haiku-5-5"   # optional

  [[subagent]]
  name = "explorer"
  model = "claude-code/claude-haiku-5-5"   # optional; else [tasks] model, else the account's
  prompt = """You explore and report; you never edit files."""
  ```

  A task's model is the one the agent names, else its subagent's, else `[tasks] model`, else the
  account's own. You set these from any channel, and the agent may propose a change (a card shows
  the exact command; `/ok N` applies it only if the subagents are unchanged since): `/subagent` lists them,
  `/subagent add NAME [PROVIDER/MODEL] PROMPT`, `/subagent model NAME PROVIDER/MODEL|default`,
  `/subagent prompt NAME PROMPT` and `/subagent rm NAME` edit them, and `/task parallel N` and
  `/task model PROVIDER/MODEL|default` set the defaults (`zephyr subagent` and `zephyr config set`
  on the host). A model must be on a provider you have an account on, and rpi must list it. A task
  keeps the subagent prompt it started with, even if you edit it. The agent answers right away, and when the task ends its report comes
  back to the agent, which tells you. Ask it to cancel, or use `/task list` and `/task stop N`.
  `/stop` stops the reply and every task. A task cut off by a restart or a settings reload picks up
  where it left off in its own session, told to check what was in progress before repeating
  anything (at most 3 times; then it is reported as unfinished).
- **Talking to it while it works.** A message you send mid-run reaches the agent after the step
  it is on, and its answer comes back right away, not when the whole job is done. The agent runs
  commands through its own `exec` tool: when you send a message during a long command (or after
  10 minutes, or for a server it starts), the command moves to the background without being
  stopped, the agent answers you, and you get its result as a message when it exits.
  `/process list` and `/process stop N` show and stop them; `/stop` stops them too, along with any
  process the agent detached (`setsid`, `nohup ... &`), and says how many. Each command's log stops
  at 50 MB with a note of what was dropped. A settings
  reload or a daemon crash keeps them running (the daemon is started again inside the container);
  a container restart ends them, and they are reported. A stopped task reports how far it got.
  Old job and task sessions and process logs are deleted after 30 days. A run that makes
  no progress for 10 minutes (20 inside a command) is stopped and you are told.
- **Earlier conversations.** The agent can search what was said before `/new` with its `recall`
  tool, and read around a hit. Task and scheduled job runs are left out unless it asks for them.
- **Undoing file changes.** Before each run (a message, a task, a job) zephyr takes a checkpoint of
  every git repo in your projects folder that changed since the last one (folders without `.git` are left out) (`node_modules/`,
  `target/`, `.venv/`, `dist/`, `build/`, `*.log`, `.env*` and what a project's `.gitignore`
  ignores are skipped). `/rollback` lists the newest 10, `/rollback diff N` shows what changed
  since checkpoint N, and `/rollback N [PROJECT]` puts the files back as they were then: changed
  and deleted files return, new ones are removed, ignored ones are left alone. It is refused while
  the agent, a task or an `exec` command is running (`/stop` them first), and it takes a checkpoint
  first, so `/rollback 1` undoes it. The newest 50 per project are kept, in `~/.zephyr/.checkpoints`;
  the agent can reach them, so this is an undo, not a guard. A container from before this needs
  `zephyr up` once.

A site's terms may forbid automated use; a site that catches the agent can flag the account it is
signed in as.

## The vault

Logins and tokens you save for the agent live in `vault.toml` in the daemon's own folder, where
the agent cannot read them (DESIGN §4.15). The agent uses them by name and never sees a value:

- **A login** (a website's username, password and, optionally, a TOTP seed for one-time codes).
  The agent opens the login page and asks zephyr to fill it; zephyr types the values in only when
  the page is on the login's own site (scheme, host and port exactly), so a look-alike page gets
  nothing. The first fill of each login waits for your OK.
- **A token** (an API token, a mail password). The agent runs a command with it (`exec` with
  `secrets: ["NAME"]`), and the token is set as the environment variable `NAME` for that command
  only. Each program a token is used with needs your OK once (`gh` for `GH_TOKEN`, say); after
  that the same use runs without asking. Wherever the value shows up in what the command prints,
  the agent gets `[vault:NAME]` instead.
- **A token goes only to the hosts you bind it to.** The command does not get the token: it gets
  a stand-in (`zsur_…`), and zephyr's proxy swaps the real token in only in HTTPS requests to the
  hosts you named, exactly (`api.github.com` does not cover `github.com`; no wildcards). Sent
  anywhere else the stand-in is worthless, and where zephyr can see it the request is refused and
  you are told. A token bound to no host cannot be used.

When the agent wants to use one, you are asked: `the agent asks to use the token GH_TOKEN with gh
(#4); /ok 4 allows it from now on, /no 4 drops it`. When one is missing, the agent asks you to
save it and names the exact command. Save and manage entries:

- In `zephyr attach`, `/vault` opens a panel: `a` adds a login, `t` a token, `e` edits, `d`
  deletes, `r` takes back what an entry is allowed for. `/vault add login NAME SITE` and
  `/vault add token NAME` open the form filled in. Values are typed masked; a token's form
  ends with the hosts it is sent to. Each save shows a
  card of what was saved and what it may be used for, never the value. A new entry saved here is
  told to the agent, so it can carry on.
- On the host, `zephyr vault add login github --site https://github.com --user me [--totp]`,
  `zephyr vault add token GH_TOKEN --host api.github.com --host github.com` (asked for when
  left out; `edit NAME --host ..` replaces them; `--header X-Api-Key` names a header it is
  swapped in besides `Authorization`), `list`, `edit`, `rm` and `revoke NAME [PROGRAM]`. The agent
  is not told of these: tell it.
- `/vault bind GH_TOKEN api.github.com github.com` binds a token to hosts (`HOST` or `HOST:PORT`).
  The token is put into the `Authorization` header only; for an API that wants it elsewhere,
  `/vault header NAME X-Api-Key` adds a header (`/vault unheader NAME [HEADER]` takes it back).
  In any other header the request is refused and you are told.
  `/vault url NAME on` also swaps it in a request's path and query, for APIs that want the token
  in the URL (a server may repeat its URL back, so it is off by default). `/vault raw NAME on`
  hands the real token to the command as before: for a token used in a request body, for signing
  (AWS, webhooks), for gRPC, for a host with a private certificate or plain HTTP, or for a program
  that does not use the proxy (`himalaya`, `ssh`). `bind`, `header`, `url on` and `raw on` need a strong channel (`tui`,
  `chat` or a passkey unless you changed `settings.strong_channels`; in the web app always a
  passkey), also as the `/ok` of the agent's request;
  `/vault unbind NAME [HOST]`, `/vault url NAME off` and `/vault raw NAME off` work anywhere. The
  agent can ask for a binding (`the agent asks to send the token GH_TOKEN to api.github.com (#5)`).
- From any channel, `/vault` lists the names, `/vault rm NAME` deletes one, and `/vault revoke
  NAME [PROGRAM]` takes back a grant. A chat never takes a value: `/vault add` there is refused.
  Never paste a password into a chat.

**A certificate error on a bound host.** For the hosts tokens are bound to, zephyr's proxy
answers in the host's place, with a certificate from a CA that exists only in this container and
that the agent's tools are set up to trust (`zephyr doctor` lists the hosts and checks the CA). A
program that brings its own roots or pins certificates fails there at once, and nothing leaks.
For such a program mark the token `raw`, or bind it to a host that program does not use. If one
that used to work fails after a long uptime, restart with `zephyr up`: the CA is renewed only
when the container starts, and zephyr tells you once a day in its last 30 days. Never bind a token to a model provider's host: the agent's own
connection to the provider would stop.

Names: a login is lowercase (`github`, `my-bank`), a token is an environment variable name
(`GH_TOKEN`). The vault is for the main conversation; background tasks and jobs cannot use it.
The file is not encrypted (a passphrase would stop unattended restarts), and the daemon reads it
on every use, so an edit needs no reload. `[agent] env` still works, but hands a secret to every
command the agent runs; the vault is the better place for one.

What it protects against: credentials stay out of chats, transcripts and the model's context in
normal use; a login is filled only on its own site; and each new use needs your OK. What it does
not: an agent set on misusing a login, or a token you marked `raw`, can still read it (a filled
field read back, its own command's log under `.procs/`, a program that prints its environment to
a file; what a command prints is scrubbed from its log, what it writes to a file of its own is
not). A bound token it cannot read: if the host sends the token back (an echo or debug endpoint,
an error page), zephyr turns it into the stand-in again before the agent's program sees the
reply (as it is, in base64 as `curl -u` sends it, JSON-escaped or percent-encoded), and what a command prints reaches its log
already scrubbed. What is left: a host that sends the token back in another form (another case, HTML
entities, hex, a hash) is not caught; a bound host that needs a websocket, a range request or a compressed reply cannot
be used (the request fails, nothing leaks); a remote connector's secret is still in `mcp.json`; and the agent can still *use* a bound token on the hosts you bound. So
bind a token only to the API it is for, and grant it only to programs you trust with it.

**Mail.** There is no mail channel, but the agent can read and send mail with `himalaya`. Save the
password as a token (`zephyr vault add token MAIL_PASSWORD`, then `/vault raw MAIL_PASSWORD on`:
himalaya speaks IMAP, which the proxy cannot swap a stand-in in) and have the agent write
`/zephyr/.config/himalaya/config.toml` with the password taken from the environment:

```toml
[accounts.me]
default = true
email = "me@example.com"
backend.type = "imap"
backend.host = "imap.example.com"
backend.port = 993
backend.login = "me@example.com"
backend.auth.type = "password"
backend.auth.cmd = "printenv MAIL_PASSWORD"
message.send.backend.type = "smtp"
message.send.backend.host = "smtp.example.com"
message.send.backend.port = 465
message.send.backend.login = "me@example.com"
message.send.backend.auth.type = "password"
message.send.backend.auth.cmd = "printenv MAIL_PASSWORD"
```

The first `himalaya` command run with `secrets: ["MAIL_PASSWORD"]` asks you to allow
`MAIL_PASSWORD` for `himalaya`.

## Virtual cards

At a checkout the agent can ask for a virtual card instead of using a card of yours (DESIGN
§4.25). It is off until you pick a provider. Set your currency first: the cap and the limits
count in it.

What keeps a card safe is its own limit, not secrecy. A number typed into the agent's browser
can be read back by an agent that turns hostile or is fooled by a page, and no scrubbing stops
that. So every card is single use or holds at most the amount you approved, and the payment
window caps all money; hiding the number from the agent is a second line, not the first.

```sh
zephyr config set approvals.currency CAD
zephyr config set approvals.strong_only loosen,money    # needed for manual
zephyr config set cards.provider manual                 # or link
```

- **What you approve.** The card names what it pays by its address, the thing zephyr checks
  when it fills the card, and shows the agent's name for the shop on a line of its own:

  ```
  [zephyr] approve #7: payment (virtual card)
  pays: https://www.amazon.ca · the number is filled only there
  card: one you make for this purchase · up to 42.50 CAD · forgotten 15 min after you enter it
  merchant (the agent's words): "Amazon.ca"
  the agent says: "the 2 kg bag of rice from your list"
  window: 1 of 5 payments in the last 10 min
  /ok 7 · /no 7 · expires in 10 min · this one needs tui or chat or passkey
  ```

- **`manual`**: after your `/ok` (from the TUI, the host or the web app's passkey), make a
  card for this purchase in your own card app, and only a bounded one: a single-use card (a
  one-time card, such as Wise's), or a prepaid card with at most the approved amount on it.
  zephyr refuses any other card, your main card above all: it cannot limit a card it did not
  issue, and the agent could read the number back from its browser, so the card's own limit is
  what bounds it. Enter it on the host with `zephyr card put 7 single-use` or
  `zephyr card put 7 prepaid 42.50`, which asks for the number, the expiry and the code without
  showing them.
  The number stays in zephyr's memory, is typed only into a page on the approved address, and
  is forgotten after `cards.minutes` (15). zephyr sees no charge on such a card: check your card
  app, delete the card there, then `/card done 7` (it reminds you once a day until you do).
- **`link`** (Stripe's Link Agent Wallet): `zephyr vault add service cards --provider link
  --account CLIENT_ID`, then `zephyr card link` to sign in. Each card is then a request you
  approve in Link with your fingerprint or passkey: zephyr checks it first (the cap, the address,
  the payment window) and sends you Link's approval link, with no `/ok` of its own. The card is
  single use and closed when Link reports the charge. This provider is built from Link's
  published documents and has not yet been run with a real account.
- **The number never reaches the agent.** Wherever the number, its code or its expiry would show
  in a command's output, its log or a reply, it reads `[card:#7]`. zephyr types the card into
  the page itself, only into fields of the approved address's own page, or of a card frame
  that Stripe (`js.stripe.com` and its `*.js.stripe.com` frames) or Braintree
  (`assets.braintreegateway.com`) puts directly in that page. It checks the frame where it
  types: the frame's own origin, that its parent is the page, and that the page is on the
  approved address. No other frame is filled (an Adyen or Shopify checkout's card fields are
  not, for now), and the card closes if the page moves away while it types. Until an hour after
  the card closed, the agent's browser refuses to read that page back, a frame in it included (a
  field's value, the HTML, a script, a screenshot), and a `snapshot` of the page it typed into
  is refused until the page moves on. `/card` lists your
  cards; `/card close 7` (or `/card done 7`) closes one early, at the issuer too. `/stop` and
  `/pause` close every open card and stop a fill in hand.
- **One purchase per card.** The card lets one Pay click through, on the page zephyr filled it
  in, for a total the page shows within the cap. A Pay click with no fill before it is asked
  about like any payment. A second click there is asked about like any payment.
- **Limits** (`/settings`): `cards.max` (100) is the most one card may be for. The **payment
  window** covers every payment, not only cards: past `approvals.window.effects` payments (5)
  or `approvals.window.amount` (your `auto N` cap, when you set one) in
  `approvals.window.seconds` (600), the next payment waits for a strong channel whatever your
  approval mode, so a purchase split into several small ones is caught. A payment whose amount
  zephyr could not read is not counted against the amount: it waits for an ordinary `/ok`, as
  before. An install from before this version is told of the window once.
- `CDN$`, `CA$` and `C$` are Canadian dollars, and `$45 USD` is US dollars. A total written with a
  bare `$` is read as US dollars when your `approvals.currency` is USD, and as Canadian dollars
  when it is CAD and the shop's address ends in `.ca`. Anywhere else (a `.com` shop under CAD,
  for one) a bare `$` total counts as unread, so its Pay click is asked about rather than let
  through on the card.

## The agent's network

The container runs on a Docker network of its own, and the agent has no direct route out of it. Its
programs reach the internet only through a proxy in the daemon (`HTTP_PROXY` and friends are set
for them): any public address on ports 80 and 443, and nothing else. Your machine's localhost, your
LAN, your tailnet, cloud metadata addresses (Azure's WireServer too) and every other port (SSH,
SMTP, IMAP, databases) are refused at once, and the agent gets no DNS of its own. `git`, `curl`,
`pip`, `npm`, `gh` and the browsers work as before; `ssh` and mail clients fail until you allow
their destination, and `ping` never works.

- **Allowing more.** `/set network.allow HOST:PORT, HOST:PORT` lists exact destinations beyond
  that, e.g. `imap.gmail.com:993, github.com:22, homeassistant.lan:8123` (see
  [Changing settings](#changing-settings)). The agent can only propose a change; `/ok N` from
  `zephyr attach` or `zephyr chat` applies it. Within a few seconds a listed destination is
  reachable two ways: through the proxy, and directly, by name, for programs that speak no proxy
  (himalaya, `ssh`, a database client). Its name is the only one the agent can look up, and its
  address is looked up again every hour. Remove the entry and both ways close. A refused request tells the agent which entry to ask
  for, and `sqlite3 ~/.zephyr-daemon/state.db "select * from audit where session = 'net'"` (with
  `sudo`) shows what it reached, once per destination and hour, and everything that was refused.
- **A model server on this machine** (Ollama, a local proxy) is no longer reachable on localhost.
  Have it listen on the Docker network's gateway address
  (`docker network inspect -f '{{(index .IPAM.Config 0).Gateway}}' zephyr`) and list that
  `ADDRESS:PORT` in `network.allow`.
- **zephyr's own ports** (the attach port, the iMessage webhook) are published on this machine's
  `127.0.0.1` only. `zephyr config set attach.port N` and `zephyr channel add|remove imessage`
  start the container again to publish the change; after a hand edit of those keys, run
  `zephyr up`.
- **iMessage.** BlueBubbles posts to your Tailscale address, so forward that to the webhook once:
  `sudo tailscale serve --bg --tcp 7791 tcp://127.0.0.1:7791`. `zephyr channel add imessage` (and
  `init`'s iMessage step) offers to run it for you when `tailscale serve status` does not show it
  yet, and prints the `sudo` form when this user may not manage serve; the `zephyr up` that moves an
  install over prints the line with your port. `[imessage] listen` keeps naming the Tailscale
  address and port.
- **Upgrading.** The first `zephyr up` or `zephyr upgrade` on a version with the network floor
  moves the container over and says so once. Until `tailscale serve` forwards the webhook, iMessage
  messages do not arrive (they are picked up from BlueBubbles when the daemon next starts).
- **Going back.** `network = "host"` in `~/.zephyr-daemon/host.toml`, then `zephyr up`, runs the
  container on the host's network as before: no proxy, no rules, and the agent can reach
  everything this machine can. `zephyr doctor` says which of the two is in effect, and under the
  floor proves it by trying the forbidden connections as the agent.

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
  - `vault.toml` (0600): the [vault](#the-vault)'s logins and tokens, and what each may be used
    for. `zephyr vault` edits it; comments you add by hand are kept.
  - `facts.toml` (0600): your private facts: name, kind (`text`, `digits`, `email`), value and the
    targets each may go to. `zephyr facts` edits it. A value is matched however it is written (case,
    spacing, `+1 (555) 010-2345` against `5550102345`, percent-encoded, base64); a digits fact
    needs 6 digits and a text one 4 characters or it never matches.
  - `host.toml`: the projects folder, `network` (`"bridge"`, the [network floor](#the-agents-network), or `"host"` to run without it), optionally `memory = "8g"` to cap the container's memory (75% of the machine's when unset),
    `packages`: the apt packages `zephyr packages` bakes into the local image, and `offered_extensions`: the image extensions `init`, `up` and `upgrade` have already added to `zephyr.toml`, so one you remove stays removed. The first `up` after upgrading to a zephyr that keeps this record adds once any image extension removed earlier, or every one if the config lists none; after that, a removal sticks.
    Used only on the host, at `zephyr up`.
  - `state.db`: inbox, outbox, jobs, spend, the actions waiting for your approval and the
    recipients you approved, and the audit of tool calls. Rows older than 90 days
    are pruned (a held action that old keeps its number and summary, not its message); a reply
    that could not be sent for 7 days is given up as `expired`.
  - `backups/` (0700): a daily copy of `state.db` and five of the files above (`zephyr.toml`,
    `secrets.env`, `vault.toml`, `facts.toml` and `host.toml`; not `tui.token`), the newest 7 kept.
    `zephyr doctor` warns when the newest is over 2 days old. Copy this folder off the machine
    yourself (it holds secrets: keep the copy encrypted).

`zephyr up` caps the container's logs (100 MB), open files (65536) and memory (host.toml's
`memory`, else 75% of the machine's). A daemon that hangs is restarted inside the container after 5
minutes without a heartbeat.

An install from before this layout is moved over by the next `zephyr up`.

## When something goes wrong

**The upgrade failed, or the container will not start.** `docker logs --tail 100 zephyr` shows why.
`zephyr upgrade <previous version>` goes back: it restores that version's `state.db`,
`zephyr.toml`, `vault.toml` and `host.toml` backups (works offline when the image is still local).
If `up` failed to create the new container, the old one was put back and is running.

**Restoring by hand** (the CLI itself is broken, or you want a nightly backup):

```sh
docker stop zephyr
sudo ls ~/.zephyr-daemon/backups/                # state-YYYYMMDD.db, zephyr.toml.YYYYMMDD, ...
sudo cp ~/.zephyr-daemon/backups/state-20261105.db ~/.zephyr-daemon/state.db
sudo rm -f ~/.zephyr-daemon/state.db-wal ~/.zephyr-daemon/state.db-shm
sudo cp ~/.zephyr-daemon/backups/zephyr.toml.20261105 ~/.zephyr-daemon/zephyr.toml   # if needed
zephyr up                                        # with the CLI of the version that made the backup
```

A newer `state.db` cannot be read by an older zephyr (migrations only go forward), so restore a
backup made by the version you start, or start a newer one.

**A token leaked.** Rotate it where it was issued, then:
- the attach token: `sudo rm ~/.zephyr-daemon/tui.token`, then `zephyr doctor --fix` writes a new one
  and `zephyr restart` applies it;
- Discord, BlueBubbles or Telegram: `zephyr channel add discord|imessage|telegram` asks for the new secret;
- a provider account: `zephyr login <provider> --account NAME`;
- a vault entry: `zephyr vault edit NAME` (grants are kept);
- a connector's secret: edit `secrets.env` through `zephyr config` or `sudo`, then `zephyr reload`.

**Moving to a new machine.** `zephyr down`, then copy `~/.zephyr` and `~/.zephyr-daemon`
(`sudo rsync -aHAX`, keeping owners), and the projects folder at the same path. On the new
machine: install the CLI of the same version, `zephyr up`, `zephyr doctor`.

**The disk is full.** Look at, in this order: `docker image prune` (keep the running release and
the one before it, for going back), `~/.zephyr-daemon/*.bak` (the newest 5 versions are kept),
`~/.zephyr/sessions` (task and job sessions go after 30 days; `main` sessions stay), `.procs` logs
(50 MB each, gone 30 days after they end), and the projects' checkpoints (50 per project).

**Hearing about outages.** `zephyr alert setup <topic>` (pick a long random topic: anyone who
knows it can read it), subscribe to the topic in the ntfy app, then `zephyr alert test`.

## Claude subscription

The Claude provider runs Anthropic's official Claude Agent SDK, signed in with your own token from
`claude setup-token` (`zephyr login claude-code` runs it in the container). Anthropic's terms govern
using a subscription unattended; check that your use fits them.

## Building the image

`zephyr up --build` builds `deploy/Dockerfile` from pinned sources: this repo, `rpi-providers` from
`yansigit/rpi-package`, the Cursor provider from `crates/zephyr-cursor`, `rpi-claude-agent` from
`yansigit/rpi-provider-plugins`, and the upstream `rpi` 0.25.0 release (sha256 checked). Your `gh
auth token` goes in as the BuildKit secret `gh_token`, which is never stored in a layer. The
provider sources are pinned to commit SHAs by the build args (`RPI_PACKAGE_REF`, `PLUGINS_REF`);
local checkouts can stand in for them with `--build-context rpi-package=<dir> --build-context
provider-plugins=<dir>` (see the Dockerfile). Both provider repos are public, so no token is needed.

A smaller image leaves the owner's provider sources out: an empty repo arg skips that source.

```sh
docker build -f deploy/Dockerfile --build-arg RPI_PACKAGE_REPO= --build-arg PLUGINS_REPO= \
  -t ghcr.io/yansigit/zephyr:$(zephyr --version | cut -d' ' -f2) .   # or your ZEPHYR_REPO
zephyr up    # finds the image locally and runs it
```

Without `RPI_PACKAGE_REPO` you lose OpenCode Go, Antigravity, ChatGPT/Codex and every `[[mcp]]`
connector; without `PLUGINS_REPO`, Claude through the Agent SDK (`zephyr login claude-code`). What
stays: zephyr, the policy plugin, Cursor and rpi's own providers (the Anthropic API key). The
config still lists every extension; the agent starts without the missing ones and `zephyr doctor`
names each. `gh workflow run image.yml -f providers=none` builds and checks this variant in CI
(`:dev-minimal`, never pushed).

The build arg `SIGNAL` (default 1) puts signal-cli 0.14.10 and a Java 25 runtime cut to its needs
by `jlink` in `/opt/zephyr/signal`, for the Signal channel: 190 MB on disk (126 MB of jars, a 64 MB
runtime), 160 MB more to pull. `--build-arg SIGNAL=0`
leaves them out, and an arm64 build has none (libsignal's native library ships for x86_64 only);
without `/opt/zephyr/signal` the Signal channel is off.

Voice notes (whisper.cpp with Vulkan, a cut-down static ffmpeg, Piper and their models, about
490 MB in `/opt/zephyr/voice`) are in the image by default. `zephyr up --build` passes no build
args, so build by hand to change that, then point `ZEPHYR_IMAGE` at the tag:
- `--build-arg VOICE=0` leaves voice out; `zephyr doctor` then reads `voice: not in this image`.
- `--build-arg KO_VOICE=1` adds Piper's Korean voice (`ko_KR-kss-medium`), so replies in Korean
  are spoken too; without it they stay text. Its training data, the KSS dataset, is licensed CC
  BY-NC-SA 4.0: non-commercial use only. Whisper hears Korean either way.

```sh
docker build -t zephyr-local:ko --build-arg KO_VOICE=1 -f deploy/Dockerfile .
```

The base images (`rust`, `node`, `ubuntu`, uv and the Dockerfile frontend) are pinned by digest,
which `deploy/bump-rpi.sh` does not move: to take a tag's security fixes, read its multi-arch digest
with `docker buildx imagetools inspect ubuntu:24.04 --format '{{json .Manifest.Digest}}'` and
replace it in `deploy/Dockerfile`.

Pushing a `v*` tag publishes `ghcr.io/yansigit/zephyr:<version>` (`.github/workflows/image.yml`,
which tags `ghcr.io/<the repository it runs in>`, so a fork publishes to its own).

The CLI runs and upgrades from `ghcr.io/yansigit/zephyr` unless `ZEPHYR_REPO` names another
repository: in the environment for one shell, or at build time under its own name
(`ZEPHYR_BUILD_REPO=… cargo install …`, or `--build-arg ZEPHYR_BUILD_REPO=…` for the image's own
CLI), which the CLI keeps; a `ZEPHYR_REPO` exported while you build is not baked in. `ZEPHYR_IMAGE`, one
exact image, wins over both. Your `gh` token only ever goes to ghcr.io; for another registry,
`docker login` to it yourself.

## Releasing

From a clean `main` that matches `origin/main`, after the change is merged:

```sh
deploy/release.sh 0.2.1   # the image changed: publish it and upgrade the live install
deploy/release.sh --cli   # only the host CLI changed: reinstall it, publish nothing
```

Every PR runs fmt, clippy (`-D warnings`, Rust 1.95 as the image) and the tests, and `cargo deny check
advisories bans` (`.github/workflows/ci.yml`, see [CI](#ci)). The script stops at the first problem,
and first at a version that is not above `Cargo.toml`'s. It runs fmt, clippy and the tests, then
the real-rpi suite (`ZEPHYR_REAL_RPI=1 cargo test -- --ignored`) against the pinned binary, fetched
when it is missing; with no binary it stops (`RPI_BIN=<path>` points it at another copy; the default
is the pinned path). Then it sets the version in `Cargo.toml` (the CLI pulls
`ghcr.io/yansigit/zephyr:<version>`), commits and tags `v<version>`, pushes both at once (`git push
--atomic`: the commit and the tag land together or not at all), and waits for the image workflow. Then it installs the CLI into `~/.cargo/bin` (from the
clean tree, so nothing uncommitted ships) and runs `zephyr upgrade <version>`.

`zephyr upgrade` waits until nothing is in flight: a message the agent is answering, a job run, a
background task or an `exec` command. Replies waiting for a channel (a notice for a TUI nobody has
attached, say) and messages held by `/pause` don't count, since they never clear by themselves. After
30 minutes busy it gives up and says to send `/stop` and run it again. Then it backs up `state.db`,
`zephyr.toml`, `vault.toml` and `host.toml` as `~/.zephyr-daemon/<file>.<old version>.bak`
(migrations only go forward; the newest 5 versions are kept) and replaces the container. `zephyr
upgrade <older version>` goes back, restoring that version's backups, also after `zephyr down` and
offline when the image is still local. `zephyr up`, `doctor --fix` and `init` back up the same way
when the running container is another version than the CLI's.

`zephyr up` stops the old container (so its daemon drains) and keeps it as `zephyr-old` until the
new one runs; if the new one cannot be created, the old one is put back. `.env*` files under the
projects folder are masked from the agent at each `up` (not inside `.git` or `node_modules`; a path
with a comma cannot be masked and is named in a warning).

The image workflow smoke-tests the built image (zephyr, rpi, the policy plugin and every extension)
before it pushes, and tags `:<version>` and `:latest`. It runs on GitHub-hosted runners, or on
this machine's runner (below, building on its rootless BuildKit, without the smoke test) when
`ZEPHYR_CI=local` or GitHub refused a hosted job this month and started none since (see CI
below). If the image run itself is refused for minutes, `gh run rerun <id>` (the whole run, not
`--failed`) plans again and builds here. `gh workflow run image.yml --ref <branch>` builds a test
image (`:dev`) without pushing it; `-f local=true` builds it here.

### CI

CI runs on GitHub's runners and on the owner's own, label `zephyr-local`, at once, and gets by
with either. `.github/ci.sh` holds the placement and the checks; the jobs are:

- `plan` decides where everything goes, from repo variable `ZEPHYR_CI`:
  - unset or `both`: everything here, costing no hosted minutes however often a PR is pushed,
    but for the nightly run (09:17 UTC), whose `test` runs on GitHub-hosted, a clean 2-CPU
    machine, so GitHub's environment, and the `hosted` path below, are still tested daily. If
    GitHub refused a hosted job (spending limit or quota) this month, and no hosted job has
    started since, everything goes here too for the rest of the month; the first run of the next
    month tries hosted again. To try sooner, after a raised limit, run
    `gh workflow run ci.yml -f hosted=true`: it tests on GitHub-hosted whatever was refused, and a
    hosted job that starts ends the refusal.
  - `local`: everything here (`gh variable set ZEPHYR_CI --body local`).
  - `hosted`: everything on GitHub, for when this machine is down. `plan` needs a runner too, so
    in the other modes a run waits (up to a day) while no `zephyr-local` runner is online.
- `plan` also says when no checks are needed, and `ci-ok` then passes saying why: every file a PR
  changed (against its base) or a push changed (against the commit before it, every commit it
  brought), both sides of a rename counted, is Markdown outside `.github/`, `crates/` (which
  embeds files) and `deploy/` (whose `SKILL.md` files ship in the image); no test reads
  `README.md`, `DESIGN.md` or `CLAUDE.md`. Or the push to main is the merge of a PR whose head has
  the same tree (the PR was rebased onto main) and passed: a successful `ci.yml` run of that pull
  request, whose own `ci-ok` check-run, made by GitHub Actions in that run's check suite, passed (a
  check-run of the same name from another workflow or app counts for nothing). A force push, a new
  branch, or anything the API does not answer runs everything, and so do the nightly and manual
  runs. A PR's newer push cancels its older run; a push to main and the nightly run are never
  cancelled, and a cancelled run posts no failed `ci-ok`.
- Two checks, each compiling once, side by side on the two runners: `lint` (shellcheck, fmt,
  `cargo deny check advisories bans`, clippy) and `test`, the whole suite in one `cargo nextest
  run` (`.config/nextest.toml` profile `ci`). A test that passes only on its one retry is listed
  as flaky in the job summary; one that hangs is killed after 3 minutes.
- `real-rpi`, a third check, runs the ignored tests against the pinned rpi release: `ci.sh` fetches
  the Dockerfile's x86_64 tarball, checks it against its sha256 and that `rpi --version` is the pin,
  then runs `ZEPHYR_REAL_RPI=1 cargo test --workspace -- --ignored` with `RPI_BIN` set to it. It is
  added when the pinned rpi is what is tested: on the nightly run, on an `rpi-bump/*` branch, when
  the commit moves `RPI_VERSION` (or its parent cannot be read), and on `gh workflow run ci.yml -f
  real_rpi=true`. It runs on `zephyr-local` (x86_64 only) and `ci-ok` counts it. `rescue` never
  reruns it past a failure.
- `rescue` runs here, after them, each check GitHub did not start (failed with no runner and no
  step; the job's annotation says why). A check that started and failed is never rescued. Its
  token only asks the Actions API which checks those are; the step that runs them has none.
- `ci-ok` passes when every check passed on its own runner or through `rescue`. It is the one
  check to gate merges on (`gh pr checks <n>`). A rescued run still shows the refused job's red X;
  under the monthly rule that happens at most once a month, at the first refusal, not each night.

`rpi-bump.yml` proposes an rpi bump and starts this workflow on its branch for the `real-rpi` check
(below); `image.yml` as above.

The runners come from `deploy/runner/start.sh up [N]` (default 2), from the image
`deploy/runner/Dockerfile` builds: GitHub's runner image with Rust 1.95, nextest, cargo-deny, gh,
tmux, python3, node and shellcheck. Each gets `ZEPHYR_RUNNER_CPUS` (default 12) CPUs and
`ZEPHYR_RUNNER_MEMORY` (default 24g), and `warm` below 6 CPUs and 12g (`ZEPHYR_WARM_CPUS`,
`ZEPHYR_WARM_MEMORY`): 30 at most, on a machine of 28, with all three busy at once, which is rare
and only slows them down. They are ephemeral: a runner registers with `--ephemeral`, takes one job, and its
container is removed when it exits; systemd user unit `zephyr-runner@<i>` (which `up` installs,
running a copy of the script in `~/.local/share/zephyr-runner/`) starts a fresh one from the image.
The checkout, the cargo registry and target dir, the home and anything a job's code wrote there go
with its container, and the next job cannot run what an earlier one left behind.

A job still starts warm. Systemd timer `zephyr-ci-warm` (`start.sh warm`, every 10 minutes) reads
`heads/main` with the owner's `gh`; if it moved, it fetches that commit's tarball and, in a
throwaway container from the runner image, started as a runner is (the runner user, no
capabilities, no-new-privileges), builds what `lint` and `test` compile into Docker volume
`zephyr-ci-cache`, in a folder of its own renamed into place when done; then `/cache/current`
points at it in one rename (the build before is kept for a job still copying it). Every runner
mounts that volume read only; a job copies main's build into its own target dir (about 10 s for
12 GB) and touches its checkout, so every file of the branch is newer than anything copied. The
dependencies then come from the copy; the workspace's own crates are rebuilt every time,
incrementally from main's build (34 s to compile what both checks need, against 219 s cold;
without incremental compilation the copy is 8 GB but that takes 104 s). Why PR code can never
write it: the writer is outside Actions, on the host, and builds only main's commit; a runner's
mount is read only, whatever label a PR's workflow file picks; and no workflow names the volume.
A PR can read main's build, which it could build itself. What this trusts, as the releases do, is
that only the owner's own merges reach main: this free private repo has no branch protection, so a
workflow a PR from this repo starts can ask for `contents: write`, and a PR that edits `.github/`
needs a person to read it before it is merged. A job runs as the runner
user with no capabilities, no-new-privileges and no sudo. Run `up` again after changing the
Dockerfile or the limits: each slot's next job uses them, and a runner started otherwise (or one
from before ephemeral runners, with its `zephyr-runner-<i>-work` volume) is unregistered once
idle, never mid-job, and replaced. `deploy/runner/start.sh down` stops the units and unregisters
the runners. `up` turns on linger (or says the command), so the units outlive a logout. The units
reach GitHub with `gh`'s own login (`~/.config/gh/hosts.yml`); if `gh` keeps it in a keyring a
user service cannot reach, put `GH_TOKEN=<token>` in `~/.config/zephyr-runner/env` (mode 0600). A
failed login or registration retries after 20 s, doubling up to 10 min.

The rootless BuildKit (`zephyr-buildkitd`) answers only a client with the certificate `up` makes
(mTLS; the CA and keys in `~/.config/zephyr-runner/buildkit/`, 0700) and stores as repo secrets
`BUILDKIT_CA`, `BUILDKIT_CERT` and `BUILDKIT_KEY`, which `image.yml` alone reads. A test job on
the same network can reach the daemon but not use it. Each build step runs in its own process
namespace (BuildKit's sandbox). A release (a `v*` tag) built here uses no cache at all, so what a
branch's image build left in the cache never reaches a release. The runners register with
`yansigit/zephyr`, or with `ZEPHYR_GH_REPO=<owner/repo>` for a fork. They and the rootless BuildKit
sit on their own Docker network with no Docker socket, no access to the host's files and no route
to its localhost; services the host serves on all interfaces (SSH, ComfyUI, ...) are still
reachable.

### Keeping rpi current

`.github/workflows/rpi-bump.yml` proposes a bump each day at 21:17 UTC, when crates.io has a newer
`rpi-tui` than the Dockerfile's `RPI_VERSION`, and merges a tested one in a sweep at 22:47, 00:47
and 03:47 UTC. By hand: `gh workflow run rpi-bump.yml [-f version=X.Y.Z]` proposes that version (the
newest by default), and `gh workflow run rpi-bump.yml -f merge=true` only looks for a tested bump to
merge. It never releases: tag as above to ship an image with the new rpi.

`propose` runs `deploy/bump-rpi.sh` in edits-only mode, in a step with no token. It moves every pin:
the Dockerfile's version and both sha256s (from the release's own `.sha256` files, checked against
the tarballs), `RPI_PIN`, the `=` Cargo pins and `Cargo.lock`, fake-rpi's `--version` and the
real-rpi test path. The new binary is never unpacked or run, and nothing is built. The one commit
goes to `rpi-bump/v<version>`, the PR is opened or updated, and `ci.yml` runs on that branch with
`real_rpi=true`: the ordinary checks, plus `real-rpi` (above). A new commit always gets a CI run;
an unchanged one gets one only if none exists. The PR's body says whether the change is a pure pin
move (`bump-check.py verify`, run on the edited tree before the PR opens).

A branch is the bot's only while `github-actions[bot]` both wrote and committed every commit on
it. One you amended, rebased or added to is yours: propose never replaces it and never closes its
PR. The bot's own PR for a lower version is closed when a newer release is proposed, unless it is
one commit on the current main with its CI running or passed, which the sweep may still merge. To
decline a bump, close its PR: the daily run does not propose that version again (`-f
version=X.Y.Z` does).

`merge` never checks out or builds the PR. Of the open `rpi-bump/v<version>` PRs above the pin it
takes the highest whose commit has proof: the newest `ci.yml` run started by `workflow_dispatch`
for that exact commit (a cancelled one aside) has `ci-ok` and `real-rpi` passing, so a later run
that failed takes an earlier pass back, and a lower version you proposed by hand is merged while
the newest release's PR is red. It fetches the commit, requires it to be one commit on main, and
reads its diff as bytes: `bump-check.py patch` gates them, `git apply` applies them to main's
checkout, and `bump-check.py verify` checks what git wrote, which must equal the commit's tree. Only then does it
fast-forward `main` to that commit. A `main` that moved meanwhile refuses the push, and GitHub marks
the PR merged when it lands. The steps that apply the diff hold no token, and the commit CI
tested is the commit main gets.

A bump that is not a pure pin move, such as one that needs another crate moved in `Cargo.lock`, is
still proposed, with its PR saying "Not auto-merged"; a person merges or fixes it. A failing
`ci.yml` run leaves the PR open with its run linked. The sweep skips the PR, and propose does not
start CI again for that commit. A person pushes a fix to the branch or merges by hand.

The repo needs "Allow GitHub Actions to create and approve pull requests" (Settings > Actions >
General). No secret is needed: a PR opened with `GITHUB_TOKEN` starts no `pull_request` run, so
propose dispatches `ci.yml` itself, and dispatched runs always run. A future required-check ruleset
on `main` would need a GitHub App token, since checks from `workflow_dispatch` runs do not satisfy a
ruleset. `deploy/bump-rpi.sh [version]` does the same edits locally; `release.sh` fetches the pinned
binary when it is missing.
