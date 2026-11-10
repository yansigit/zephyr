//! Drives one `rpi --mode rpc` child: JSONL on stdio, frames as in upstream
//! `crates/rpi-cli/src/rpc/protocol.rs` and `modes.rs` (v0.18.0).

use crate::config::{Account, Config};
use anyhow::{anyhow, bail, Context};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

/// The session every channel shares (DESIGN §4.1), until `/new` starts `main-<unix secs>`
/// (`Store::main_session`). Attach always shows this name.
pub const MAIN_SESSION: &str = "main";

/// Ends the rpi children a crashed daemon left behind (DESIGN §4.4): `zephyr supervise` starts a
/// new daemon in the same container, and an old child still holds its session open, so the new
/// one could not resume it. Their pipes to the old daemon are gone, so they have no work left.
/// Matched exactly: this `rpi` binary, `--mode rpc`, this `--session-dir`, and the agent's uid
/// (ours without one). A group is killed only when the process leads it (children since this
/// change do); one from an older build shares the supervisor's group, so only it is killed.
/// Returns how many. Called before anything is spawned, so none of these are ours.
pub fn kill_leftovers(rpi: &std::path::Path, session_dir: &std::path::Path) -> usize {
    let uid = crate::agent::ids().map_or_else(|| unsafe { libc::getuid() }, |(u, _)| u);
    let (rpi, dir) = (rpi.to_string_lossy(), session_dir.to_string_lossy());
    let me = std::process::id();
    let mut killed = 0;
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        if pid == me {
            continue;
        }
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        // argv[0] is the binary, or argv[1] when it is a script run through its interpreter.
        let ours = args.iter().take(2).any(|a| *a == rpi)
            && args.windows(2).any(|w| w[0] == "--mode" && w[1] == "rpc")
            && args
                .windows(2)
                .any(|w| w[0] == "--session-dir" && w[1] == dir);
        if !ours || proc_uid(pid) != Some(uid) {
            continue;
        }
        let pid = pid as i32;
        let target = if proc_pgid(pid) == Some(pid) {
            -pid
        } else {
            pid
        };
        // SAFETY: a plain syscall; the target was matched just above.
        if unsafe { libc::kill(target, libc::SIGKILL) } == 0 {
            killed += 1;
        }
    }
    if killed > 0 {
        eprintln!("zephyr: ended {killed} rpi process(es) a previous daemon left behind");
    }
    killed
}

fn proc_uid(pid: u32) -> Option<u32> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(1)?.parse().ok()
}

/// Field 5 of `/proc/<pid>/stat`, read after the command name's last `)`.
fn proc_pgid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat[stat.rfind(')')? + 1..]
        .split_whitespace()
        .nth(2)?
        .parse()
        .ok()
}

/// The main sessions on disk, newest first, as (id, mtime in unix secs). rpi files them as
/// `<dir>/<cwd folder>/<ISO>_<id>.jsonl`. Only `main` and the
/// `main-<secs>` ids `/new` and `/fork` make count, so `/switch` never hands rpi a job's or a
/// stray name.
pub fn saved_sessions(dir: &std::path::Path) -> Vec<(String, i64)> {
    let mut found: HashMap<String, i64> = HashMap::new();
    for (id, _, mtime) in main_files(dir) {
        let at = found.entry(id).or_insert(mtime);
        *at = (*at).max(mtime);
    }
    let mut list: Vec<_> = found.into_iter().collect();
    list.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    list
}

/// Session `id`'s file: the newest, should there be several.
fn session_file(dir: &std::path::Path, id: &str) -> Option<std::path::PathBuf> {
    (main_files(dir).into_iter())
        .filter(|f| f.0 == id)
        .max_by_key(|f| f.2)
        .map(|f| f.1)
}

/// One saved file as zephyr last saw it: its length, inode, change and modification times and
/// SHA-256. The change time cannot be set back by the agent, so a file whose length, inode and
/// both times are as recorded is not hashed again.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FilePrint {
    len: u64,
    ino: u64,
    ctime: (i64, i64),
    mtime: (i64, i64),
    hash: String,
    /// When it was hashed, in unix seconds.
    seen: i64,
}

/// The same file as seen, whenever it was hashed.
impl PartialEq for FilePrint {
    fn eq(&self, o: &FilePrint) -> bool {
        (self.len, self.ino, self.ctime, self.mtime, &self.hash)
            == (o.len, o.ino, o.ctime, o.mtime, &o.hash)
    }
}

impl Eq for FilePrint {}

/// A file that is there but not a plain file zephyr can read whole: a link, a folder, one that
/// changed while it was read.
#[derive(Debug)]
struct Bad;

/// `path`'s metadata, `None` when there is no file, `Bad` when it is not a plain file.
fn plain(path: &std::path::Path) -> Result<Option<std::fs::Metadata>, Bad> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Ok(m) if m.is_file() => Ok(Some(m)),
        _ => Err(Bad),
    }
}

/// `path`'s bytes, read whole between two looks that agree: what was read is what is there.
fn read_whole(path: &std::path::Path, m: &std::fs::Metadata) -> Result<Vec<u8>, Bad> {
    let bytes = std::fs::read(path).map_err(|_| Bad)?;
    let after = plain(path)?.ok_or(Bad)?;
    let key = |m: &std::fs::Metadata| {
        use std::os::unix::fs::MetadataExt;
        (m.len(), m.ino(), m.ctime(), m.ctime_nsec())
    };
    match bytes.len() as u64 == m.len() && key(m) == key(&after) {
        true => Ok(bytes),
        false => Err(Bad),
    }
}

fn hex(ctx: ring::digest::Context) -> String {
    data_encoding::HEXLOWER.encode(ctx.finish().as_ref())
}

impl FilePrint {
    fn with(m: &std::fs::Metadata, hash: String, seen: i64) -> FilePrint {
        use std::os::unix::fs::MetadataExt;
        FilePrint {
            len: m.len(),
            ino: m.ino(),
            ctime: (m.ctime(), m.ctime_nsec()),
            mtime: (m.mtime(), m.mtime_nsec()),
            hash,
            seen,
        }
    }

