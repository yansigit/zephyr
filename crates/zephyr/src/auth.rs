//! Signing a provider in on the live gateway: the `zephyr.login_*` attach methods
//! (DESIGN §4.3.1) and the `/login` command (DESIGN §4.2), on one registry.
//!
//! `rpi auth login --provider <p>` wants a terminal, prints a sign-in URL for an OAuth provider
//! or waits for a key, and writes the credential into the `RPI_CODING_AGENT_DIR` it runs with.
//! So a login is one `script`-wrapped subprocess on the account's own agent dir, exactly as
//! `zephyr login` runs it on the host, but held in a process-global registry that the attach
//! socket and the router both drive, and reported line by line while it runs.
//!
//! Nothing here restarts anything. A finished login writes `zephyr.toml`; the router's own tick
//! (`edits`/`reload`) reads the new pool, and a new `auth.json` for the account the child runs on
//! makes it restart the child once idle (`Plan::RestartChild`). A login never re-execs the daemon.
//!
//! Tokens are never logged, stored or returned. What a caller sends into a login (the key) is
//! kept only to redact the pty's echo from the output `login_poll` reports, and command replies
//! never carry it.

use crate::config::{Account, Config};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// `script`, by full path: run as the agent, a program of that name on its PATH would be the
/// daemon's child while a sign-in runs.
const SCRIPT: &str = "/usr/bin/script";

/// How a provider takes its credential; `zephyr.login_providers` reports it as `kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// A browser or device sign-in: `rpi` prints a URL and finishes on its own.
    Oauth,
    /// An API key, pasted at the prompt.
    Key,
    /// A long-lived token printed by a vendor CLI (`claude setup-token`).
    SetupToken,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Oauth => "oauth",
            Kind::Key => "key",
            Kind::SetupToken => "setup_token",
        }
    }
}

/// One provider `rpi auth login --provider` takes.
pub struct Provider {
    pub id: &'static str,
    pub label: &'static str,
    pub kind: Kind,
}

/// The providers `zephyr init` offers and `/login` accepts, in that order. The ids are rpi's.
pub const PROVIDERS: [Provider; 6] = [
    Provider {
        id: "opencode-go",
        label: "OpenCode Go",
        kind: Kind::Key,
    },
    Provider {
        id: "antigravity",
        label: "Antigravity",
        kind: Kind::Oauth,
    },
    Provider {
        id: "openai-codex",
        label: "ChatGPT/Codex",
        kind: Kind::Oauth,
    },
    Provider {
        id: "claude-code",
        label: "Claude (Agent SDK)",
        kind: Kind::SetupToken,
    },
    Provider {
        id: "cursor",
        label: "Cursor",
        kind: Kind::Key,
    },
    Provider {
        id: "anthropic",
        label: "Anthropic API key",
        kind: Kind::Key,
    },
];

/// The provider with this id, or `None`.
pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// Every provider id, for an error message.
fn ids() -> String {
    PROVIDERS
        .iter()
        .map(|p| p.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// `zephyr.login_providers`: `{"providers":[{"id","label","kind"}]}`.
pub fn providers() -> Value {
    let list: Vec<Value> = PROVIDERS
        .iter()
        .map(|p| json!({"id": p.id, "label": p.label, "kind": p.kind.as_str()}))
        .collect();
    json!({"providers": list})
}

/// `<provider>-<n>` with the smallest free n, for a home whose account dirs it does not see
/// (`zephyr init` names accounts before any login ran). The daemon's own naming is
/// `next_name_in`, which also skips a name whose directory is already on disk and a name another
/// sign-in in this process holds.
pub fn next_name(doc: &toml_edit::DocumentMut, provider: &str) -> String {
    free_name(doc, provider, None)
}

/// `<provider>-<n>` with the smallest free n, skipping names the config already uses, names whose
/// `home/accounts/<name>` directory exists, and names a running login in this process still holds
/// (`Reserved`). A directory left behind by a login whose row was later dropped still holds that
/// provider's `auth.json`, so reusing the name would pass `rpi auth check` and report `done`
/// without anyone signing in — and skipping all of them keeps the old attempt's credential where
/// it is instead of clearing it. A function of the config doc, the home directory and the logins
/// this process has running.
pub fn next_name_in(doc: &toml_edit::DocumentMut, provider: &str, home: &Path) -> String {
    free_name(doc, provider, Some(home))
}

fn free_name(doc: &toml_edit::DocumentMut, provider: &str, home: Option<&Path>) -> String {
    let taken: Vec<String> = crate::host::conf::accounts(doc)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    // The names running logins hold in this home. Without a home (the host's `next_name`) the
    // answer stays what it is on disk and in the config: that process runs no logins.
    let held: Vec<String> = match home {
        Some(home) => names()
            .iter()
            .filter(|(at, _)| at == home)
            .map(|(_, name)| name.clone())
            .collect(),
        None => Vec::new(),
    };
    (1..)
        .map(|n| format!("{provider}-{n}"))
        .find(|n| {
            !taken.contains(n)
                && !held.contains(n)
                && !home.is_some_and(|home| home.join("accounts").join(n).exists())
        })
        .expect("a free name")
}

/// A started login: what `zephyr.login_start` answers.
#[derive(Debug, Clone)]
pub struct Start {
    pub login_id: String,
    pub provider: &'static str,
    pub account: String,
    /// Empty when rpi lists no model before the sign-in: several providers only list theirs
    /// once they are signed in, so the pump asks again on the account's own dir and only that
    /// answer can fail the login.
    pub model: String,
    pub kind: Kind,
}

impl Start {
    /// The `zephyr.login_start` result, exactly the fields the wire contract names.
    pub fn to_json(&self) -> Value {
        json!({
            "loginId": self.login_id,
            "provider": self.provider,
            "account": self.account,
            "kind": self.kind.as_str(),
        })
    }
}

/// How a login stands.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Phase {
    /// The child is running and has not finished; it may still print a URL.
    #[default]
    Waiting,
    /// The child takes a line on stdin (`zephyr.login_input`, `/login paste`): a key, or under
    /// the network floor the callback address of an OAuth sign-in.
    NeedInput,
    /// `rpi auth check` passed and the `[[account]]` row is going into `zephyr.toml`: set under
    /// the login's own lock, immediately before `record` writes. A cancel that lands during the
    /// write reads it and refuses, rather than answering "cancelled" over a row it cannot take
    /// back.
    Recording,
    /// `rpi auth check` said ready and the account is in `zephyr.toml`.
    Done,
    /// The child ended without a stored credential, or the account could not be recorded.
    Failed,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Waiting => "waiting",
            Phase::NeedInput => "need_input",
            Phase::Recording => "recording",
            Phase::Done => "done",
            Phase::Failed => "failed",
        }
    }

    /// Past the point a cancel or the idle deadline may drop it: it is writing (or has written)
    /// its row, or it is over. Whatever it waited for, nobody is left to answer.
    fn settled(self) -> bool {
        matches!(self, Phase::Recording | Phase::Done | Phase::Failed)
    }
}

