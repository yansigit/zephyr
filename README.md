# Zephyr

An always-on personal agent gateway over upstream `rpi --mode rpc`, running in one Docker container.
It talks to you through its own terminal UI (or the stock rpi TUI), a line chat, Discord and iMessage. See [DESIGN.md](DESIGN.md).

## Quick start

You need Docker and, for the image from GitHub, the `gh` CLI signed in to an account that can read `yansigit/zephyr`.

```sh
cargo install --path crates/zephyr   # the same binary runs on the host and in the container
zephyr init                          # the wizard; safe to run again, it offers the current values
```

Install without Rust (Linux x86_64; while the package is private, `docker login ghcr.io` first):

```sh
docker run --rm --entrypoint cat ghcr.io/yansigit/zephyr:latest /usr/local/bin/zephyr > ~/.local/bin/zephyr && chmod +x ~/.local/bin/zephyr
```

`zephyr init` checks Docker, asks for the projects folder the agent may work in, pulls the image
(`docker login ghcr.io` with your `gh` token when needed; it builds locally when it cannot pull),
starts the container, signs in the providers you pick, sets the failover order and the channels,
then runs `zephyr doctor`.

Providers: OpenCode Go, Antigravity, ChatGPT/Codex, Claude (through the Claude Agent SDK), Cursor
(through its agent service, tools included) and the Anthropic API key. Each sign-in is an account
under `~/.zephyr/accounts/<name>/`, listed in `[[account]]` in `~/.zephyr-daemon/zephyr.toml`; when one account is rate limited, the next takes over.

Without prompts:

```sh
zephyr init --non-interactive --projects ~/Projects \
  --account codex-main=openai-codex/gpt-5.5 --model openai-codex/gpt-5.5
zephyr login openai-codex --account codex-main
```

## Everyday commands