    /// `path` now (`None`: no file); `prior`'s hash is reused only when nothing else about the
    /// file differs and it was hashed more than a second after its last change: file times tick
    /// coarsely, so a write in the same tick as the one hashed would keep them.
    fn of(path: &std::path::Path, prior: Option<&FilePrint>) -> Result<Option<FilePrint>, Bad> {
        let Some(m) = plain(path)? else {
            return Ok(None);
        };
        let now = FilePrint::with(&m, String::new(), 0);
        let same = |p: &&FilePrint| {
            (p.len, p.ino, p.ctime, p.mtime) == (now.len, now.ino, now.ctime, now.mtime)
                && p.seen > p.ctime.0 + 1
        };
        if let Some(p) = prior.filter(same) {
            return Ok(Some(FilePrint::with(&m, p.hash.clone(), p.seen)));
        }
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        ctx.update(&read_whole(path, &m)?);
        Ok(Some(FilePrint::with(&m, hex(ctx), crate::now())))
    }
}

/// What session `id`'s saved files are (DESIGN §4.6): how many there are, the newest one's path,
/// and its and its `.progress.jsonl`'s prints. No file at all is the `Default`.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionPrint {
    count: usize,
    path: Option<String>,
    file: Option<FilePrint>,
    progress: Option<FilePrint>,
    /// One of them is there but not a plain file zephyr could read whole.
    #[serde(default)]
    bad: bool,
}

impl SessionPrint {
    /// Session `id`'s files, and the newest one's and its companion's paths.
    fn files(
        dir: &std::path::Path,
        id: &str,
    ) -> (usize, Option<(std::path::PathBuf, std::path::PathBuf)>) {
        let files: Vec<_> = (main_files(dir).into_iter())
            .filter(|f| f.0 == id)
            .collect();
        let newest = files.iter().max_by_key(|f| f.2).map(|f| {
            let progress = format!("{}.progress.jsonl", f.1.display());
            (f.1.clone(), std::path::PathBuf::from(progress))
        });
        (files.len(), newest)
    }

    /// Session `id` now, hashing only what differs from `prior`.
    pub fn now(dir: &std::path::Path, id: &str, prior: Option<&SessionPrint>) -> SessionPrint {
        let (count, newest) = SessionPrint::files(dir, id);
        let Some((file, progress)) = newest else {
            return SessionPrint::default();
        };
        let _fs = crate::agent::fs();
        let path = file.display().to_string();
        // A print of another file is no help.
        let prior = prior.filter(|p| p.path.as_deref() == Some(path.as_str()));
        let file = FilePrint::of(&file, prior.and_then(|p| p.file.as_ref()));
        let progress = FilePrint::of(&progress, prior.and_then(|p| p.progress.as_ref()));
        let bad = !matches!(file, Ok(Some(_))) || progress.is_err();
        SessionPrint {
            count,
            path: Some(path),
            file: file.ok().flatten(),
            progress: progress.ok().flatten(),
            bad,
        }
    }

    /// Whether there is any file.
    pub fn exists(&self) -> bool {
        self.count > 0
    }

    /// Whether a file is there that is not a plain file zephyr could read whole.
    pub fn bad(&self) -> bool {
        self.bad
    }

    /// Session `id` now, if it is this print with lines added after it, as rpi adds them (or
    /// unchanged): the same one file, its first bytes as hashed here; from no file, rpi's first.
    /// The newest file is read once, and what is returned is the print of the bytes checked.
    /// The progress companion, which rpi rewrites, only has to be a plain file.
    pub fn follow(&self, dir: &std::path::Path, id: &str) -> Option<SessionPrint> {
        if self.bad {
            return None;
        }
        let (count, newest) = SessionPrint::files(dir, id);
        let Some((file, progress)) = newest else {
            return (self.count == 0).then(SessionPrint::default);
        };
        let path = file.display().to_string();
        let same = match &self.file {
            None => self.count == 0 && count == 1,
            Some(_) => count == self.count && self.path.as_deref() == Some(path.as_str()),
        };
        if !same {
            return None;
        }
        let _fs = crate::agent::fs();
        let m = plain(&file).ok()??;
        let bytes = read_whole(&file, &m).ok()?;
        let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
        if let Some(then) = &self.file {
            let head = bytes.get(..then.len as usize)?;
            ctx.update(head);
            if hex(ctx.clone()) != then.hash {
                return None;
            }
            ctx.update(&bytes[head.len()..]);
        } else {
            ctx.update(&bytes);
        }
        let progress = FilePrint::of(&progress, None).ok()?;
        Some(SessionPrint {
            count,
            path: Some(path),
            file: Some(FilePrint::with(&m, hex(ctx), crate::now())),
            progress,
            bad: false,
        })
    }
}

/// The first words of session `id`'s first user message, untagged, for `/switch`.
pub fn opening(dir: &std::path::Path, id: &str) -> Option<String> {
    use std::io::BufRead;
    let path = session_file(dir, id)?;
    let _fs = crate::agent::fs();
    let file = std::fs::File::open(&path).ok()?;
    // One entry per line, so only the top of a long session is read.
    let text = (std::io::BufReader::new(file).lines().map_while(Result::ok)).find_map(|line| {
        let v: Value = serde_json::from_str(&line).ok()?;
        (v["message"]["kind"] == "user").then(|| message_text(&v["message"]))
    })?;
    let body = crate::router::untag(&text).map_or(text.as_str(), |(_, _, body)| body);
    let words: Vec<&str> = body.split_whitespace().collect();
    Some(match words.len() {
        0..=6 => words.join(" "),
        _ => format!("{}…", words[..6].join(" ")),
    })
}