/// How many output lines `zephyr.login_poll` reports, from the end.
const MAX_LINES: usize = 40;
/// Logins the registry holds at once. A running one is a live `rpi` under a pty, so the cap
/// covers those and not only the finished ones `registry_insert` can drop.
const MAX_LOGINS: usize = 16;
/// The longest line kept from the pty: a progress bar or a prompt redraw can go on for far
/// longer than anything a human reads.
const MAX_LINE: usize = 4 * 1024;
/// How long `rpi --list-models` gets before the login goes on without it.
const LIST_MODELS: Duration = Duration::from_secs(20);
/// The shortest run of a known secret masked on its own: a prompt echoes a key as it is typed
/// and a push can end mid-token (a redraw per `\r`, the `MAX_LINE` cap), so a fragment is as much
/// a leak as the whole token. Tokens are high-entropy, so eight characters of one is not a word;
/// shorter runs are, and masking them would hide real output (a typed `http` is no secret).
const GRAM_MIN: usize = 8;
/// The longest run masked on its own. Longer runs are still cut by the whole-secret replace, and
/// capping keeps the gram set of a long token to a few hundred entries.
const GRAM_MAX: usize = 16;
/// What a secret (or a span carrying part of one) becomes in the lines a caller reads.
pub(crate) const REDACTED: &str = "[redacted]";
/// The longest account name: it becomes a directory under `accounts/` and a `[[account]]` row
/// `zephyr status` prints, so a token pasted in its place is not a name.
const MAX_ACCOUNT_NAME: usize = 32;
/// The longest run of an account name that may carry no `-`/`_`: a pasted key is one unbroken
/// stretch of random characters, a name a person types is not (and the rule only applies past
/// this length, so `work-2` and `a` are names either way).
const NAME_RUN: usize = 20;
/// How long a sign-in may go without a word — no new pty output and nothing through `input` —
/// before the daemon drops it. A login nobody answers otherwise lives for the life of the daemon,
/// holding a `script`+`rpi` child and a registry place; `MAX_LOGINS` of those refuse every further
/// `/login` for good.
const IDLE: Duration = Duration::from_secs(10 * 60);

struct Inner {
    phase: Phase,
    lines: VecDeque<String>,
    /// The sign-in URL the child printed, if any.
    url: Option<String>,
    error: Option<String>,
    /// The account's model; empty until a listing names one.
    model: String,
    /// What callers sent in, kept only to redact the pty's echo from `lines`.
    secrets: Vec<String>,
    /// Lines queued for the child's stdin; `None` once it is gone.
    input: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    /// The `script` process, so a cancel can kill it (and the child under it).
    child: Option<tokio::process::Child>,
    /// The name this login holds (`Reserved`), kept here so the reservation is released in the
    /// same critical section that reports the outcome, whichever way the login ends.
    reserved: Option<Reserved>,
    /// When the sign-in last said anything: a pty line, or a line typed in. `IDLE` is measured
    /// from here.
    activity: Instant,
}

impl Default for Inner {
    fn default() -> Self {
        Inner {
            phase: Phase::default(),
            lines: VecDeque::new(),
            url: None,
            error: None,
            model: String::new(),
            secrets: Vec::new(),
            input: None,
            child: None,
            reserved: None,
            // Made once per login, so this is when the sign-in began.
            activity: Instant::now(),
        }
    }
}

struct Login {
    id: String,
    provider: &'static str,
    account: String,
    /// Insertion order, so the oldest finished login is the one dropped.
    seq: u64,
    inner: Mutex<Inner>,
}

impl Login {
    fn phase(&self) -> Phase {
        lock(&self.inner).phase
    }

    /// The model the account is for, once a listing named one.
    fn model(&self) -> String {
        lock(&self.inner).model.clone()
    }

    /// Appends a line of child output, redacted and capped, with its URL picked out.
    fn push_line(&self, line: &[u8]) {
        let mut inner = lock(&self.inner);
        // SGR/OSC escapes come out first: a terminal emphasises a key as it is typed
        // (`sk-ant-\e[7mKEY`), and a token broken by an escape is not a token any substring rule
        // can see. What is left is also what a human reads, and still carries the URL.
        let text = strip_ansi(&String::from_utf8_lossy(line));
        let text = plain(&redact(&text, &inner.secrets));
        // The pty's `\r\n` is a line and then an empty one: a blank push says nothing happened,
        // so it neither counts as output nor pushes a line out of `MAX_LINES`.
        if text.is_empty() {
            return;
        }
        if inner.url.is_none() {
            inner.url = url_of(&text);
        }
        inner.activity = Instant::now();
        inner.lines.push_back(text);
        while inner.lines.len() > MAX_LINES {
            inner.lines.pop_front();
        }
    }
}

/// The `zephyr.login_poll` result, or `None` for an id that is not (or no longer) a login.
pub fn poll(id: &str) -> Option<Value> {
    let login = get(id)?;
    let inner = lock(&login.inner);
    Some(json!({
        "phase": inner.phase.as_str(),
        "url": inner.url,
        "lines": inner.lines.iter().cloned().collect::<Vec<_>>(),
        "account": login.account,
        // `null` until rpi lists one: a login may be waiting for a model.
        "model": (!inner.model.is_empty()).then(|| inner.model.clone()),
        "error": inner.error,
    }))
}

/// The ids of the logins this process still holds, oldest first.
pub fn logins() -> Vec<String> {
    let mut all: Vec<(u64, String)> = registry().values().map(|l| (l.seq, l.id.clone())).collect();
    all.sort();
    all.into_iter().map(|(_, id)| id).collect()
}

/// Writes `text` and a newline to the login's child. `false` for an unknown id.
pub fn input(id: &str, text: &str) -> bool {
    let Some(login) = get(id) else {
        return false;
    };
    let text = text.trim().to_string();
    let mut inner = lock(&login.inner);
    // Only while the child takes one: a line typed before it is there (or after it is gone) is
    // not input, and must not be kept as a secret either.
    if inner.phase != Phase::NeedInput {
        return false;
    }
    // Never logged, never returned: this is only here to redact the pty's echo.
    inner.secrets.push(text.clone());
    while inner.secrets.len() > 8 {
        inner.secrets.remove(0);
    }
    // A key typed in is a sign of life: a sign-in waiting on it is not idle.
    inner.activity = Instant::now();
    match &inner.input {
        Some(tx) => tx.send(text).is_ok(),
        None => false,
    }
}

/// Kills the login's child and forgets it. `false` for an unknown id, and for a login that is
/// already `Done` or `Recording`: that one wrote (or is writing) its `[[account]]` row, and DESIGN
/// §4.3.1 says a login that had finished before the cancel is not cancelled — it stays `done` with
/// its account.
pub fn cancel(id: &str) -> bool {
    let Some(login) = get(id) else {
        return false;
    };
    {
        let mut inner = lock(&login.inner);
        // Read under the same lock the pump sets the phase in. The pump sets `Recording` in the
        // one critical section that then writes the row, so a cancel that lands while `record`
        // runs either sees `Recording` (and refuses) or `Failed` (and already won); it can never
        // report "cancelled" over a row the file keeps.
        if matches!(inner.phase, Phase::Done | Phase::Recording) {
            return false;
        }
        // Set before the entry leaves the registry. The pump re-reads the phase under this same
        // lock before it records the account, so a cancel that lands while `rpi auth check`
        // runs cannot be answered with "cancelled" and then add the row anyway.
        inner.phase = Phase::Failed;
        inner.error = Some("cancelled".into());
        kill(&mut inner);
    }
    registry().remove(id).is_some()
}