| Command | What it does |
|---|---|
| `zephyr up [--build]` / `down` / `restart` | Start (building the image locally with `--build`), stop, restart |
| `zephyr upgrade [VERSION]` | Move to the newest release (updating this CLI from the image first), once the agent is idle, after backing up `state.db` to `~/.zephyr-daemon/state.db.<old version>.bak`. With an older VERSION, go back to it by restoring that version's backup |
| `zephyr status` / `logs [-f]` / `doctor [--fix]` | Container and daemon state, logs, checks (and repairs) |
| `zephyr --version` | This CLI's version and the running container's, with a hint when they differ |
| `zephyr login <provider> [--account NAME]` | Sign in an account, then make a small test call (`--no-verify` skips it) |
| `zephyr accounts [list \| order NAME.. \| remove NAME]` | The failover pool |
| `zephyr model [--account NAME] [PROVIDER/MODEL]` | An account's model (or the default without accounts); with no model, pick from `rpi --list-models` |
| `zephyr channel add\|remove discord\|imessage \| list` | Channel settings (secrets are asked for hidden); `remove` drops the channel, its owners and secrets, `list` shows them. Discord shows typing while the agent works, and a progress note after 20 s (`discord.progress`, `discord.progress_after_secs`) |
| `zephyr skill add DIR \| list \| rm NAME` | Skills the agent may use (see [Skills and connectors](#skills-and-connectors)); one you add is approved at once |
| `zephyr mcp add NAME (--url URL [--header K=V].. \| -- CMD ARGS..) [--secret NAME].. \| list \| rm NAME` | Connectors (MCP servers); each `--secret` is asked for hidden and fills in `${NAME}` |
| `zephyr packages add NAME.. \| rm NAME.. \| list` | Ubuntu (apt) packages for the agent, baked into a local image (`zephyr-local:<version>-<hash>`) on the release one; `up` and `upgrade` build it, and `add`/`rm` rebuild and recreate the container once the agent is idle |
| `zephyr vault list \| add login NAME --site URL [--user U] [--totp] \| add token NAME \| edit NAME \| rm NAME \| revoke NAME [PROGRAM]` | Logins and tokens the agent uses by name (see [The vault](#the-vault)); values are asked for hidden, or read one per line from stdin when piped, never taken as arguments |
| `zephyr config get\|set KEY [VALUE]` | Edit `zephyr.toml` (comments are kept; invalid keys are refused) |
| `zephyr attach [--stock]` | Zephyr's terminal UI on the live session: history, tool calls, what the agent is doing, the queue and today's spend; `^D` detaches and the agent keeps running. `--stock` runs the stock `rpi --connect` client instead |
| `zephyr shell [bash\|zsh]` | A login shell in the container as the agent, in its home (`/zephyr`) and on its desktop; needs a terminal |
| `zephyr chat` | The same UI on a terminal; a line client (`< reply` per line) when piped or scripted |
| `zephyr pause` / `resume` | Stop and restart dispatch (`/pause` and `/resume` from any channel do the same) |
| `zephyr reload` | Apply hand edits now instead of within 5 s (`/reload` from any channel) |
| `zephyr uninstall [--keep-data]` | Remove the container and its images (the local package ones too), and `~/.zephyr` unless kept |

Sign in from the running gateway too, with no re-exec afterwards. In `zephyr attach`, `/login`
lists the providers and Enter starts the one you pick: an OAuth provider (Antigravity,
ChatGPT/Codex) prints a URL to open in a browser on your machine; nothing else is needed. The
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

Moving around in `zephyr attach`:

| Key | What it does |
|---|---|
| `Ctrl+F` | Search the conversation: the newest match first, `Enter` or `Ctrl+G` the one above, `Shift+Enter` or `Ctrl+Shift+G` the one below, `Esc` closes |
| `Ctrl+Up` / `Ctrl+Down` | Jump to your previous or next message |
| `Home` / `End` | Top or bottom of the conversation, when the composer is empty |
| `/focus` | Hide tool calls and thinking (again to show them); `focus` shows in the footer |

The window title shows `◇` idle, `✦` working and `✋` when something waits for your OK, and the tab
shows a progress bar while the agent works. When you are in another window and a reply finishes or
an OK is asked for, the terminal gets a notification (`zephyr: reply ready` or `needs your OK`, at
most one every 10 s). iTerm2, WezTerm, kitty, Ghostty and Windows Terminal show these; other
terminals just ring the bell, or nothing.

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

Drafts put aside are gone when you detach; the history starts again from the messages in the conversation.
Mid-reply in `zephyr attach`, Enter steers your message into the running turn; Alt+Enter instead
holds it until the reply ends and then sends it as a request of its own (when idle, both just
send). Alt+Up takes back your messages that have not reached the agent yet (held follow-ups, or
ones waiting behind `/pause`) and puts them in the input to edit; a steered one is already the
agent's. Shift+Tab steps the thinking level, Ctrl+P the model (Ctrl+Shift+P back, where the
terminal tells them apart), and the footer shows `ctx N%`, how full the model's context is after
the last reply.

## Changing settings

Changes apply by themselves; nothing needs `zephyr restart`. The commands above, and hand edits to
`zephyr.toml` or `secrets.env` (seen within 5 s), go to the running daemon, which says what it did:

| Change | Applies |
|---|---|
| Budget, owners, time zone, `[auth]`, the pool order | at once |
| The model, on the same provider | at once, or after the reply in progress (rpi's `set_model`) |
| A model on another provider, the active account, extensions, connectors (`[[mcp]]`), a re-login | the agent restarts after the reply in progress; the conversation carries on |
| Discord, iMessage, `[attach]`, `[agent]`, a secret | the daemon restarts itself in place once idle; waiting messages are kept |

A file that does not load is not applied, and the daemon keeps its current settings. From any
channel: `/model` shows the models, `/model PROVIDER/MODEL` switches (to the account on that
provider), `/thinking` shows how hard the model thinks and `/thinking LEVEL` or `/thinking next`
changes it from the next request (kept until the daemon restarts), `/status` shows the agent's account and model, `/reload` re-reads the files, and `/new`
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
`zephyr attach`, which masks it), and `/login cancel [ID|PROVIDER]` stops the ones you gave up on.
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

## What you are told

- After a turn that ran a command sending data off the machine (curl or wget with a body, ssh, scp,
  rsync to a host, nc, `git push`, ...), zephyr lists them: `[zephyr] outbound this turn: ...`.
  Jobs report theirs on the job's channel. Plain downloads are not listed. In the agent's browser,
  typing, uploading and running script on a page count (`agent-browser fill`, `type`, `keyboard`,
  `upload`, `eval`); plain browsing does not.
- When the agent repeats a tool call that keeps failing the same way, zephyr tells it after 3 tries
  to change approach, and after 5 stops the run: `[zephyr] stopped a run that repeated a failing …
  call 5 times`. Jobs and background tasks end `failed` the same way.
- Notices go to the channel you last wrote from. Under `zephyr attach` they appear as messages in
  the conversation, or at your next attach if none is open.
- When the agent changes its memory (`memory/USER.md`, `MEMORY.md`), you get the diff. The change
  reaches the agent's context only after `/memory ok` (if it changed after you were sent the diff,
  you are shown the new one first); `/memory undo` drops it, `/memory diff` shows
  it again, `/memory log` lists commits. A web page cannot plant a lasting "preference" without you.
  For the same reason rpi skills are off, only the image's extensions load, and a `SYSTEM.md`, `APPEND_SYSTEM.md`, `CLAUDE.md` or
  extra `AGENTS*.md` the agent writes is renamed `*.ignored-by-zephyr` before the next start.
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

**Connectors** are MCP servers the agent may call. Add one on the host:

```sh
zephyr mcp add docs --url https://example.com/mcp --header 'Authorization=Bearer ${DOCS_TOKEN}' --secret DOCS_TOKEN
zephyr mcp add files -- npx -y @modelcontextprotocol/server-filesystem /zephyr/projects
```

This writes an `[[mcp]]` table to `zephyr.toml` (`name`, then `url` and `headers`, or `command`,
`args` and `cwd`; also `env`, `timeout`, `enabled`), and the agent restarts after the reply in
progress. `${NAME}` is filled in from `secrets.env`; a connector whose secret is missing is left
out, and `zephyr doctor` and `/mcp` say so. The agent may also propose one in
`connectors/drafts/<name>.json`: `/mcp ok NAME` adds it, `/mcp no NAME` drops it, `/mcp rm NAME`
removes a connector, and `/mcp` lists them. The agent can use only these connectors: an MCP config
it writes itself, or a server named by URL, is refused. A connector runs as the agent, so the
agent can read its token.

## Sending the agent files

Images and files you send on Discord or iMessage are saved for the agent in `~/.zephyr/inbox/`
(`/zephyr/inbox/` in the container), and your message tells it each file's path; it opens images
with `read`. A message with only a photo works. Up to 10 files a message, 25 MB each: anything
beyond is listed but not downloaded. Only owners' files are downloaded. iPhone photos arrive as
JPEG (BlueBubbles converts HEIC). A voice note is saved as a file; nothing transcribes it. Files
are deleted after 30 days.

## Limits

`[budget]` in `zephyr.toml`, all optional: `daily_tokens` and `daily_cost_usd` pause the gateway
for the rest of the UTC day (`/resume` overrides); `run_turns` and `run_minutes` stop one run that
goes on too long, and you are told why. A job run also stops after 30 minutes.

## Standing rules

`/rule never push to main` adds a rule the agent sees first in every session, above its memory,
from the next message on. `/rules` lists them, `/rule rm 2` removes one. Rules live with the
daemon, out of the agent's reach, and do not fade when a long conversation is summarised the way
instructions given in chat can. `/new` starts a fresh conversation; rules and memory carry over, and `/switch` goes back to an earlier one.

## The agent's browser and desktop

The image gives the agent a browser and a screen of its own (DESIGN §4.11):

- **Browser.** fingerprint-chromium (ungoogled-chromium with fingerprint patches; Playwright's
  Chromium on arm64), driven by the `agent-browser` CLI. Its profile is `~/.zephyr/browser`, kept
  across restarts. It is the agent's, never yours, and holds only the sign-ins you give it: through
  [the vault](#the-vault), or by hand (below).
  A site that still blocks it gets Camoufox, a stealth Firefox, which keeps no sign-ins.
- **Python.** A read-only environment first on the agent's PATH (`/opt/zephyr/py`: requests,
  beautifulsoup4, pandas, pypdf, pillow, Camoufox), locked in `deploy/python/uv.lock`, plus `uv` for
  anything else (`uv run --with`, or a venv of its own; uv's cache lives in `~/.zephyr/.cache/uv`).
  To add a package for every run, edit `deploy/python/pyproject.toml`, run `uv lock` there, and
  rebuild.
- **Command-line tools.** `gh` (GitHub, once you give the agent a token), `himalaya` (mail, below),
  `jq`, `rg`, `fd`, `sqlite3`, `ssh`, `zip`/`unzip`, `tree`, `file`, `zsh`, and node with `npm` and
  `npx`. Writes through `gh` (`pr create`, `issue comment`, `api -X POST`, ...) and mail
  `himalaya` sends are in the turn's outbound list.
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
- **Watching it, or signing it into a site.** Nothing listens by default. To look, start a one-shot
  VNC server on 127.0.0.1:5900 and connect a VNC viewer; it exits when you disconnect:

  ```sh
  docker exec -it -u "$(id -u)" zephyr x11vnc -storepasswd /zephyr/.vnc-passwd   # once
  docker exec -d -u "$(id -u)" zephyr x11vnc -display :99 -auth /zephyr/.zephyr-xauthority -localhost -once -rfbauth /zephyr/.vnc-passwd
  ```

- **Files and jobs.** The agent's `send_file` tool sends you a file (up to 10 MB: a screenshot, a
  PDF) with its reply. Its `schedule` tool only proposes a job: you are shown it, and it runs after
  `/job ok N` (`/job rm N` drops it; `/job list` marks the ones waiting).
- **Commands.** The agent's `gateway` tool runs `/status`, `/model`, `/switch`, `/rules`,
  `/memory diff`, `/memory log` or `/job list` and reads the output in the same turn. It can only
  propose `/model PROVIDER/MODEL`, `/new`, `/switch ID`, `/pause` or `/reload`: you are asked
  (`the agent asks to run /new (#3)`), `/ok 3` runs it exactly as if you had typed it, and `/no 3`
  drops it (a proposal lapses after 24 hours). Approvals (`/ok`, `/memory ok`, `/job ok`, `/rule`,
  `/login`, `/resume`) and `/stop` are never proposable, so text the agent read cannot approve
  itself. In a scheduled job it gets no answer and proposes nothing.
- **Background tasks.** You can keep talking while the agent works. It hands anything that takes
  more than a minute to a task (its `task` tool): a fresh session running beside the conversation,
  at most 2 at once. The agent answers right away, and when the task ends its report comes back to
  the agent, which tells you. Ask it to cancel, or use `/task list` and `/task stop N`. `/stop`
  stops the reply and every task. A task cut off by a restart or a settings reload picks up where
  it left off in its own session, told to check what was in progress before repeating anything (at
  most 3 times; then it is reported as unfinished).
- **Talking to it while it works.** A message you send mid-run reaches the agent after the step
  it is on, and its answer comes back right away, not when the whole job is done. The agent runs
  commands through its own `exec` tool: when you send a message during a long command (or after
  10 minutes, or for a server it starts), the command moves to the background without being
  stopped, the agent answers you, and you get its result as a message when it exits.
  `/process list` and `/process stop N` show and stop them; `/stop` stops them too. A settings
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

When the agent wants to use one, you are asked: `the agent asks to use the token GH_TOKEN with gh
(#4); /ok 4 allows it from now on, /no 4 drops it`. When one is missing, the agent asks you to
save it and names the exact command. Save and manage entries:

- In `zephyr attach`, `/vault` opens a panel: `a` adds a login, `t` a token, `e` edits, `d`
  deletes, `r` takes back what an entry is allowed for. `/vault add login NAME SITE` and
  `/vault add token NAME` open the form filled in. Values are typed masked. A new entry saved
  here is told to the agent, so it can carry on.
- On the host, `zephyr vault add login github --site https://github.com --user me [--totp]`,
  `zephyr vault add token GH_TOKEN`, `list`, `edit`, `rm` and `revoke NAME [PROGRAM]`. The agent
  is not told of these: tell it.
- From any channel, `/vault` lists the names, `/vault rm NAME` deletes one, and `/vault revoke
  NAME [PROGRAM]` takes back a grant. A chat never takes a value: `/vault add` there is refused.
  Never paste a password into a chat.

Names: a login is lowercase (`github`, `my-bank`), a token is an environment variable name
(`GH_TOKEN`). The vault is for the main conversation; background tasks and jobs cannot use it.
The file is not encrypted (a passphrase would stop unattended restarts), and the daemon reads it
on every use, so an edit needs no reload. `[agent] env` still works, but hands a secret to every
command the agent runs; the vault is the better place for one.

What it protects against: credentials stay out of chats, transcripts and the model's context in
normal use; a login is filled only on its own site; and each new use needs your OK. What it does
not: an agent set on misusing a value it was allowed to use can still read it (a filled field
read back, its own command's log under `.procs/`, a program that prints its environment to a
file). Grant a token only to programs you trust with it.

**Mail.** There is no mail channel, but the agent can read and send mail with `himalaya`. Save the
password as a token (`zephyr vault add token MAIL_PASSWORD`) and have the agent write
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
  - `host.toml`: the projects folder, optionally `memory = "8g"` to cap the container's memory,
    `packages`: the apt packages `zephyr packages` bakes into the local image, and `offered_extensions`: the image extensions `init`, `up` and `upgrade` have already added to `zephyr.toml`, so one you remove stays removed. The first `up` after upgrading to a zephyr that keeps this record adds once any image extension removed earlier, or every one if the config lists none; after that, a removal sticks.
    Used only on the host, at `zephyr up`.
  - `state.db`: inbox, outbox, jobs, spend.

An install from before this layout is moved over by the next `zephyr up`.

## Claude subscription

The Claude provider runs Anthropic's official Claude Agent SDK, signed in with your own token from
`claude setup-token` (`zephyr login claude-code` runs it in the container). Anthropic's terms govern
using a subscription unattended; check that your use fits them.

## Building the image

`zephyr up --build` builds `deploy/Dockerfile` from pinned sources: this repo, `rpi-providers` from
`yansigit/rpi-package`, the Cursor provider from `crates/zephyr-cursor`, `rpi-claude-agent` from
`yansigit/rpi-provider-plugins`, and the upstream `rpi` 0.18.0 release (sha256 checked). Your `gh
auth token` goes in as the BuildKit secret `gh_token`, which is never stored in a layer. The
provider sources are pinned to commit SHAs by the build args (`RPI_PACKAGE_REF`, `PLUGINS_REF`);
local checkouts can stand in for them with `--build-context rpi-package=<dir> --build-context
provider-plugins=<dir>` (see the Dockerfile).
Pushing a `v*` tag publishes `ghcr.io/yansigit/zephyr:<version>` (`.github/workflows/image.yml`).

## Releasing

From a clean `main` that matches `origin/main`, after the change is merged:

```sh
deploy/release.sh 0.2.1   # the image changed: publish it and upgrade the live install
deploy/release.sh --cli   # only the host CLI changed: reinstall it, publish nothing
```

The script stops at the first problem. It runs fmt, clippy and the tests, sets the version in
`Cargo.toml` (the CLI pulls `ghcr.io/yansigit/zephyr:<version>`), commits and tags `v<version>`,
pushes, and waits for the image workflow. Then it installs the CLI into `~/.cargo/bin` (from the
clean tree, so nothing uncommitted ships) and runs `zephyr upgrade <version>`, which waits until no
message, reply or job is in flight and backs up `state.db` to
`~/.zephyr-daemon/state.db.<old version>.bak` (migrations only go forward). `zephyr upgrade <older
version>` goes back, restoring that version's backup.

The image workflow tags `:<version>` and `:latest` and runs on GitHub-hosted runners. When the
Actions minutes run out, start the fallback runner on this machine once with
`deploy/runner/start.sh up`, then `gh variable set ZEPHYR_RUNNER --body zephyr-local` and
`gh run rerun <id>`; `gh variable delete ZEPHYR_RUNNER` switches back, and
`deploy/runner/start.sh down` removes it. The runner and its rootless BuildKit sit on their own
Docker network with no Docker socket, no access to the host's files and no route to its
localhost; services the host serves on all interfaces (SSH, ComfyUI, ...) are still reachable.
`gh workflow run image.yml --ref <branch>` builds a test image (`:dev`) without pushing it.