/// `/fork` (DESIGN §4.1): copies session `from` to the new id `to`, leaf and all, as the agent.
/// rpi 0.18 has no `fork` command, and starts a copy only when the ids inside match its name:
/// the `.jsonl` header line's `id` and its `.progress.jsonl` companion's `session_id`.
pub fn copy_session(dir: &std::path::Path, from: &str, to: &str) -> anyhow::Result<()> {
    use std::io::Write;
    let src = session_file(dir, from).with_context(|| format!("no session file for {from}"))?;
    let name = src.file_name().unwrap_or_default().to_string_lossy();
    let stamp = &name[..name.len() - ".jsonl".len() - from.len()];
    let dst = src.with_file_name(format!("{stamp}{to}.jsonl"));
    let _fs = crate::agent::fs();
    let renamed = |path: &std::path::Path, key: &str| -> anyhow::Result<String> {
        let text = std::fs::read_to_string(path)?;
        let (head, rest) = text.split_once('\n').unwrap_or((&text, ""));
        let mut head: Value = serde_json::from_str(head)?;
        if head.get(key).is_none() {
            bail!("{} has no {key} to rename", path.display());
        }
        head[key] = json!(to);
        Ok(format!("{head}\n{rest}"))
    };
    let create = |path: std::path::PathBuf, body: String| -> std::io::Result<()> {
        let mut f = std::fs::File::create_new(path)?;
        f.write_all(body.as_bytes())?;
        f.sync_all()
    };
    // The companion first: a session file without one would be listed, and rpi refuse it.
    let companion = |p: &std::path::Path| format!("{}.progress.jsonl", p.display());
    let progress = std::path::PathBuf::from(companion(&src));
    if progress.exists() {
        create(companion(&dst).into(), renamed(&progress, "session_id")?)?;
    }
    create(dst, renamed(&src, "id")?)?;
    Ok(())
}

/// The main session files on disk, as (id, path, mtime in unix secs).
fn main_files(dir: &std::path::Path) -> Vec<(String, std::path::PathBuf, i64)> {
    // The sessions are in the agent's home, which root cannot enter (DESIGN §4.4).
    let _fs = crate::agent::fs();
    let entries = |d: &std::path::Path| {
        std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .flatten()
            .collect::<Vec<_>>()
    };
    let mut found = vec![];
    // One folder per cwd, as rpi files them.
    for e in entries(dir).into_iter().filter(|e| e.path().is_dir()) {
        for f in entries(&e.path()) {
            let name = f.file_name().to_string_lossy().into_owned();
            let Some(stem) = name.strip_suffix(".jsonl") else {
                continue;
            };
            let id = stem.rsplit_once('_').map_or(stem, |(_, id)| id);
            let numbered = id
                .strip_prefix("main-")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
            if id != MAIN_SESSION && !numbered {
                continue;
            }
            let mtime = f
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs() as i64);
            found.push((id.to_string(), f.path(), mtime));
        }
    }
    found
}

/// The slash command the policy plugin registers as proof that it loaded.
pub const POLICY_BEACON: &str = "zephyr-policy";

/// Commands zephyr sends; `doctor` checks them against `ready.supportedCommands`.
pub const USED_COMMANDS: &[&str] = &[
    "prompt",
    "get_commands",
    "queue_message",
    "clear_queue",
    "abort",
    "get_state",
    "get_messages",
    "resume_pending",
    "get_auth_status",
    "set_model",
    "stop",
    "compact",
    "navigate",
    "get_available_thinking_levels",
    "set_thinking_level",
];

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Child {
    pub ready: Value,
    /// Every frame that is not a command response. Closes when the child exits.
    pub events: mpsc::UnboundedReceiver<Value>,
    /// The session it runs, for the audit; `None` for a `/btw` child.
    pub session: Option<String>,
    /// A task's or job's inbox for notes from outside its run (`scheduler::Steers`), which
    /// `scheduler::follow` steers in; `None` for main and `/btw`.
    pub notes: Option<mpsc::UnboundedReceiver<crate::scheduler::Note>>,
    stdin: ChildStdin,
    pending: Pending,
    next_id: u64,
    process: Group,
}

/// The rpi process, which leads its own group (`spawn_as`). Dropped before it is reaped (a task's
/// child at a re-exec, say), it takes the whole group with it: the tools it started, a build or a
/// dev server, would otherwise run on as orphans beside the run that resumes.
struct Group(tokio::process::Child);

impl Drop for Group {
    fn drop(&mut self) {
        // Not yet reaped, so the pid is still this group's and cannot have been reused.
        if let Some(pid) = self.0.id() {
            // SAFETY: a plain syscall.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    }
}

impl Child {
    /// A child on `agent_dir` and `model`.
    pub async fn spawn(cfg: &Config, session_id: &str) -> anyhow::Result<Self> {
        Self::spawn_as(cfg, Some(session_id), None).await
    }

    /// A child on a pooled account: its own dir as `RPI_CODING_AGENT_DIR`, and its model.
    /// With no `session_id` it is a `/btw` child (`crate::btw`): no session file, no tools.
    pub async fn spawn_as(
        cfg: &Config,
        session_id: Option<&str>,
        account: Option<&Account>,
    ) -> anyhow::Result<Self> {
        {
            let _fs = crate::agent::fs();
            std::fs::create_dir_all(cfg.session_dir())?;
            std::fs::create_dir_all(crate::tools::gateway_dir(cfg))?;
        }
        let (model, agent_dir) = match account {
            Some(a) => (Some(&a.model), Some(crate::pool::prepare(cfg, a)?)),
            None => (cfg.model.as_ref(), cfg.agent_dir.clone()),
        };
        // rpi reads the context files (AGENTS.md) once, at start, so regenerate them first.
        let context_dir = agent_dir
            .clone()
            .unwrap_or_else(|| crate::auth_keeper::agent_dir(cfg));
        // memory::prepare, then the approved skills and connectors (DESIGN §4.6).
        crate::drafts::prepare(cfg, &context_dir)?;
        let mut cmd = Command::new(&cfg.rpi);
        crate::agent::cmd(cmd.as_std_mut());
        // AGENTS.md is the agent's only standing instruction (memory::prepare): no skills it
        // could plant, and a trust.json it wrote can't switch AGENTS.md off. `--local-only`
        // loads only the `--extension` files below, none from a dir or setting the agent can write.
        // rpi 0.15 defaults to compact deltas; the TUI streams from each update's `message`.
        cmd.args(["--mode", "rpc", "--approve", "--no-skills", "--local-only"])
            .args(["--rpc-message-updates", "snapshot"])
            // rpi files sessions under a per-cwd folder, so the cwd must never change.
            .current_dir(&cfg.home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        // Its own process group, so a daemon that finds it left over from a crash can end it and
        // what it started (the provider's runtime) together: `kill_leftovers`.
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }
        match session_id {
            Some(id) => cmd
                .args(["--session-id", id, "--session-dir"])
                .arg(cfg.session_dir()),
            None => cmd.args(["--no-session", "--no-tools"]),
        };
        // The main session (`main`, `main-<secs>`) runs commands through zephyr's `exec`, which can
        // move to the background when the owner speaks (DESIGN §4.13); jobs and tasks keep `bash`.
        if session_id.is_some_and(|id| id == MAIN_SESSION || id.starts_with("main-")) {
            cmd.args(["--exclude-tools", "bash"])
                .env(zephyr_policy::tools::EXEC_ENV, "1");
        }
        if let Some(model) = model {
            cmd.args(["--model", model]);
        }
        if let Some(plugin) = &cfg.policy_plugin {
            cmd.arg("--extension").arg(plugin);
        }
        for ext in &cfg.extensions {
            cmd.arg("--extension").arg(ext);
        }
        if let Some(dir) = &agent_dir {
            cmd.env("RPI_CODING_AGENT_DIR", dir);
        }
        cmd.env(
            zephyr_policy::tools::GATEWAY_DIR_ENV,
            crate::tools::gateway_dir(cfg),
        );
        // `recall` reads the session files itself (DESIGN §4.11).
        cmd.env(zephyr_policy::tools::SESSIONS_DIR_ENV, cfg.session_dir());
        // Where the plugin asks about a send, and as which session (approvals spec §4.2).
        cmd.envs(crate::gate::agent_env(cfg, session_id.unwrap_or_default()));
        // The only MCP config the policy plugin lets the adapter's tools use (DESIGN §4.6).
        cmd.env(
            zephyr_policy::policy::MCP_CONFIG_ENV,
            crate::drafts::mcp_path(cfg),
        );
        if let Some(display) = crate::desktop::ensure() {
            cmd.env("DISPLAY", display);
            // The desktop takes only clients holding its cookie (`desktop` module docs).
            if let Some(auth) = crate::desktop::xauthority() {
                cmd.env("XAUTHORITY", auth);
            }
        }
        let mut process = cmd
            .spawn()
            .with_context(|| format!("spawning {}", cfg.rpi.display()))?;
        let stdin = process.stdin.take().expect("piped stdin");
        let mut lines = BufReader::new(process.stdout.take().expect("piped stdout")).lines();

        let ready = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(line) = lines.next_line().await? {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v["type"] == "ready" {
                        return Ok(v);
                    }
                }
            }
            bail!("rpi exited before ready")
        })
        .await
        .map_err(|_| anyhow!("no ready frame from rpi within 30s"))??;