/// Kills the login's child and forgets it, its queued stdin and the name it held. Called with the
/// login's lock held, by `cancel` and by the idle deadline: the phase is already `Failed`, so what
/// is left is the process. A login with no child (still listing models, or already reaped) has
/// nothing to kill.
fn kill(inner: &mut Inner) {
    inner.input = None;
    inner.reserved.take();
    if let Some(mut child) = inner.child.take() {
        let _ = child.start_kill();
        // Reaped off the caller's thread: `cancel` runs on the attach loop, which must not wait on
        // a process. Without this `script` (and rpi under it) stays <defunct>. With no runtime to
        // spawn on (a test thread, a bare call) the kill still happens but nothing reaps:
        // `kill_on_drop` reaps `script`, and the `rpi` under it is left to init. That is the one
        // case a cancel cannot clean up fully.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = child.wait().await;
            });
        }
    }
}

/// Starts `rpi auth login --provider <p>` for a new or existing account, records the account in
/// `zephyr.toml` once rpi holds the credential, and answers at once with the ids to poll.
pub async fn start(
    cfg: &Config,
    provider_id: &str,
    account: Option<String>,
    model: Option<String>,
) -> anyhow::Result<Start> {
    let p = provider(provider_id)
        .with_context(|| format!("unknown provider {provider_id}: one of {}", ids()))?;
    // The account is recorded in the config file the router reloads.
    let Some(path) = cfg.path.clone() else {
        bail!("no zephyr.toml to record the account in")
    };
    let doc = crate::host::conf::load(&path)?;
    let account = match account {
        Some(name) => name.trim().to_string(),
        None => next_name_in(&doc, p.id, &cfg.home),
    };
    check_account(&account)?;
    // The name is taken before anything slow runs. Neither the config nor `accounts/<name>` says
    // a name is in use until after the model listing and `pool::prepare`, so a name resolved now
    // and registered later would be handed to a second start running alongside (`Reserved`).
    let reserved = reserve(&cfg.home, &account);
    let model = resolve_model(&doc, &account, p.id, model)?;
    // The slot is taken here, before the model listing and before the spawn, and held until the
    // pump has the login: `rpi --list-models` is a child of its own that can take seconds, so a
    // check made up front and an insert made after it would let N concurrent starts all pass the
    // check and spawn N children into a cap of `MAX_LOGINS`.
    let seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    let login = Arc::new(Login {
        id: format!("l{seq}"),
        provider: p.id,
        account,
        seq,
        // `waiting` whatever the kind: `need_input` is set once the child is there to take the
        // key (`inner.input`), so a poll taken while rpi lists models neither reports a prompt
        // `input` would refuse as an unknown login nor makes a client drop the key it just took.
        inner: Mutex::new(Inner {
            model: model.clone().unwrap_or_default(),
            ..Inner::default()
        }),
    });
    registry_insert(&login)?;
    // From here the entry is in the registry: any failure below drops it again, so a start that
    // could not spawn a child leaves no slot behind.
    let mut slot = Slot {
        id: login.id.clone(),
        dir: None,
        made: false,
        keep: false,
    };
    let model = match model {
        Some(model) => model,
        // rpi may not list a provider's models before it is signed in: the pump asks again on
        // the account's own dir, and fails the login only if that answer is empty too.
        None => {
            let listed = first_model(cfg, p.id, &crate::auth_keeper::agent_dir(cfg))
                .await
                .unwrap_or_default();
            lock(&login.inner).model = listed.clone();
            listed
        }
    };
    // The dir must exist before rpi writes `auth.json` into it. `prepare` also takes one that is
    // already there (a re-login of a named account), so note whether this start made it: only a
    // directory this start created is removed again when the start fails.
    let account = Account {
        name: login.account.clone(),
        model: model.clone(),
    };
    let made = {
        let _fs = crate::agent::fs();
        !cfg.account_dir(&account).exists()
    };
    // Remembered before `prepare`, so a directory it made and then failed on is removed again too.
    slot.dir = Some(cfg.account_dir(&account));
    slot.made = made;
    let dir = crate::pool::prepare(cfg, &account)?;
    slot.dir = Some(dir.clone());
    let mut child = spawn(cfg, p.id, &dir)?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let (input, typed) = tokio::sync::mpsc::unbounded_channel();
    feed(stdin, typed);
    {
        let mut inner = lock(&login.inner);
        inner.input = Some(input);
        // Now that there is a child to write to: a key provider is ready for the token from
        // here on. An OAuth one stays `waiting` for the browser, except under the network floor
        // (DESIGN §4.20), where the redirect to 127.0.0.1 never gets to the container and the
        // owner pastes its address instead. `input` refuses a line in any other phase.
        if p.kind != Kind::Oauth || cfg.floor {
            inner.phase = Phase::NeedInput;
        }
        inner.child = Some(child);
        // From here the login releases the name when its phase turns terminal.
        inner.reserved = Some(reserved);
    }
    // The pump owns the login from here on: killing it is its business now, not the slot's.
    slot.keep = true;
    pump(cfg.clone(), &login, stdout, stderr, dir);
    Ok(Start {
        login_id: login.id.clone(),
        provider: p.id,
        account: login.account.clone(),
        model: login.model(),
        kind: p.kind,
    })
}

/// The registry slot `start` takes before it lists a model or spawns a child. Dropped — the entry
/// taken out again, so a start that failed at the listing, the account dir or the pty does not
/// leak a place under `MAX_LOGINS` — and, for a start that did not get as far as `pump`, the
/// account directory it made on the way: nothing in `zephyr.toml` names an account `start` failed
/// to record, so that directory is unreachable, and leaving it would keep `next_name` off that
/// name for good.
struct Slot {
    id: String,
    /// The account directory `start` prepared, once it has one.
    dir: Option<PathBuf>,
    /// Whether this start created `dir` (a re-login of a named account keeps the one it found).
    made: bool,
    keep: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.keep {
            return;
        }
        registry().remove(&self.id);
        if let (true, Some(dir)) = (self.made, self.dir.as_deref()) {
            // Best effort: the start already fails with its own error, and a directory made under
            // the agent's uid has to go as that uid.
            let _fs = crate::agent::fs();
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Reads the child's output into the login, then checks and records the credential.
fn pump(
    cfg: Config,
    login: &Arc<Login>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    dir: PathBuf,
) {
    let mut readers = Vec::new();
    if let Some(out) = stdout {
        readers.push(tokio::spawn(read(login.clone(), BufReader::new(out))));
    }
    if let Some(err) = stderr {
        readers.push(tokio::spawn(read(login.clone(), BufReader::new(err))));
    }
    let login = login.clone();
    tokio::spawn(async move {
        // The deadline runs beside the readers, not as another tick in the router: a login nobody
        // answers is dropped whether or not anyone ever polls it. It ends by itself once the
        // phase is settled, which every return below does.
        let _deadline = tokio::spawn(deadline(cfg.clone(), login.clone(), dir.clone(), IDLE));
        for reader in readers {
            let _ = reader.await;
        }
        // The readers saw EOF, so the child has ended: reap it, or `script` and the rpi under it
        // stay <defunct> until the registry evicts this entry. Off the lock, so a cancel can
        // still kill and take it.
        let child = lock(&login.inner).child.take();
        let status = match child {
            Some(mut child) => child.wait().await.ok(),
            // A cancel took it and already ended the login; there is no status to read here.
            None => None,
        };
        {
            // The child is gone: nothing more can be typed into it, and the deadline has nothing
            // left to watch.
            let mut inner = lock(&login.inner);
            inner.input = None;
            inner.activity = Instant::now();
        }
        if login.phase() == Phase::Failed {
            discard(&cfg, &login.account, &dir); // cancelled: no row, so nothing keeps the dir
            return;
        }
        // `script -qec` hands the command's exit status through, so a non-zero one says it did not
        // store a credential. A login has to pass both this and `rpi auth check` below; a status
        // that already failed is not asked again into a pass, and what the child printed stays out
        // of the message (only the status is reported).
        if let Some(note) = exit_note(status) {
            fail(&cfg, &login, &dir, note);
            return;
        }
        if !ready(&cfg, &dir, login.provider).await {
            fail(
                &cfg,
                &login,
                &dir,
                format!(
                    "rpi stored no credential for {}; see the lines above",
                    login.provider
                ),
            );
            return;
        }
        // A provider that only lists its models once signed in answers here, as the account
        // rpi now runs as. Nothing given and nothing listed either way: nothing to record.
        let model = match login.model() {
            model if !model.is_empty() => model,
            _ => match first_model(&cfg, login.provider, &dir).await {
                Some(model) => model,
                None => {
                    fail(
                        &cfg,
                        &login,
                        &dir,
                        format!(
                            "rpi lists no {} model; give one as PROVIDER/MODEL",
                            login.provider
                        ),
                    );
                    return;
                }
            },
        };
        // Done only once the account is in the file the router reloads. The phase is read in the
        // same critical section that writes it, so a cancel cannot land between the check and
        // the row: whatever the TUI was told, the file says the same. `Recording` is what makes
        // that true while `record` itself runs — a cancel that gets the lock then refuses, instead
        // of reporting a cancellation over a row that is already on disk.
        let mut inner = lock(&login.inner);
        if inner.phase == Phase::Failed {
            // Cancelled while `ready` or the model listing ran: the credential rpi may already
            // have written is the cancelled attempt's, and the config does not name it.
            drop(inner);
            discard(&cfg, &login.account, &dir);
            return;
        }
        inner.model = model.clone();
        inner.phase = Phase::Recording;
        let recorded = match cfg.path.as_deref() {
            Some(path) => record(path, &login.account, &model),
            None => Err(anyhow::anyhow!("no zephyr.toml to record the account in")),
        };
        match recorded {
            Ok(()) => {
                inner.phase = Phase::Done;
                inner.reserved.take();
            }
            Err(e) => {
                inner.phase = Phase::Failed;
                inner.error = Some(format!("{e:#}"));
                inner.reserved.take();
                // A row was not written, so nothing names the directory: DESIGN §4.7.1's "a
                // sign-in that ends without a row also drops the directory it made".
                drop(inner);
                discard(&cfg, &login.account, &dir);
            }
        }
    });
}

/// Marks the login failed with `error`; what the child printed above it is for the human.
fn fail(cfg: &Config, login: &Arc<Login>, dir: &Path, error: String) {
    {
        let mut inner = lock(&login.inner);
        inner.phase = Phase::Failed;
        inner.error = Some(error);
        // The name is free again in the same critical section that reports the failure, so a
        // caller that sees `failed` can start the same account straight away.
        inner.reserved.take();
    }
    discard(cfg, &login.account, dir);
}

/// Fails the login nobody answered, the way a cancel would: kill and reap the child, drop the
/// directory, and keep the registry entry as `Failed` so `login_poll` (and the notice the router
/// posts from it) says why instead of the login simply vanishing. `idle` is `IDLE` in the daemon
/// and a shorter one in tests.
async fn deadline(cfg: Config, login: Arc<Login>, dir: PathBuf, idle: Duration) {
    loop {
        let left = {
            let inner = lock(&login.inner);
            if inner.phase.settled() {
                return;
            }
            idle.saturating_sub(inner.activity.elapsed())
        };
        tokio::time::sleep(left).await;
        let mut inner = lock(&login.inner);
        if inner.phase.settled() {
            return;
        }
        // Something came in while this was going round again, or the phase was about to move:
        // either way the wait starts over from the latest activity.
        if inner.activity.elapsed() < idle {
            continue;
        }
        inner.phase = Phase::Failed;
        inner.error = Some(format!(
            "no answer for {} minutes; the sign-in was dropped",
            idle.as_secs() / 60
        ));
        kill(&mut inner);
        drop(inner);
        discard(&cfg, &login.account, &dir);
        return;
    }
}

/// Drops the account directory a sign-in left behind when `zephyr.toml` names no account for it.
///
/// rpi writes `auth.json` before this process decides whether the attempt counts: a sign-in that
/// fails on the model listing, or is cancelled after the key went in, would otherwise leave a
/// credential that makes `rpi auth check` pass for that name later — the next `/login` picks a
/// fresh name, so the directory is dead weight in the pool, and a name given explicitly would be
/// "signed in" without anyone signing in. A row means a working account (a re-login), so nothing
/// is touched then. Best effort: a failure here costs a stale directory, not the answer.
fn discard(cfg: &Config, account: &str, dir: &Path) {
    let Some(path) = cfg.path.as_deref() else {
        return;
    };
    let named = crate::host::conf::load(path)
        .map(|doc| {
            crate::host::conf::accounts(&doc)
                .iter()
                .any(|(name, _)| name == account)
        })
        .unwrap_or(true); // cannot tell: keep it
    if !named {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// One reader for a child stream: a line at a time, into the login. A line ends at a newline or
/// a carriage return (a progress bar or a prompt redraw is one line after another), and nothing
/// longer than `MAX_LINE` is buffered: a child that never writes a break must not grow the
/// buffer, and the overflow goes out as its own capped entry.
async fn read<R: tokio::io::AsyncBufRead + Unpin>(login: Arc<Login>, mut lines: R) {
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = match lines.fill_buf().await {
            Ok(chunk) => chunk,
            Err(_) => return,
        };
        if chunk.is_empty() {
            // End of the stream mid-line: what is left is the last line.
            if !buf.is_empty() {
                login.push_line(&buf);
            }
            return;
        }
        let break_at = chunk.iter().position(|b| *b == b'\n' || *b == b'\r');
        match break_at {
            Some(at) => {
                // Never more than `MAX_LINE` into `buf`: the cap is checked before the break, not
                // after, so a long line with a break far away is cut like one without.
                let take = at.min(MAX_LINE.saturating_sub(buf.len()));
                buf.extend_from_slice(&chunk[..take]);
                login.push_line(&buf);
                buf.clear();
                if take == at {
                    lines.consume(at + 1); // past the break
                } else {
                    lines.consume(take); // the rest of the line comes round again
                }
            }
            None => {
                let take = chunk.len().min(MAX_LINE - buf.len());
                buf.extend_from_slice(&chunk[..take]);
                lines.consume(take);
                if buf.len() >= MAX_LINE {
                    login.push_line(&buf);
                    buf.clear();
                }
            }
        }
    }
}

/// `script -qec '<rpi> auth login --provider <p> [--extension ..]' /dev/null` on `dir`.
fn spawn(cfg: &Config, provider: &str, dir: &Path) -> anyhow::Result<tokio::process::Child> {
    let mut shell = format!(
        "{} auth login --provider {}",
        quote(&cfg.rpi.display().to_string()),
        quote(provider)
    );
    for ext in cfg.policy_plugin.iter().chain(cfg.extensions.iter()) {
        shell.push_str(&format!(
            " --extension {}",
            quote(&ext.display().to_string())
        ));
    }
    // rpi reads keys from a terminal, so `script` gives it one; the pty's own output is what
    // `login_poll` reports. `-e` hands the command's exit status through — with `-f` it is
    // `script`'s own, which is always 0 — so the pump can tell a sign-in that ended badly from one
    // that finished (see `exit_note`).
    let mut cmd = tokio::process::Command::new(SCRIPT);
    cmd.arg("-qec")
        .arg(&shell)
        .arg("/dev/null")
        .current_dir(&cfg.home)
        .env("RPI_CODING_AGENT_DIR", dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::agent::cmd(cmd.as_std_mut());
    // A login dies with the daemon (or test) that started it: its registry entry, which
    // `kill_on_drop` hangs on, is never dropped when that process exits or is killed. Killing
    // `script` hangs up the pty, which ends the rpi under it. The signal comes when the thread
    // that spawned it ends, which for the daemon's runtime workers is the process's end.
    let parent = std::process::id() as libc::pid_t;
    // SAFETY: prctl and getppid are async-signal-safe, and run after `agent::cmd`'s setuid,
    // which would clear the setting.
    unsafe {
        cmd.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            if libc::getppid() != parent {
                return Err(std::io::Error::other(
                    "the daemon ended before the login started",
                ));
            }
            Ok(())
        });
    }
    cmd.spawn()
        .with_context(|| format!("running rpi auth login --provider {provider} in a pty"))
}

/// Writes the lines `input` queues into the child's stdin, one per line, until it is gone.
fn feed(
    stdin: Option<tokio::process::ChildStdin>,
    mut typed: tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let Some(mut stdin) = stdin else {
        return;
    };
    tokio::spawn(async move {
        while let Some(text) = typed.recv().await {
            if stdin
                .write_all(format!("{text}\n").as_bytes())
                .await
                .is_err()
            {
                return;
            }
            if stdin.flush().await.is_err() {
                return;
            }
        }
    });
}

/// Why a login's child ended without a credential to show for it, when its status says so.
///
/// Only the status is reported: what the child printed is already in the login's lines (redacted),
/// and an error message travels further — into a chat notice and an attach reply — so nothing the
/// child wrote is repeated in it.
fn exit_note(status: Option<std::process::ExitStatus>) -> Option<String> {
    match status {
        Some(status) if status.success() => None,
        Some(status) => Some(format!(
            "rpi stored no credential: rpi auth login ended with {status}; see the lines above"
        )),
        // No status is no evidence: `rpi auth check` decides.
        None => None,
    }
}

/// The model for the account: the one given, else the one already recorded for that account on
/// this provider. `None` when it has to come from what rpi lists. A model is only taken when its
/// provider prefix is the login's — `/login opencode-go a` with `anthropic/claude` must not record
/// a model the new credential cannot serve, and that holds for a model given as much as for a
/// stored one.
fn resolve_model(
    doc: &toml_edit::DocumentMut,
    account: &str,
    provider: &str,
    model: Option<String>,
) -> anyhow::Result<Option<String>> {
    if let Some(model) = model {
        let model = model.trim().to_string();
        let Some((prefix, _)) = model.split_once('/') else {
            bail!("model {model:?}: want PROVIDER/MODEL");
        };
        if prefix != provider {
            bail!(
                "model {model:?} is not a {provider} model: this login is for {provider}, so the \
                 model has to start with {provider}/"
            );
        }
        return Ok(Some(model));
    }
    Ok(crate::host::conf::accounts(doc)
        .into_iter()
        .find(|(name, _)| name == account)
        .map(|(_, model)| model)
        .filter(|model| model.split('/').next() == Some(provider)))
}

/// The first `provider/model` row rpi lists for `provider` on `dir`, if it lists one.
async fn first_model(cfg: &Config, provider: &str, dir: &Path) -> Option<String> {
    list_models(cfg, provider, dir).await.into_iter().next()
}

/// Models `rpi --list-models` lists for `provider` on the agent dir `dir`, empty when rpi could
/// not say inside `LIST_MODELS`. A wedged rpi must not hold the login (or the daemon) all day, so
/// the listing is async with a timeout, and a timeout reads as "listed nothing".
async fn list_models(cfg: &Config, provider: &str, dir: &Path) -> Vec<String> {
    let mut cmd = tokio::process::Command::new(&cfg.rpi);
    crate::agent::cmd(cmd.as_std_mut());
    cmd.arg("--list-models")
        .current_dir(&cfg.home)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .env("RPI_CODING_AGENT_DIR", dir)
        .kill_on_drop(true);
    for ext in cfg.policy_plugin.iter().chain(cfg.extensions.iter()) {
        cmd.arg("--extension").arg(ext);
    }
    let Ok(Ok(out)) = tokio::time::timeout(LIST_MODELS, cmd.output()).await else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let (p, m) = (cols.next()?, cols.next()?);
            (p == provider).then(|| format!("{p}/{m}"))
        })
        .collect()
}