        let pending: Pending = Arc::default();
        let (tx, events) = mpsc::unbounded_channel();
        let reader_pending = pending.clone();
        tokio::spawn(async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                let waiter = (v["type"] == "response")
                    .then(|| v["id"].as_u64())
                    .flatten()
                    .and_then(|id| reader_pending.lock().unwrap().remove(&id));
                match waiter {
                    Some(w) => drop(w.send(v)),
                    None => drop(tx.send(v)),
                }
            }
            // Dropping `tx` and the waiters tells the router the child is gone.
            reader_pending.lock().unwrap().clear();
        });
        let mut child = Self {
            ready,
            events,
            session: session_id.map(Into::into),
            notes: None,
            stdin,
            pending,
            next_id: 0,
            process: Group(process),
        };
        if cfg.policy_plugin.is_some() {
            let listed = child.call(json!({"type": "get_commands"})).await?;
            let loaded = listed["commands"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|c| {
                    c["name"].as_str().map(|n| n.trim_start_matches('/')) == Some(POLICY_BEACON)
                });
            if !loaded {
                child.stop().await;
                bail!("policy plugin did not load: `{POLICY_BEACON}` is not in get_commands");
            }
        }
        Ok(child)
    }

    /// Sends one command and returns its `data`, or the child's error string.
    pub async fn call(&mut self, mut cmd: Value) -> anyhow::Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        cmd["id"] = json!(id);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let mut line = serde_json::to_vec(&cmd)?;
        line.push(b'\n');
        // The write is timed too: a child that stops reading fills its stdin pipe.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let write = async {
            self.stdin.write_all(&line).await?;
            self.stdin.flush().await
        };
        let Ok(written) = tokio::time::timeout_at(deadline, write).await else {
            self.pending.lock().unwrap().remove(&id);
            // A half-written line has broken the stream: end the child, whose exit the caller sees.
            let _ = self.process.0.start_kill();
            bail!("{} timed out: rpi is not reading its input", cmd["type"]);
        };
        written?;
        let Ok(resp) = tokio::time::timeout_at(deadline, rx).await else {
            // A late answer then finds no waiter and goes out as an event, which the router ignores.
            self.pending.lock().unwrap().remove(&id);
            bail!("{} timed out", cmd["type"]);
        };
        let resp = resp.map_err(|_| anyhow!("rpi exited during {}", cmd["type"]))?;
        if resp["success"] == true {
            Ok(resp["data"].clone())
        } else {
            bail!("{}: {}", cmd["type"], resp["error"])
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.process.0.id()
    }

    /// `stop` lets rpi drain its session; then wait for the process to exit.
    pub async fn stop(mut self) {
        let _ =
            tokio::time::timeout(Duration::from_secs(5), self.call(json!({"type": "stop"}))).await;
        drop(self.stdin);
        // Past that, dropping `self.process` kills its group.
        let _ = tokio::time::timeout(Duration::from_secs(10), self.process.0.wait()).await;
    }
}

/// What one run did that zephyr answers for, read from its events.
#[derive(Debug, Default)]
pub struct Tally {
    /// Tokens and cost of the replies. rpi's `get_stats` sums only usage that extensions
    /// record, never the replies' own, so spend is summed from each assistant `message_end`.
    pub used: (i64, f64),
    /// Bash commands that sent data off the machine (`zephyr_policy::policy::outbound`).
    pub outbound: Vec<String>,
    /// Model turns taken (`turn_end`), for `budget.run_turns`.
    pub turns: u32,
    /// `send_file` and `schedule` calls that succeeded, as `(tool, args)`: the daemon carries
    /// them out when the run settles (`crate::tools`, DESIGN §4.11).
    pub requests: Vec<(String, Value)>,
    /// Those calls between their start and end, by tool call id.
    started: std::collections::HashMap<String, (String, Value)>,
    /// Every tool call between its start and end, as `(tool, its args)`, for the guard.
    calls: std::collections::HashMap<String, (String, String)>,
    /// The same call failing with the same error, in a row (DESIGN §4.4).
    streak: Streak,
    /// Main's home (`Tally::main`): its tool calls are judged against the clean list.
    home: Option<std::path::PathBuf>,
    /// The first of main's calls that could have read text that is not the owner's, as
    /// `used TOOL` (DESIGN §4.6); a task's or job's tally never sets it.
    pub taint: Option<String>,
}