/// The account name a login may take: `[a-z0-9][a-z0-9_-]*`, at most `MAX_ACCOUNT_NAME` and at
/// most `NAME_RUN` characters with no `-`/`_`. It is a directory under `accounts/` and a row in
/// `zephyr.toml`, so a token typed where the name goes is refused rather than recorded and printed
/// by `zephyr status`. The message never repeats what was given: the value is very likely a
/// pasted credential, and the error goes on to a chat notice and an attach reply.
pub(crate) fn check_account(name: &str) -> anyhow::Result<()> {
    let mut chars = name.chars();
    let first = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    let run = name
        .split(['-', '_'])
        .map(str::len)
        .max()
        .unwrap_or_default();
    if first && rest && name.len() <= MAX_ACCOUNT_NAME && run <= NAME_RUN {
        return Ok(());
    }
    bail!(
        "account name: want [a-z0-9][a-z0-9_-]*, at most {MAX_ACCOUNT_NAME} characters, and a \
         token is not one"
    )
}

/// Whether `rpi auth check --json --provider <p>` says the credential is stored.
async fn ready(cfg: &Config, dir: &Path, provider: &str) -> bool {
    let mut cmd = tokio::process::Command::new(&cfg.rpi);
    crate::agent::cmd(cmd.as_std_mut());
    cmd.args(["auth", "check", "--json", "--provider", provider])
        .current_dir(&cfg.home)
        .env("RPI_CODING_AGENT_DIR", dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = match cmd.output().await {
        Ok(out) if out.status.success() => out,
        // A non-zero exit says what is missing on stderr; it never carries the credential.
        Ok(_) | Err(_) => return false,
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .any(|v| v["ready"] == true)
}

/// Adds the account to `zephyr.toml` (`toml_edit` keeps the comments); the router reloads it.
/// Through `conf::update`, so a `/model` writing the same file at the same time cannot drop the
/// row the login just reported.
///
/// A document whose `account` is not an array of tables is refused here rather than allowed to
/// reach `conf::upsert_account`, whose `expect` would panic — in a spawned task, with the login's
/// lock held and the phase `Recording`, which no cancel can then take back out.
fn record(path: &Path, account: &str, model: &str) -> anyhow::Result<()> {
    crate::host::conf::update(path, |doc| {
        if doc
            .get("account")
            .is_some_and(|a| a.as_array_of_tables().is_none())
        {
            bail!(
                "zephyr.toml has an `account` that is not a list of [[account]] tables; fix it \
                 and try the sign-in again"
            );
        }
        crate::host::conf::upsert_account(doc, account, model);
        Ok(())
    })
}

static SEQ: AtomicU64 = AtomicU64::new(0);
static REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Login>>>> = OnceLock::new();
/// The account names running logins hold, per home (see `Reserved`).
static NAMES: OnceLock<Mutex<HashSet<(PathBuf, String)>>> = OnceLock::new();

fn registry() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Login>>> {
    lock(REGISTRY.get_or_init(Default::default))
}

fn get(id: &str) -> Option<Arc<Login>> {
    registry().get(id).cloned()
}

fn registry_insert(login: &Arc<Login>) -> anyhow::Result<()> {
    let mut map = registry();
    // Under this one lock, from the check to the insert: that is what makes the cap hold when
    // several starts run at once. Finished logins make room; a running one is a live `rpi` under
    // a pty, so the rest is a clear refusal rather than an eviction of someone's sign-in.
    if map.len() >= MAX_LOGINS {
        evict_finished(&mut map);
    }
    if map.len() >= MAX_LOGINS {
        bail!(
            "{MAX_LOGINS} sign-ins are running; finish or cancel one (zephyr.login_cancel) first"
        );
    }
    map.insert(login.id.clone(), login.clone());
    Ok(())
}

/// Drops finished logins, oldest first, until one fits.
fn evict_finished(map: &mut HashMap<String, Arc<Login>>) {
    let mut finished: Vec<(u64, String)> = map
        .values()
        .filter(|l| matches!(l.phase(), Phase::Done | Phase::Failed))
        .map(|l| (l.seq, l.id.clone()))
        .collect();
    finished.sort();
    for (_, id) in finished {
        map.remove(&id);
        if map.len() < MAX_LOGINS {
            break;
        }
    }
}

/// The names running logins hold, so `free_name` skips a name another sign-in is still using.
fn names() -> std::sync::MutexGuard<'static, HashSet<(PathBuf, String)>> {
    lock(NAMES.get_or_init(Default::default))
}

/// The account name a starting login holds.
///
/// Nothing on disk says a generated name is taken until its `accounts/<name>` directory exists or
/// its `[[account]]` row is written, and neither happens until after the model listing and
/// `pool::prepare`: without this set two starts running at once — `/login antigravity` twice, or
/// the TUI and a chat — both resolve `antigravity-1`, both write the same `auth.json`, and a
/// cancel of one discards the directory under the other's live child. So `start` takes the name
/// before the listing and gives it back when the login's phase turns terminal (a finish, a
/// failure, a cancel and the idle deadline all release it in the critical section that reports
/// the outcome), or when `start` itself returns early. Per home: `accounts/<name>` belongs to one
/// home, so two homes may hold one name at the same time.
struct Reserved {
    /// Set only for the login that took the name: a second start given the same name explicitly
    /// joins the reservation instead of taking its release away.
    held: bool,
    key: (PathBuf, String),
}

impl Drop for Reserved {
    fn drop(&mut self) {
        if self.held {
            names().remove(&self.key);
        }
    }
}

fn reserve(home: &Path, name: &str) -> Reserved {
    let key = (home.to_path_buf(), name.to_string());
    Reserved {
        held: names().insert(key.clone()),
        key,
    }
}

/// A line with control characters taken out, keeping `\t` (rpi's tables), and capped at
/// `MAX_LINE` so one entry cannot carry a megabyte into `MAX_LINES` entries' worth of memory.
fn plain(line: &str) -> String {
    let clean: String = line
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect();
    let clean = clean.trim();
    if clean.len() <= MAX_LINE {
        return clean.to_string();
    }
    format!("{}…", &clean[..clean.floor_char_boundary(MAX_LINE)])
}

/// A line of child output with every known secret, and every long run of one, taken out. Both the
/// whole token and part of it can reach a login's lines: a terminal redraws `> sk-ant-…` as the
/// characters are typed, a push ends at a `\r` or at the `MAX_LINE` cap, so a paste of a token is
/// several pushes and the verbatim replace only ever sees a piece of it. So every run of
/// `GRAM_MIN..=GRAM_MAX` characters of each secret, at every offset, is masked as well.
///
/// Only the matched span is replaced, never the line: the pty's output is what carries the sign-in
/// URL, and a short string that happens to be a prefix of a secret (a typed `http`) would otherwise
/// hide the very line the user is waiting for. The run set is bounded — a few hundred characters
/// of secret make a few hundred grams, and there are at most a handful of secrets.
pub(crate) fn redact(line: &str, secrets: &[String]) -> String {
    let mut text = line.to_string();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        text = text.replace(secret.as_str(), REDACTED);
    }
    let grams = grams(secrets);
    if grams.is_empty() {
        return text;
    }
    let bounds = bounds(&text);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at + 1 < bounds.len() {
        // The longest run starting here wins, so one gram and its shorter head are one marker.
        let hit = (GRAM_MIN..=GRAM_MAX).rev().find(|len| {
            at + len < bounds.len() && grams.contains(&text[bounds[at]..bounds[at + len]])
        });
        match hit {
            Some(len) => {
                out.push_str(REDACTED);
                at += len;
            }
            None => {
                out.push_str(&text[bounds[at]..bounds[at + 1]]);
                at += 1;
            }
        }
    }
    out
}

/// Every `GRAM_MIN..=GRAM_MAX`-character run of every secret, at every offset.
fn grams(secrets: &[String]) -> std::collections::HashSet<&str> {
    let mut set = std::collections::HashSet::new();
    for secret in secrets.iter().filter(|s| !s.is_empty()) {
        let bounds = bounds(secret);
        for at in 0..bounds.len() {
            for len in GRAM_MIN..=GRAM_MAX {
                if at + len < bounds.len() {
                    set.insert(&secret[bounds[at]..bounds[at + len]]);
                }
            }
        }
    }
    set
}

/// The byte offsets of `text`'s character boundaries, with `text.len()` last, so a run of `n`
/// characters starting at character `i` is `text[bounds[i]..bounds[i + n]]`.
fn bounds(text: &str) -> Vec<usize> {
    text.char_indices()
        .map(|(at, _)| at)
        .chain(std::iter::once(text.len()))
        .collect()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// `'…'`, with an embedded `'` as `'\''`, for the one string `/bin/sh` parses.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The first `http(s)://` URL in a line, without trailing punctuation.
fn url_of(line: &str) -> Option<String> {
    for scheme in ["https://", "http://"] {
        let Some(at) = line.find(scheme) else {
            continue;
        };
        let rest = &line[at..];
        let end = rest
            .find(|c: char| c.is_whitespace() || "\"'<>()`".contains(c))
            .unwrap_or(rest.len());
        let url = rest[..end].trim_end_matches(['.', ',', ';', ':']);
        if url.len() > scheme.len() {
            return Some(url.to_string());
        }
    }
    None
}

/// Drops the SGR/OSC escapes a terminal client would strip anyway, so a URL is found whole.
fn strip_ansi(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI: ESC [ … final byte in @..~
            Some('[') => {
                for c in chars.by_ref() {
                    if ('@'..='~').contains(&c) {
                        break;
                    }
                }
            }
            // OSC: ESC ] … BEL or ST
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\u{7}' {
                        break;
                    }
                    if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn providers_carry_the_kind_their_login_needs() {
        let kinds: Vec<(&str, &str)> = PROVIDERS.iter().map(|p| (p.id, p.kind.as_str())).collect();
        assert_eq!(
            kinds,
            [
                ("opencode-go", "key"),
                ("antigravity", "oauth"),
                ("openai-codex", "oauth"),
                ("claude-code", "setup_token"),
                ("cursor", "key"),
                ("anthropic", "key"),
            ]
        );
        assert!(provider("nope").is_none());
        let listed = providers();
        assert_eq!(listed["providers"][0]["label"], "OpenCode Go");
    }

    #[test]
    fn a_url_is_found_whole_and_its_trailing_punctuation_is_dropped() {
        assert_eq!(
            url_of("open this: https://example.test/auth?c=1 to sign in"),
            Some("https://example.test/auth?c=1".into())
        );
        assert_eq!(
            url_of("(see http://127.0.0.1:1455/x)."),
            Some("http://127.0.0.1:1455/x".into())
        );
        assert_eq!(url_of("no link here"), None);
        // An SGR-wrapped URL is found once the escapes are gone.
        assert_eq!(
            url_of(&strip_ansi("\u{1b}[36mhttps://a.test/b\u{1b}[0m")),
            Some("https://a.test/b".into())
        );
    }

    #[test]
    fn a_redrawn_or_escape_split_token_does_not_reach_the_lines() {
        let secrets = vec!["sk-ant-api03-SECRET".to_string()];
        // A prompt that redraws itself as the token is typed: each push ends the line, so the
        // verbatim replace cannot see the token, but a run of it is masked where it stands.
        assert_eq!(redact("sk-ant-api0", &secrets), REDACTED);
        // SGR emphasis inside the token is gone before `redact` runs (see `push_line`).
        assert_eq!(
            redact(
                &strip_ansi("sk-ant-\u{1b}[7mapi03-SECRET\u{1b}[0m"),
                &secrets
            ),
            REDACTED
        );
        // The verbatim token, surrounded by real output, keeps the output.
        assert_eq!(
            redact("key: sk-ant-api03-SECRET ok", &secrets),
            "key: [redacted] ok"
        );
        // Nothing resembling the token is left alone.
        assert_eq!(
            redact("Sign in to Antigravity", &secrets),
            "Sign in to Antigravity"
        );
    }

    #[test]
    fn the_tail_of_a_token_split_across_two_pushes_is_masked() {
        // The key a login is typed at is pushed a piece at a time: the head of it ends one line
        // and the rest starts the next, so no push ever holds the whole token.
        let secrets = vec!["sk-test-4f0a9c-DEADBEEF".to_string()];
        assert_eq!(redact("> sk-test-4f0a", &secrets), "> [redacted]");
        assert_eq!(redact("9c-DEADBEEF", &secrets), REDACTED);
        // A tail shorter than a gram is a word, not a leak: eight characters is where a run of a
        // high-entropy token stops being something real output prints by accident.
        assert_eq!(redact("\r> de", &secrets), "\r> de");
    }

    #[test]
    fn only_the_matched_span_goes_so_the_sign_in_url_survives() {
        let secrets = vec!["sk-ant-api03-SECRET".to_string()];
        let url = "open https://example.test/login/antigravity?c=1 to sign in";
        assert_eq!(redact(url, &secrets), url);
        // A token on the same line does not take the URL with it.
        let both = redact(
            "visit https://example.test/auth?c=1 and paste sk-ant-api03-SECRET",
            &secrets,
        );
        assert_eq!(
            both,
            "visit https://example.test/auth?c=1 and paste [redacted]"
        );
        assert_eq!(url_of(&both), Some("https://example.test/auth?c=1".into()));
        // A short string typed at the prompt — the head of a secret, as a pty first echoes it —
        // masks nothing at all: it cannot hide the line the URL is on.
        assert_eq!(redact(url, &["sk-ant".to_string()]), url);
    }

    #[test]
    fn a_login_that_wrote_its_row_is_not_cancelled() {
        let at = |id: &str, phase| {
            let login = Arc::new(Login {
                id: id.into(),
                provider: "anthropic",
                account: "work".into(),
                seq: 0,
                inner: Mutex::new(Inner {
                    phase,
                    ..Inner::default()
                }),
            });
            registry_insert(&login).unwrap();
            login
        };
        // `Done`: the `[[account]]` row is in `zephyr.toml`, so the login stays done and stays.
        let done = at("l-test-done", Phase::Done);
        assert!(!cancel(&done.id));
        assert_eq!(poll(&done.id).unwrap()["phase"], "done");
        // `Recording`: the row is being written under the same lock, so a cancel here cannot
        // honour a cancellation either — it refuses and lets the pump finish.
        let recording = at("l-test-recording", Phase::Recording);
        assert!(!cancel(&recording.id));
        assert_eq!(poll(&recording.id).unwrap()["phase"], "recording");
        // A running one is cancelled, as before.
        let waiting = at("l-test-waiting", Phase::Waiting);
        assert!(cancel(&waiting.id));
        registry().remove(&done.id);
        registry().remove(&recording.id);
    }

    #[test]
    fn a_name_whose_account_dir_already_exists_is_skipped() {
        let root = std::env::temp_dir().join(format!(
            "zephyr-auth-name-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        std::fs::create_dir_all(root.join("accounts/opencode-go-1")).unwrap();
        let doc: toml_edit::DocumentMut =
            "[[account]]\nname = \"opencode-go-2\"\nmodel = \"opencode-go/alpha\"\n"
                .parse()
                .unwrap();
        // 1 is a dir left behind by a dropped row (it still holds an `auth.json`), 2 is in the
        // config: a fresh sign-in takes 3 rather than reporting `done` on someone else's key.
        assert_eq!(next_name_in(&doc, "opencode-go", &root), "opencode-go-3");
        // The config-only naming the host uses does not look at a home.
        assert_eq!(next_name(&doc, "opencode-go"), "opencode-go-1");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_name_another_login_holds_is_skipped_until_it_lets_go() {
        let root = std::env::temp_dir().join(format!(
            "zephyr-auth-hold-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let doc: toml_edit::DocumentMut = "".parse().unwrap();
        let first = next_name_in(&doc, "antigravity", &root);
        assert_eq!(first, "antigravity-1");
        let held = reserve(&root, &first);
        // Two starts running at once: nothing is on disk or in the config to tell them apart, so
        // the second must not resolve the name the first one holds.
        assert_eq!(next_name_in(&doc, "antigravity", &root), "antigravity-2");
        drop(held);
        // Released, the name is free again — and a second home has its own `accounts/`, so it is
        // never in the way there.
        assert_eq!(next_name_in(&doc, "antigravity", &root), "antigravity-1");
        let elsewhere = root.join("other");
        let held = reserve(&root, "antigravity-1");
        assert_eq!(
            next_name_in(&doc, "antigravity", &elsewhere),
            "antigravity-1"
        );
        drop(held);
        std::fs::remove_dir_all(&root).ok();
    }

    /// A sign-in nobody answers holds a `script`+`rpi` child and a registry place for as long as
    /// the daemon lives; `MAX_LOGINS` of those refuse every further `/login`. The deadline drops it
    /// the way a cancel would, and gives the name back. `idle` keeps this from taking ten minutes.
    #[tokio::test]
    async fn a_login_nobody_answers_is_dropped_and_lets_the_name_go() {
        let root = std::env::temp_dir().join(format!(
            "zephyr-auth-idle-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let path = root.join("zephyr.toml");
        std::fs::write(&path, format!("rpi = \"/bin/true\"\nhome = {home:?}\n")).unwrap();
        let cfg = Config::load(&path).unwrap();
        let dir = cfg.account_dir(&Account {
            name: "antigravity-1".into(),
            model: String::new(),
        });
        std::fs::create_dir_all(&dir).unwrap();
        let id = format!("l-idle-{}", crate::unix_millis());
        let login = Arc::new(Login {
            id: id.clone(),
            provider: "antigravity",
            account: "antigravity-1".into(),
            seq: 0,
            inner: Mutex::new(Inner {
                phase: Phase::Waiting,
                reserved: Some(reserve(&home, "antigravity-1")),
                ..Inner::default()
            }),
        });
        registry_insert(&login).unwrap();
        let idle = Duration::from_millis(500);
        let task = tokio::spawn(deadline(cfg.clone(), login.clone(), dir.clone(), idle));
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Life from the child (or from a key typed in) starts the clock over.
        login.push_line(b"still here\r\n");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(login.phase(), Phase::Waiting);
        // Then silence: the login is failed with the reason a caller can read, the directory is
        // gone, and the name the login held is free again.
        for _ in 0..100 {
            if login.phase() != Phase::Waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let answer = poll(&id).expect("the failed login is still pollable");
        assert_eq!(answer["phase"], "failed", "{answer}");
        let error = answer["error"].as_str().unwrap_or_default();
        assert!(error.contains("the sign-in was dropped"), "{error}");
        assert!(!dir.exists(), "the dropped login kept its account dir");
        let doc: toml_edit::DocumentMut = "".parse().unwrap();
        assert_eq!(next_name_in(&doc, "antigravity", &home), "antigravity-1");
        task.abort();
        registry().remove(&id);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_document_whose_account_is_not_a_list_of_tables_is_an_error() {
        // `conf::upsert_account` panics on this shape. Recording happens in a spawned task with
        // the login's lock held and the phase `Recording`, where a panic leaves a sign-in no
        // cancel can take back, so the shape is checked first and reported as an error.
        let root = std::env::temp_dir().join(format!(
            "zephyr-auth-record-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("zephyr.toml");
        std::fs::write(&path, "account = \"x\"\n").unwrap();
        let e = record(&path, "work-2", "opencode-go/alpha").unwrap_err();
        assert!(
            e.to_string().contains("not a list of [[account]] tables"),
            "{e}"
        );
        // The file is left as it was: a failed sign-in writes nothing.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "account = \"x\"\n");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_account_name_is_not_a_token() {
        for ok in ["opencode-go", "a", "work_2", "0x", "work-2"]
            .iter()
            .copied()
        {
            assert!(check_account(ok).is_ok(), "{ok}");
            // A name of a few dashes or underscores has no long unbroken run either.
            assert!(check_account(&format!("work-{ok}")).is_ok(), "work-{ok}");
        }
        for bad in [
            "sk-ant-api03-SECRET",
            "Work",
            "-work",
            "a/b",
            "a.b",
            "",
            &"a".repeat(MAX_ACCOUNT_NAME + 1),
        ] {
            let e = check_account(bad).unwrap_err().to_string();
            assert!(e.contains("want [a-z0-9][a-z0-9_-]*"), "{bad}: {e}");
        }
    }

    #[test]
    fn the_account_name_error_never_repeats_what_was_pasted() {
        // A key pasted where the name goes reaches a chat notice and an attach reply: the error
        // may say what a name looks like, never what was typed.
        for pasted in [
            "sk-ant-api03-ZZl7c2VjcmV0LXZhbHVlLXRoYXQtaXMtbG9uZw",
            "sk-proj-3f9a8b7c6d5e4f3a2b1c0d9e8f7a6b5c4d3e2f1a",
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789",
            // Unbroken and lowercase, so only the unbroken-run rule can refuse it.
            &"a".repeat(MAX_ACCOUNT_NAME),
        ] {
            let e = check_account(pasted).unwrap_err().to_string();
            assert!(!e.contains(pasted), "{e}");
            assert!(
                !e.contains(&pasted[..8.min(pasted.len())]),
                "a prefix reached the error: {e}"
            );
        }
    }

    #[test]
    fn an_unbroken_line_is_capped_instead_of_growing_the_buffer() {
        // Three megabytes with no newline at all: the reader must hand out entries, each no
        // longer than MAX_LINE, or the buffer defeats MAX_LINES.
        let body: Vec<u8> = vec![b'x'; MAX_LINE * 3 + 7];
        let stream = tokio::io::BufReader::new(&body[..]);
        let login = Arc::new(Login {
            id: "l0".into(),
            provider: "opencode-go",
            account: "work".into(),
            seq: 0,
            inner: Mutex::new(Inner::default()),
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(read(login.clone(), stream));
        let inner = lock(&login.inner);
        assert_eq!(inner.lines.len(), 4, "one entry per cap, and the tail");
        assert!(inner.lines.iter().all(|l| l.len() <= MAX_LINE));
    }

    #[test]
    fn a_carriage_return_breaks_a_line_and_its_control_characters_go() {
        assert_eq!(plain("\r> sk-an\r"), "> sk-an");
        assert_eq!(plain("a\tb"), "a\tb");
        assert_eq!(
            plain(&"y".repeat(MAX_LINE + 10)).len(),
            MAX_LINE + "…".len()
        );
    }

    #[test]
    fn the_shell_sees_the_command_as_one_word() {
        assert_eq!(quote("/a b/rpi"), "'/a b/rpi'");
        assert_eq!(quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn a_sign_in_runs_script_by_full_path() {
        assert_eq!(SCRIPT, "/usr/bin/script");
    }
}