/// A call of main's that reads or writes only the memory the owner sees (DESIGN §4.6): `read`
/// and `edit` of `memory/USER.md` or `memory/MEMORY.md`, `write` of a file under `memory/`. The
/// path is taken relative to `home` when absolute; `~`, `..`, `.`, an empty part and any
/// dot-named part make it not clean.
fn clean_call(home: &Path, tool: &str, path: Option<&str>) -> bool {
    let (Some(path), Some(home)) = (path, home.to_str()) else {
        return false;
    };
    let rel = match path.starts_with('/') {
        true => (path.strip_prefix(home.trim_end_matches('/'))).and_then(|r| r.strip_prefix('/')),
        false => Some(path),
    };
    let Some(rel) = rel else { return false };
    let parts: Vec<&str> = rel.split('/').collect();
    if parts
        .iter()
        .any(|c| c.is_empty() || c.starts_with('.') || c.starts_with('~'))
    {
        return false;
    }
    let listed = matches!(
        (tool, &parts[..]),
        ("read" | "edit", ["memory", "USER.md" | "MEMORY.md"]) | ("write", ["memory", _, ..])
    );
    // The path names a memory file; the files must be what they say: plain folders down to a
    // plain file (a `write` may make a new one), never a link to text from anywhere else.
    let _fs = crate::agent::fs();
    let mut at = Path::new(home).to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        let last = i + 1 == parts.len();
        match at.symlink_metadata() {
            Ok(m) if last => return listed && m.is_file(),
            Ok(m) if m.is_dir() => {}
            Err(e) if last && tool == "write" && e.kind() == std::io::ErrorKind::NotFound => {
                return listed
            }
            _ => return false,
        }
    }
    false
}

/// Identical failures in a row: the call (tool and args), the error's first line, how many,
/// and the count the guard last acted on.
#[derive(Debug, Default)]
struct Streak {
    call: (String, String),
    error: String,
    count: u32,
    acted: u32,
}

/// What the repeated-failure guard asks of a run (DESIGN §4.4).
#[derive(Debug, PartialEq)]
pub enum Guard {
    /// Steer this note into the run.
    Steer(String),
    /// Stop the run; the text says why.
    Stop(String),
}

/// Identical failures in a row before the guard steers a note in, and before it stops the run.
pub const GUARD_STEER: u32 = 3;
pub const GUARD_STOP: u32 = 5;

impl Tally {
    /// Main's tally, which judges each tool call against the clean list.
    pub fn main(home: &Path) -> Tally {
        Tally {
            home: Some(home.to_path_buf()),
            ..Tally::default()
        }
    }

    pub fn add(&mut self, ev: &Value) {
        if let (Some(home), None, Some("tool_execution_start")) =
            (&self.home, &self.taint, ev["type"].as_str())
        {
            let tool = ev["toolName"].as_str().unwrap_or("an unnamed tool");
            if !clean_call(home, tool, ev["args"]["path"].as_str()) {
                self.taint = Some(format!("used {tool}"));
            }
        }
        let m = &ev["message"];
        if ev["type"] == "message_end" && m["role"] == "assistant" {
            self.used.0 += m["usage"]["totalTokens"].as_i64().unwrap_or(0);
            self.used.1 += m["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
        }
        if ev["type"] == "turn_end" {
            self.turns += 1;
        }
        if let (Some(id), Some(tool @ ("send_file" | "schedule"))) =
            (ev["toolCallId"].as_str(), ev["toolName"].as_str())
        {
            if ev["type"] == "tool_execution_start" {
                self.started
                    .insert(id.into(), (tool.into(), ev["args"].clone()));
            }
            if ev["type"] == "tool_execution_end" {
                let call = self.started.remove(id);
                self.requests.extend(call.filter(|_| ev["isError"] != true));
            }
        }
        self.follow_failures(ev);
        // From rpi, not from anything the agent writes, so the agent cannot hide a send.
        let shell = matches!(ev["toolName"].as_str(), Some("bash" | "exec"));
        if ev["type"] == "tool_execution_start" && shell {
            if let Some(command) = ev["args"]["command"].as_str() {
                self.outbound
                    .extend(zephyr_policy::policy::outbound(command));
            }
        }
    }

    /// Keys each tool result on its tool, args and the error's first line: the same failure
    /// lengthens the streak, any other result ends it.
    fn follow_failures(&mut self, ev: &Value) {
        let (Some(id), Some(tool)) = (ev["toolCallId"].as_str(), ev["toolName"].as_str()) else {
            return;
        };
        if ev["type"] == "tool_execution_start" {
            let args = ev["args"].to_string();
            self.calls.insert(id.into(), (tool.into(), args));
            return;
        }
        if ev["type"] != "tool_execution_end" {
            return;
        }
        let call = (self.calls.remove(id)).unwrap_or_else(|| (tool.into(), String::new()));
        if ev["isError"] != true {
            self.streak = Streak::default();
            return;
        }
        let text = message_text(&ev["result"]);
        let error = text.lines().next().unwrap_or_default().trim().to_string();
        if self.streak.count > 0 && self.streak.call == call && self.streak.error == error {
            self.streak.count += 1;
        } else {
            self.streak = Streak {
                call,
                error,
                count: 1,
                acted: 0,
            };
        }
    }

    /// What the repeated-failure guard asks for now, at most once per failure: a note at
    /// `GUARD_STEER` identical failures in a row, a stop at `GUARD_STOP`.
    pub fn guard(&mut self) -> Option<Guard> {
        let s = &mut self.streak;
        if s.count == s.acted {
            return None;
        }
        s.acted = s.count;
        let tool = &s.call.0;
        match s.count {
            GUARD_STEER => Some(Guard::Steer(format!(
                "[zephyr] {tool} failed {GUARD_STEER} times in a row with the same error ({}). \
                 Don't retry it unchanged: change approach, or stop and tell the owner.",
                s.error
            ))),
            n if n >= GUARD_STOP => Some(Guard::Stop(format!(
                "repeated a failing {tool} call {GUARD_STOP} times"
            ))),
            _ => None,
        }
    }

    /// The owner's notice of what the run sent out, if it sent anything.
    pub fn alert(&self) -> Option<String> {
        const SHOWN: usize = 10;
        if self.outbound.is_empty() {
            return None;
        }
        let mut text = String::from("[zephyr] outbound this turn:");
        for line in self.outbound.iter().take(SHOWN) {
            text.push_str("\n- ");
            text.push_str(line);
        }
        if self.outbound.len() > SHOWN {
            text.push_str(&format!("\n(+{} more)", self.outbound.len() - SHOWN));
        }
        Some(text)
    }
}

/// The text of a wire message: a plain string, or the `text` blocks of a content list.
pub fn message_text(msg: &Value) -> String {
    match &msg["content"] {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crashed_daemons_rpi_is_ended_and_no_other() {
        use std::os::unix::process::CommandExt;
        let fake = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"));
        let dir = crate::test_dir("rpc-leftovers");
        let (ours, other) = (dir.join("sessions"), dir.join("elsewhere"));
        let spawn = |sessions: &std::path::Path| {
            std::process::Command::new(fake)
                .args(["--mode", "rpc", "--session-id", "main", "--session-dir"])
                .arg(sessions)
                .env("ZEPHYR_HOME_FOR_FAKE", &dir)
                .current_dir(&dir)
                .stdin(std::process::Stdio::piped()) // held open: it waits on it like a real child
                .stdout(std::process::Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap()
        };
        let (mut left, mut kept) = (spawn(&ours), spawn(&other));
        std::thread::sleep(Duration::from_millis(300)); // past the interpreter's exec
        assert_eq!(kill_leftovers(fake, &ours), 1);
        let status = left.wait().unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(
            kept.try_wait().unwrap().is_none(),
            "another session dir is not ours"
        );
        assert_eq!(kill_leftovers(fake, &dir.join("none")), 0);
        let _ = kept.kill();
        let _ = kept.wait();
    }

    #[test]
    fn main_taints_on_any_call_off_the_clean_list() {
        let home = crate::test_dir("rpc-clean-list");
        std::fs::create_dir_all(home.join("memory/notes")).unwrap();
        for name in ["USER.md", "MEMORY.md"] {
            std::fs::write(home.join("memory").join(name), "x\n").unwrap();
        }
        let home = home.as_path();
        let abs = home.join("memory/USER.md").to_string_lossy().into_owned();
        let start = |tool: &str, path: Option<&str>| {
            let args = path.map_or(
                serde_json::json!({"command": "ls"}),
                |p| serde_json::json!({ "path": p }),
            );
            serde_json::json!({"type": "tool_execution_start", "toolCallId": "c",
                               "toolName": tool, "args": args})
        };
        let taint = |calls: &[(&str, Option<&str>)]| {
            let mut tally = Tally::main(home);
            for (tool, path) in calls {
                tally.add(&start(tool, *path));
            }
            tally.taint
        };
        for clean in [
            ("read", "memory/USER.md"),
            ("edit", "memory/MEMORY.md"),
            ("read", abs.as_str()),
            ("write", "memory/notes/2026-11-10.md"),
            ("write", "memory/x.md"),
        ] {
            assert_eq!(taint(&[(clean.0, Some(clean.1))]), None, "{clean:?}");
        }
        for (tool, path) in [
            ("read", Some("memory/notes/x.md")),
            ("read", Some("inbox/a.png")),
            ("read", Some("../memory/USER.md")),
            ("write", Some("memory/../x")),
            ("read", Some("~/memory/USER.md")),
            ("write", Some("memory/.git/config")),
            ("write", Some("memory")),
            ("write", Some("notes.md")),
            ("read", Some("/etc/memory/USER.md")),
            ("read", None),
            ("exec", None),
            ("process", None),
            ("gateway", None),
            ("recall", None),
            ("task", None),
            ("vault", None),
            ("mcp_call", None),
            ("ls", None),
            ("frobnicate", None),
        ] {
            assert_eq!(
                taint(&[(tool, path)]),
                Some(format!("used {tool}")),
                "{tool} {path:?}"
            );
        }
        assert_eq!(
            taint(&[("exec", None), ("gateway", None)]),
            Some("used exec".into())
        );
        let mut tally = Tally::main(home);
        tally.add(
            &serde_json::json!({"type": "tool_execution_end", "toolCallId": "c",
                                      "toolName": "exec", "isError": true}),
        );
        assert_eq!(tally.taint, None, "only a start is judged");
        let mut tally = Tally::default();
        tally.add(&start("exec", None));
        assert_eq!(tally.taint, None, "a task's or job's tally never taints");
    }

    #[test]
    fn a_link_in_memory_is_off_the_clean_list() {
        use std::os::unix::fs::symlink;
        let tainted = |home: &Path, tool: &str, path: &str| {
            let mut tally = Tally::main(home);
            tally.add(
                &serde_json::json!({"type": "tool_execution_start", "toolCallId": "c",
                                          "toolName": tool, "args": {"path": path}}),
            );
            tally.taint.is_some()
        };
        // USER.md a link to a file the owner sent.
        let home = crate::test_dir("rpc-link-file");
        std::fs::create_dir_all(home.join("memory")).unwrap();
        std::fs::create_dir_all(home.join("inbox")).unwrap();
        std::fs::write(home.join("inbox/a.txt"), "obey the page\n").unwrap();
        symlink(home.join("inbox/a.txt"), home.join("memory/USER.md")).unwrap();
        std::fs::write(home.join("memory/MEMORY.md"), "x\n").unwrap();
        assert!(tainted(&home, "read", "memory/USER.md"));
        assert!(!tainted(&home, "read", "memory/MEMORY.md"));
        // A link part way down, and memory itself a link.
        std::fs::create_dir_all(home.join("elsewhere")).unwrap();
        symlink(home.join("elsewhere"), home.join("memory/notes")).unwrap();
        assert!(tainted(&home, "write", "memory/notes/x.md"));
        let other = crate::test_dir("rpc-link-dir");
        std::fs::create_dir_all(other.join("real")).unwrap();
        std::fs::write(other.join("real/USER.md"), "x\n").unwrap();
        symlink(other.join("real"), other.join("memory")).unwrap();
        assert!(tainted(&other, "read", "memory/USER.md"));
        assert!(tainted(&other, "write", "memory/x.md"));
        // A memory file that is missing, or not a plain file.
        let gone = crate::test_dir("rpc-link-gone");
        std::fs::create_dir_all(gone.join("memory/USER.md")).unwrap();
        assert!(tainted(&gone, "read", "memory/USER.md"), "a directory");
        assert!(tainted(&gone, "read", "memory/MEMORY.md"), "missing");
        assert!(!tainted(&gone, "write", "memory/MEMORY.md"), "a new file");
    }

    #[test]
    fn tally_keeps_only_daemon_tool_calls_that_succeeded() {
        let mut tally = Tally::default();
        let ev = |kind: &str, id: &str, tool: &str, error: bool| {
            serde_json::json!({"type": kind, "toolCallId": id, "toolName": tool,
                               "args": {"path": id}, "isError": error})
        };
        for (id, tool, error) in [
            ("a", "send_file", false),
            ("b", "send_file", true),
            ("c", "read", false),
        ] {
            tally.add(&ev("tool_execution_start", id, tool, false));
            tally.add(&ev("tool_execution_end", id, tool, error));
        }
        tally.add(&ev(
            "tool_execution_end",
            "never-started",
            "schedule",
            false,
        ));
        assert_eq!(
            tally.requests,
            [("send_file".to_string(), serde_json::json!({"path": "a"}))]
        );
    }

    #[test]
    fn the_guard_counts_only_the_same_failure_in_a_row() {
        let mut tally = Tally::default();
        let mut n = 0;
        let mut call = |tally: &mut Tally, path: &str, error: Option<&str>| {
            n += 1;
            let id = format!("c{n}");
            tally.add(
                &serde_json::json!({"type": "tool_execution_start", "toolCallId": id,
                                          "toolName": "read", "args": {"path": path}}),
            );
            tally.add(
                &serde_json::json!({"type": "tool_execution_end", "toolCallId": id,
                "toolName": "read", "isError": error.is_some(),
                "result": {"content": [{"type": "text", "text": error.unwrap_or("ok")}]}}),
            );
            tally.guard()
        };
        let missing = Some("no such file\nat line 2");
        // Two, then a success, then another path: neither streak reaches 3.
        assert_eq!(call(&mut tally, "a", missing), None);
        assert_eq!(call(&mut tally, "a", missing), None);
        assert_eq!(call(&mut tally, "a", None), None);
        assert_eq!(call(&mut tally, "a", missing), None);
        assert_eq!(call(&mut tally, "b", missing), None);
        assert_eq!(call(&mut tally, "b", Some("denied")), None);
        assert_eq!(call(&mut tally, "b", Some("denied")), None);
        let Some(Guard::Steer(note)) = call(&mut tally, "b", Some("denied")) else {
            panic!("no note at 3");
        };
        assert_eq!(
            note,
            "[zephyr] read failed 3 times in a row with the same error (denied). Don't retry it \
             unchanged: change approach, or stop and tell the owner."
        );
        assert_eq!(tally.guard(), None, "acted on once");
        assert_eq!(call(&mut tally, "b", Some("denied")), None);
        assert_eq!(
            call(&mut tally, "b", Some("denied")),
            Some(Guard::Stop("repeated a failing read call 5 times".into()))
        );
    }

    #[test]
    fn saved_sessions_reads_only_main_ids() {
        let dir = std::env::temp_dir().join(format!(
            "zephyr-rpc-sessions-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        let real = dir.join("--zephyr--");
        std::fs::create_dir_all(&real).unwrap();
        for f in [
            "2026-10-30T03-32-24-353Z_main.jsonl",
            "2026-10-30T03-32-24-353Z_main.jsonl.lock",
            "2026-11-01T05-30-57-396Z_main-1793511057.jsonl",
            "2026-11-01T06-00-00-000Z_job-3-7.jsonl",
            "2026-11-01T06-00-00-000Z_main-x.jsonl",
        ] {
            std::fs::write(real.join(f), "").unwrap();
        }
        let mut ids: Vec<String> = saved_sessions(&dir).into_iter().map(|s| s.0).collect();
        ids.sort();
        assert_eq!(ids, ["main", "main-1793511057"]);
        assert!(saved_sessions(&dir.join("missing")).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_copied_session_names_its_new_id_inside_and_opens_with_the_first_words() {
        let dir = crate::test_dir("rpc-fork");
        let real = dir.join("--zephyr--");
        std::fs::create_dir_all(&real).unwrap();
        // rpi 0.18's layout, cut down: the header names the id, so does the companion.
        let user = r#"{"kind":"entry","type":"message","message":{"kind":"user","role":"user","content":"[tui #3] book a table for four on friday night"}}"#;
        let lane = r#"{"kind":"lane","lane":"main","leafId":"e1"}"#;
        std::fs::write(
            real.join("2026-11-05T04-22-58-894Z_main.jsonl"),
            format!("{{\"kind\":\"header\",\"version\":4,\"id\":\"main\"}}\n{user}\n{lane}\n"),
        )
        .unwrap();
        std::fs::write(
            real.join("2026-11-05T04-22-58-894Z_main.jsonl.progress.jsonl"),
            "{\"format\":\"assistant-progress-v1\",\"session_id\":\"main\",\"state\":{}}\n",
        )
        .unwrap();
        copy_session(&dir, "main", "main-7").unwrap();
        let copy = std::fs::read_to_string(real.join("2026-11-05T04-22-58-894Z_main-7.jsonl"));
        let copy = copy.unwrap();
        let mut lines = copy.lines();
        let head: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(
            (head["id"].as_str(), head["version"].as_i64()),
            (Some("main-7"), Some(4))
        );
        assert_eq!(lines.collect::<Vec<_>>(), [user, lane]);
        let progress = real.join("2026-11-05T04-22-58-894Z_main-7.jsonl.progress.jsonl");
        let progress: Value =
            serde_json::from_str(&std::fs::read_to_string(progress).unwrap()).unwrap();
        assert_eq!(progress["session_id"], "main-7");
        assert_eq!(
            opening(&dir, "main-7").unwrap(),
            "book a table for four on…"
        );
        assert!(
            copy_session(&dir, "main", "main-7").is_err(),
            "never over a session"
        );
        assert!(copy_session(&dir, "main-9", "main-10").is_err());
    }

    #[tokio::test]
    async fn extensions_follow_the_policy_plugin_in_order() {
        let dir = std::env::temp_dir().join(format!(
            "zephyr-rpc-ext-{}-{}",
            std::process::id(),
            crate::unix_millis()
        ));
        let home = dir.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let (policy, a, b) = (dir.join("policy.so"), dir.join("a.so"), dir.join("b.so"));
        for f in [&policy, &a, &b] {
            std::fs::write(f, "").unwrap();
        }
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py");
        let cfg: Config = toml::from_str(&format!(
            "rpi = {fake:?}\nhome = {:?}\npolicy_plugin = {:?}\nextensions = [{:?}, {:?}]\n",
            home.display(),
            policy.display(),
            a.display(),
            b.display(),
        ))
        .unwrap();
        Child::spawn(&cfg, "argv").await.unwrap().stop().await;

        let argv: Vec<String> =
            serde_json::from_str(&std::fs::read_to_string(home.join("argv.json")).unwrap())
                .unwrap();
        for flag in ["--approve", "--no-skills", "--local-only", "snapshot"] {
            assert!(argv.contains(&flag.to_string()), "{flag} in {argv:?}");
        }
        let at = argv.iter().position(|x| x == "--extension").unwrap();
        let want: Vec<String> = [&policy, &a, &b]
            .iter()
            .flat_map(|p| ["--extension".to_string(), p.display().to_string()])
            .collect();
        assert_eq!(argv[at..], want[..]);
    }

    #[tokio::test]
    async fn a_dropped_child_takes_the_tools_it_started_with_it() {
        let home = crate::test_dir("rpc-drop-group");
        std::fs::write(home.join("tool_sleep"), "").unwrap();
        let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py");
        let cfg: Config =
            toml::from_str(&format!("rpi = {fake:?}\nhome = {:?}\n", home.display())).unwrap();
        let child = Child::spawn(&cfg, "task-1").await.unwrap();
        let pid = std::fs::read_to_string(home.join("tool_pid")).unwrap();
        let stat = format!("/proc/{pid}/stat");
        assert!(std::fs::read_to_string(&stat).is_ok_and(|s| !s.contains(") Z ")));
        // As a re-exec drops a task's child. Then gone, or a zombie until reaped: not running.
        drop(child);
        for _ in 0..100 {
            match std::fs::read_to_string(&stat) {
                Ok(s) if !s.contains(") Z ") => {}
                _ => return,
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the tool {pid} is still running");
    }

    #[test]
    fn a_session_follows_only_by_lines_added_after_what_was_seen() {
        let dir = crate::test_dir("rpc-session-print");
        let folder = dir.join("--home--");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("2026-01-01T00-00-00-000Z_main-1.jsonl");
        let none = SessionPrint::now(&dir, "main-1", None);
        assert!(!none.exists() && !none.bad());
        // From no file: rpi makes the first one.
        std::fs::write(&file, "{\"kind\":\"header\"}\n{\"a\":1}\n").unwrap();
        let seen = none.follow(&dir, "main-1").expect("a first file");
        assert_eq!(
            SessionPrint::now(&dir, "main-1", Some(&seen)),
            seen,
            "unchanged"
        );
        assert_eq!(
            seen.follow(&dir, "main-1").as_ref(),
            Some(&seen),
            "unchanged follows"
        );
        let mut text = std::fs::read_to_string(&file).unwrap();
        text.push_str("{\"b\":2}\n");
        std::fs::write(&file, &text).unwrap();
        let grown = seen.follow(&dir, "main-1").expect("lines added");
        assert_eq!(
            SessionPrint::now(&dir, "main-1", Some(&grown)),
            grown,
            "recorded as read"
        );
        // An earlier line changed in place, same length: not a growth.
        std::fs::write(&file, text.replace("\"a\":1", "\"a\":2")).unwrap();
        assert_ne!(
            SessionPrint::now(&dir, "main-1", Some(&grown)),
            grown,
            "hashed again"
        );
        assert_eq!(seen.follow(&dir, "main-1"), None);
        // Cut short.
        std::fs::write(&file, "{\"kind\":\"header\"}\n").unwrap();
        assert_eq!(seen.follow(&dir, "main-1"), None);
        // A second file for the id.
        std::fs::write(&file, &text).unwrap();
        let second = folder.join("2027-01-01T00-00-00-000Z_main-1.jsonl");
        std::fs::write(&second, &text).unwrap();
        assert_eq!(seen.follow(&dir, "main-1"), None);
        assert_eq!(
            none.follow(&dir, "main-1"),
            None,
            "two files where none was"
        );
    }

    #[test]
    fn a_session_file_that_is_a_link_or_unreadable_is_never_followed() {
        use std::os::unix::fs::symlink;
        let dir = crate::test_dir("rpc-session-bad");
        let folder = dir.join("--home--");
        std::fs::create_dir_all(&folder).unwrap();
        let none = SessionPrint::now(&dir, "main-1", None);
        std::fs::write(dir.join("elsewhere.jsonl"), "{\"kind\":\"header\"}\n").unwrap();
        let file = folder.join("2026-01-01T00-00-00-000Z_main-1.jsonl");
        symlink(dir.join("elsewhere.jsonl"), &file).unwrap();
        assert!(SessionPrint::now(&dir, "main-1", None).bad());
        assert_eq!(none.follow(&dir, "main-1"), None, "a link where none was");
        std::fs::remove_file(&file).unwrap();
        std::fs::create_dir(&file).unwrap();
        assert!(SessionPrint::now(&dir, "main-1", None).bad(), "a folder");
        assert_eq!(none.follow(&dir, "main-1"), None);
        std::fs::remove_dir(&file).unwrap();
        // The companion counts too.
        std::fs::write(&file, "{\"kind\":\"header\"}\n").unwrap();
        let seen = none.follow(&dir, "main-1").unwrap();
        let progress = folder.join("2026-01-01T00-00-00-000Z_main-1.jsonl.progress.jsonl");
        symlink(dir.join("elsewhere.jsonl"), &progress).unwrap();
        assert!(SessionPrint::now(&dir, "main-1", Some(&seen)).bad());
        assert_eq!(seen.follow(&dir, "main-1"), None);
    }
}
