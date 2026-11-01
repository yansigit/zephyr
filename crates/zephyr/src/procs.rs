//! The main agent's `exec` and `process` tools (DESIGN §4.13). rpi's own `bash` cannot be moved
//! to the background, so the main child runs commands through `exec` instead: the daemon starts
//! each one as the agent and answers the call when it ends, or when it yields. It yields when a
//! message for main arrives (the owner, or a report), at once for `background: true`, or at the
//! `exec_backstop`; then main can answer while it keeps running, and its end comes back to main
//! as an inbox row, as a task's report does.
//!
//! A command outlives a re-exec of the daemon: `exec()` keeps the pid, so it is still our child.
//! It outlives a daemon crash too: `zephyr supervise` starts the daemon again, and the command,
//! orphaned, is kept and reaped by the container's init. The new daemon adopts it
//! (`Procs::adopt`) only if its pid still has the kernel start time it was spawned with; anything
//! else is reported `unknown` and never signalled (Hermes's rule). A command it did not start is
//! not its child, so its exit code comes from the file the wrapper writes (`WRAPPER`).

use crate::config::Config;
use crate::store::{Proc, Store};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use zephyr_policy::tools::{ExecCall, ProcessAction};

/// The default `timeout` of an `exec`, in seconds.
const DEFAULT_TIMEOUT: u64 = 30 * 60;
/// What an `exec` answer carries of the output: rpi's `bash` limits (`rpi-tools` truncate.rs).
const MAX_LINES: usize = 2000;
const MAX_BYTES: u64 = 50 * 1024;
/// What a report row carries into main, like a task's report (`tasks::MAX_REPORT`).
const REPORT_BYTES: u64 = 8 * 1024;

/// The running commands. Cloned into each command's own task.
#[derive(Clone, Default)]
pub struct Procs {
    running: Arc<Mutex<HashMap<i64, Handle>>>,
    /// A command that moved to the background wrote its report: the router dispatches it.
    pub reported: Arc<Notify>,
}

#[derive(Clone)]
struct Handle {
    pgid: i32,
    /// Still in the agent's turn: its tool call waits for it.
    foreground: Arc<AtomicBool>,
    yield_now: Arc<Notify>,
    stopped: Arc<AtomicBool>,
}

/// `(tool call id, args)` when `ev` starts a call of `tool` (from rpi's stream, not the agent).
pub fn call<'a>(ev: &'a Value, tool: &str) -> Option<(&'a str, &'a Value)> {
    if ev["type"] != "tool_execution_start" || ev["toolName"] != tool {
        return None;
    }
    Some((ev["toolCallId"].as_str()?, &ev["args"]))
}

impl Procs {
    /// Starts main's `exec` call `call_id`, reporting to `to` (channel, sender). The call is
    /// answered from the command's own task; a refusal is answered here.
    pub fn start(
        &self,
        cfg: &Arc<Config>,
        store: &Store,
        call_id: &str,
        call: ExecCall,
        to: (String, String),
    ) {
        let answer = |text: &str| crate::tools::answer_gateway(cfg, call_id, text);
        let p = match store.add_proc(&call.command, &to.0, &to.1) {
            Ok(Some(p)) => p,
            Ok(None) => {
                return answer(&format!(
                    "not started: {} commands are running; stop one with `process stop`",
                    crate::store::MAX_RUNNING_PROCS
                ))
            }
            Err(e) => return answer(&format!("not started: {e}")),
        };
        let log = log_path(cfg, p.id);
        let child = match spawn(cfg, &call.command, &log) {
            Ok(c) => c,
            Err(e) => {
                let text = format!("could not start: {e:#}");
                if let Ok(mut s) = Store::open(&cfg.db_path()) {
                    let _ = s.finish_proc(&p, "exited", None, None, crate::now());
                }
                return answer(&text);
            }
        };
        let pid = child.id().unwrap_or(0) as i32;
        let timeout = Duration::from_secs(call.timeout.unwrap_or(DEFAULT_TIMEOUT));
        if let Some(ticks) = start_ticks(pid) {
            let deadline = crate::now() + timeout.as_secs() as i64;
            if let Err(e) = store.set_proc_pid(p.id, pid, ticks, deadline) {
                eprintln!("zephyr: recording process #{}'s pid failed: {e}", p.id);
            }
        }
        let handle = Handle {
            pgid: pid,
            foreground: Arc::new(AtomicBool::new(true)),
            yield_now: Arc::new(Notify::new()),
            stopped: Arc::new(AtomicBool::new(false)),
        };
        self.running.lock().unwrap().insert(p.id, handle.clone());
        tokio::spawn(run(
            self.clone(),
            cfg.clone(),
            p,
            call_id.to_string(),
            child,
            handle,
            timeout,
            call.background,
        ));
    }

    /// A message for main arrived: every command still in the agent's turn moves to the
    /// background, so the turn can go on to read it.
    pub fn yield_all(&self) {
        for h in self.running.lock().unwrap().values() {
            if h.foreground.load(Ordering::SeqCst) {
                h.yield_now.notify_one();
            }
        }
    }

    /// Kills command `id` and whatever it started (its process group).
    pub fn stop(&self, id: i64) -> String {
        match self.running.lock().unwrap().get(&id) {
            Some(h) => {
                kill(h);
                format!("[zephyr] stopping process #{id}")
            }
            None => format!("[zephyr] process #{id} is not running"),
        }
    }

    /// `/stop`, `/pause` and shutdown: kills every command. Returns how many.
    pub fn stop_all(&self) -> usize {
        let running = self.running.lock().unwrap();
        running.values().for_each(kill);
        running.len()
    }

    /// Startup: takes over the commands a re-exec handed over alive, and reports the rest
    /// `unknown`. A pid whose start time no longer matches was reused by something else: it is
    /// neither adopted nor signalled.
    pub fn adopt(&self, cfg: &Arc<Config>, store: &mut Store) -> rusqlite::Result<()> {
        let mut kept = Vec::new();
        for (p, pid, ticks, deadline) in store.running_procs()? {
            let (Some(pid), Some(ticks)) = (pid, ticks) else {
                continue;
            };
            if pid <= 0 {
                continue;
            }
            // The pidfd first: it pins whichever process holds the pid now, so the start-time
            // check below is about that process and the pid cannot be reused in between.
            // SAFETY: a plain syscall; the fd it returns is ours to own.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
            let fd = (fd >= 0).then(|| {
                // SAFETY: a fresh fd from pidfd_open, owned by nothing else.
                unsafe { OwnedFd::from_raw_fd(fd as i32) }
            });
            let Some(fd) = fd.filter(|_| start_ticks(pid) == Some(ticks)) else {
                // Gone, or reused by something else: dropping the fd neither adopts nor signals it.
                // If the wrapper recorded an exit, it ended while no daemon watched: report it.
                if let Some(code) = recorded_exit(&log_path(cfg, p.id)) {
                    let ran = Duration::from_secs((crate::now() - p.created_at).max(0) as u64);
                    record(
                        store,
                        cfg,
                        &p,
                        "exited",
                        Some(code),
                        format!("exited {code}"),
                        ran,
                    );
                    self.reported.notify_one();
                }
                continue;
            };
            let h = Handle {
                pgid: pid,
                foreground: Arc::new(AtomicBool::new(false)),
                yield_now: Arc::new(Notify::new()),
                stopped: Arc::new(AtomicBool::new(false)),
            };
            let left = deadline.map_or(DEFAULT_TIMEOUT as i64, |d| d - crate::now());
            let left = Duration::from_secs(left.max(0) as u64);
            self.running.lock().unwrap().insert(p.id, h.clone());
            kept.push(p.id);
            tokio::spawn(watch(self.clone(), cfg.clone(), p, fd, h, left));
        }
        store.interrupt_running_procs(crate::now(), &kept)?;
        Ok(())
    }

    /// The agent's `process` call and the owner's `/process`.
    pub fn reply(&self, cfg: &Config, store: &Store, action: ProcessAction) -> String {
        match action {
            ProcessAction::List => list(store),
            ProcessAction::Stop(id) => self.stop(id),
            ProcessAction::Log(id) => match tail(&log_path(cfg, id), MAX_BYTES) {
                Ok(t) => t.text(id),
                Err(e) => format!("[zephyr] no log for process #{id}: {e}"),
            },
        }
    }
}

fn kill(h: &Handle) {
    h.stopped.store(true, Ordering::SeqCst);
    if h.pgid > 0 {
        // SAFETY: a plain syscall; a negative pid names the process group the command leads.
        unsafe { libc::kill(-h.pgid, libc::SIGKILL) };
    }
}

fn log_path(cfg: &Config, id: i64) -> PathBuf {
    cfg.home.join(".procs").join(format!("{id}.log"))
}

/// Runs `$0` (the command) and records its exit code in `$1` once it ends, so a daemon that is not
/// its parent (after a crash) still learns it. Both are arguments, never spliced into the script.
/// The outer `bash` stays the process-group leader and keeps the command's exit code.
const WRAPPER: &str =
    r#"bash -c "$0"; s=$?; echo "$s" > "$1.tmp" && mv -f "$1.tmp" "$1"; exit "$s""#;

/// The exit code the wrapper recorded for the command logging to `log`, read as the agent.
fn recorded_exit(log: &Path) -> Option<i32> {
    let _fs = crate::agent::fs();
    std::fs::read_to_string(log.with_extension("exit"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// The command through `WRAPPER`, as the agent, in its home, leading its own process group, with
/// stdout and stderr in `log` and its exit code recorded next to it.
fn spawn(cfg: &Config, command: &str, log: &Path) -> anyhow::Result<tokio::process::Child> {
    let exit = log.with_extension("exit");
    let file = {
        // As the agent: the log is the agent's file, and a symlink it planted leads nowhere new.
        let _fs = crate::agent::fs();
        std::fs::create_dir_all(log.parent().expect("a dir"))?;
        let _ = std::fs::remove_file(&exit);
        std::fs::File::create(log)?
    };
    let mut cmd = tokio::process::Command::new("bash");
    cmd.args(["-c", WRAPPER, command])
        .arg(&exit)
        .current_dir(&cfg.home)
        .stdin(std::process::Stdio::null())
        .stdout(file.try_clone()?)
        .stderr(file);
    // No kill_on_drop: a command outlives a re-exec of the daemon, which adopts it (`adopt`).
    crate::agent::cmd(cmd.as_std_mut());
    cmd.as_std_mut().process_group(0);
    if let Some(display) = crate::desktop::ensure() {
        cmd.env("DISPLAY", display);
        // The desktop takes only clients holding its cookie (`desktop` module docs).
        if let Some(auth) = crate::desktop::xauthority() {
            cmd.env("XAUTHORITY", auth);
        }
    }
    Ok(cmd.spawn()?)
}

/// How the turn's part of a command ended.
enum First {
    Exited(std::io::Result<std::process::ExitStatus>),
    Yield(String),
    Timeout,
}

#[allow(clippy::too_many_arguments)]
async fn run(
    procs: Procs,
    cfg: Arc<Config>,
    p: Proc,
    call_id: String,
    mut child: tokio::process::Child,
    h: Handle,
    timeout: Duration,
    background: bool,
) {
    let started = Instant::now();
    let log = log_path(&cfg, p.id);
    let answer = |text: String| crate::tools::answer_gateway(&cfg, &call_id, &text);
    let output = |max| match tail(&log, max) {
        Ok(t) => t.text(p.id),
        Err(e) => format!("[output unreadable: {e}]"),
    };
    let first = if background {
        First::Yield("you asked for the background".into())
    } else {
        tokio::select! {
            s = child.wait() => First::Exited(s),
            _ = h.yield_now.notified() => First::Yield("a message for you arrived".into()),
            _ = tokio::time::sleep(cfg.exec_backstop) => {
                First::Yield(format!("it ran past {}", human(cfg.exec_backstop)))
            }
            _ = tokio::time::sleep(timeout) => First::Timeout,
        }
    };
    let mut store = Store::open(&cfg.db_path());
    let mut finish =
        |status: &str, code: Option<i32>, report: Option<(&str, &str)>| match &mut store {
            Ok(s) => {
                if let Err(e) = s.finish_proc(&p, status, code, report, crate::now()) {
                    eprintln!("zephyr: recording process #{} failed: {e}", p.id);
                }
            }
            Err(e) => eprintln!("zephyr: process #{} could not open the store: {e}", p.id),
        };
    match first {
        First::Exited(s) => {
            let (status, said) = ended(&s, &h);
            answer(format!("{said}\n{}", output(MAX_BYTES)));
            finish(status, s.ok().and_then(|s| s.code()), None);
        }
        First::Timeout => {
            kill(&h);
            let _ = child.wait().await;
            answer(format!(
                "killed after its timeout of {}\n{}",
                human(timeout),
                output(MAX_BYTES)
            ));
            finish("timeout", None, None);
        }
        First::Yield(why) => {
            h.foreground.store(false, Ordering::SeqCst);
            answer(format!(
                "Moved to the background ({why}) without being killed: it is still running as \
                 process #{}. You get a message when it exits. Reply to the owner now and end your \
                 turn; `process` lists, shows or stops it.\nOutput so far:\n{}",
                p.id,
                output(MAX_BYTES)
            ));
            let left = timeout.saturating_sub(started.elapsed());
            let (status, said, code) = tokio::select! {
                s = child.wait() => {
                    let (status, said) = ended(&s, &h);
                    (status, said, s.ok().and_then(|s| s.code()))
                }
                _ = tokio::time::sleep(left) => {
                    kill(&h);
                    let _ = child.wait().await;
                    ("timeout", format!("timed out after {}", human(timeout)), None)
                }
            };
            let outcome = format!("{said} after {}", human(started.elapsed()));
            finish(status, code, Some((&outcome, &output(REPORT_BYTES))));
            procs.reported.notify_one();
        }
    }
    procs.running.lock().unwrap().remove(&p.id);
}

/// Follows an adopted command to its end (or its timeout) and reports it, as `run` does for one
/// that moved to the background. The new image is still its parent, so `waitid` reaps it.
async fn watch(procs: Procs, cfg: Arc<Config>, p: Proc, fd: OwnedFd, h: Handle, left: Duration) {
    use tokio::io::{unix::AsyncFd, Interest};
    // SAFETY: an OwnedFd stays open and names the same pidfd until the AsyncFd drops it.
    let registered = unsafe { AsyncFd::register_with_interest(fd, Interest::READABLE) };
    let (status, said, code) = match registered {
        Ok(fd) => {
            let timed_out = tokio::select! {
                _ = fd.readable() => false,
                _ = tokio::time::sleep(left) => {
                    kill(&h);
                    let _ = fd.readable().await;
                    true
                }
            };
            // After a re-exec we are its parent; after a crash we are not, and the wrapper's
            // record says how it ended.
            let s = match reap(fd.get_ref()) {
                Err(e) if e.raw_os_error() == Some(libc::ECHILD) => {
                    recorded_exit(&log_path(&cfg, p.id))
                        .map(|c| std::process::ExitStatus::from_raw((c & 0xff) << 8))
                        .ok_or(e)
                }
                s => s,
            };
            match timed_out {
                true => ("timeout", "timed out".to_string(), None),
                false => {
                    let (status, said) = ended(&s, &h);
                    (status, said, s.ok().and_then(|s| s.code()))
                }
            }
        }
        Err(e) => {
            kill(&h);
            (
                "unknown",
                format!("could not be followed after a restart: {e}"),
                None,
            )
        }
    };
    let ran = Duration::from_secs((crate::now() - p.created_at).max(0) as u64);
    match Store::open(&cfg.db_path()) {
        Ok(mut s) => record(&mut s, &cfg, &p, status, code, said, ran),
        Err(e) => eprintln!("zephyr: process #{} could not open the store: {e}", p.id),
    }
    procs.reported.notify_one();
    procs.running.lock().unwrap().remove(&p.id);
}

/// Closes an adopted command's row with its report for main.
fn record(
    store: &mut Store,
    cfg: &Config,
    p: &Proc,
    status: &str,
    code: Option<i32>,
    said: String,
    ran: Duration,
) {
    let outcome = format!("{said} after {}", human(ran));
    let output = match tail(&log_path(cfg, p.id), REPORT_BYTES) {
        Ok(t) => t.text(p.id),
        Err(e) => format!("[output unreadable: {e}]"),
    };
    if let Err(e) = store.finish_proc(p, status, code, Some((&outcome, &output)), crate::now()) {
        eprintln!("zephyr: recording process #{} failed: {e}", p.id);
    }
}

/// Reaps the child a pidfd names and gives its exit status.
fn reap(fd: &OwnedFd) -> std::io::Result<std::process::ExitStatus> {
    // SAFETY: zeroed siginfo is valid for waitid to fill; the fd is a live pidfd.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let r = unsafe {
        libc::waitid(
            libc::P_PIDFD,
            fd.as_raw_fd() as libc::id_t,
            &mut info,
            libc::WEXITED,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: waitid succeeded, so si_status is set.
    let status = unsafe { info.si_status() };
    Ok(match info.si_code {
        libc::CLD_EXITED => std::process::ExitStatus::from_raw((status & 0xff) << 8),
        _ => std::process::ExitStatus::from_raw(status & 0x7f),
    })
}

/// The kernel start time of `pid` (field 22 of `/proc/<pid>/stat`, in clock ticks since boot),
/// which tells a command from whatever later reuses its pid. After the last `)`, since the
/// command name in field 2 may hold spaces and parentheses.
fn start_ticks(pid: i32) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

/// `(status, what to say)` for a command that ended.
fn ended(s: &std::io::Result<std::process::ExitStatus>, h: &Handle) -> (&'static str, String) {
    match s {
        _ if h.stopped.load(Ordering::SeqCst) => ("stopped", "was stopped".into()),
        Ok(s) => match (s.code(), s.signal()) {
            (Some(c), _) => ("exited", format!("exited {c}")),
            (None, Some(sig)) => ("exited", format!("killed by signal {sig}")),
            _ => ("exited", "ended".into()),
        },
        // Not ours to reap and nothing recorded: the wrapper itself was killed.
        Err(_) => ("exited", "ended; its exit code was not recorded".into()),
    }
}

fn human(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        _ => format!("{}m{:02}s", s / 60, s % 60),
    }
}

/// The end of a log, read as the agent.
struct Tail {
    text: String,
    cut: bool,
}

impl Tail {
    fn text(self, id: i64) -> String {
        if self.cut {
            format!(
                "{}\n[earlier output cut; the whole log is .procs/{id}.log]",
                self.text
            )
        } else {
            self.text
        }
    }
}

/// The last `max` bytes and [`MAX_LINES`] lines of `path`.
fn tail(path: &Path, max: u64) -> std::io::Result<Tail> {
    let _fs = crate::agent::fs();
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let start = len.saturating_sub(max);
    f.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    f.read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    // A cut start lands mid-line: drop that partial line.
    let text = match (start > 0, text.find('\n')) {
        (true, Some(i)) => &text[i + 1..],
        _ => &text[..],
    };
    let lines: Vec<&str> = text.lines().collect();
    let keep = lines.len().saturating_sub(MAX_LINES);
    Ok(Tail {
        text: lines[keep..].join("\n"),
        cut: start > 0 || keep > 0,
    })
}

/// `/process list` and the agent's `process list`.
fn list(store: &Store) -> String {
    let procs = match store.procs(5) {
        Ok(p) => p,
        Err(e) => return format!("[zephyr] listing processes failed: {e}"),
    };
    if procs.is_empty() {
        return "[zephyr] no processes".into();
    }
    let now = crate::now();
    let lines: Vec<String> = procs
        .iter()
        .map(|p| {
            let command: String = p.command.split_whitespace().collect::<Vec<_>>().join(" ");
            let short: String = command.chars().take(60).collect();
            let cut = if short.len() < command.len() {
                "…"
            } else {
                ""
            };
            let age = (now - p.created_at).max(0) / 60;
            format!("#{} {} ({age} min ago): {short}{cut}", p.id, p.status)
        })
        .collect();
    format!("[zephyr] processes:\n{}", lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command started the way `spawn` starts one, recorded the way `start` records it.
    fn spawned(
        cfg: &Config,
        store: &Store,
        script: &str,
        skew: i64,
    ) -> (Proc, std::process::Child) {
        let p = store.add_proc(script, "chat", "o").unwrap().unwrap();
        let log = log_path(cfg, p.id);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        let file = std::fs::File::create(&log).unwrap();
        let child = std::process::Command::new("bash")
            .args(["-c", WRAPPER, script])
            .arg(log.with_extension("exit"))
            .stdout(file)
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        let ticks = start_ticks(pid).unwrap() + skew;
        store
            .set_proc_pid(p.id, pid, ticks, crate::now() + 60)
            .unwrap();
        (p, child)
    }

    fn setup(name: &str) -> (Arc<Config>, Store) {
        let home = crate::test_dir(name);
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        (Arc::new(cfg), store)
    }

    #[tokio::test]
    async fn a_command_handed_over_by_a_reexec_is_adopted_and_reported() {
        let (cfg, mut store) = setup("procs-adopt");
        // Not waited for here: like a re-exec's new image, only `adopt` reaps it.
        let (p, _child) = spawned(&cfg, &store, "sleep 1; echo survived", 0);
        let procs = Procs::default();
        procs.adopt(&cfg, &mut store).unwrap();
        assert!(procs.running.lock().unwrap().contains_key(&p.id));
        assert!(procs
            .reply(&cfg, &store, ProcessAction::List)
            .contains("running"));
        tokio::time::timeout(Duration::from_secs(10), procs.reported.notified())
            .await
            .expect("no report");
        let rows = store.inbox_with_status("new").unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0]
                .text
                .contains(&format!("[process #{} exited 0 after", p.id)),
            "{rows:?}"
        );
        assert!(rows[0].text.contains("> survived"), "{rows:?}");
        assert_eq!(store.procs(5).unwrap()[0].status, "exited");
        assert!(procs.running.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_reused_pid_is_neither_adopted_nor_signalled() {
        let (cfg, mut store) = setup("procs-reused");
        // The recorded start time is off by one tick: the pid now names something else.
        let (p, mut child) = spawned(&cfg, &store, "sleep 30", 1);
        let procs = Procs::default();
        procs.adopt(&cfg, &mut store).unwrap();
        assert!(procs.running.lock().unwrap().is_empty());
        assert_eq!(store.procs(5).unwrap()[0].status, "unknown");
        let rows = store.inbox_with_status("new").unwrap();
        assert!(
            rows[0]
                .text
                .contains(&format!("[process #{} unknown", p.id)),
            "{rows:?}"
        );
        procs.stop_all();
        assert!(
            child.try_wait().unwrap().is_none(),
            "the unrelated process was signalled"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }

    #[test]
    fn the_wrapper_records_the_exit_code_and_keeps_it() {
        let dir = crate::test_dir("procs-wrapper");
        let run = |script: &str, n: u32| {
            let log = dir.join(format!("{n}.log"));
            let mut child = std::process::Command::new("bash")
                .args(["-c", WRAPPER, script])
                .arg(log.with_extension("exit"))
                .process_group(0)
                .spawn()
                .unwrap();
            (child.wait().unwrap(), recorded_exit(&log), child)
        };
        let (s, rec, _) = run("true", 1);
        assert_eq!((s.code(), rec), (Some(0), Some(0)));
        // Quotes and `$` in the command reach the inner shell untouched.
        let (s, rec, _) = run("v='a \"b\" $c'; [ \"$v\" = 'a \"b\" $c' ] && exit 3", 2);
        assert_eq!((s.code(), rec), (Some(3), Some(3)));
        // A killed group writes nothing: the daemon knows it stopped it.
        let log = dir.join("3.log");
        let mut child = std::process::Command::new("bash")
            .args(["-c", WRAPPER, "sleep 30"])
            .arg(log.with_extension("exit"))
            .process_group(0)
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: a plain syscall on the group this test just made.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        child.wait().unwrap();
        assert_eq!(recorded_exit(&log), None);
    }

    #[tokio::test]
    async fn an_orphan_left_by_a_crash_is_adopted_and_its_recorded_exit_reported() {
        let (cfg, mut store) = setup("procs-orphan");
        let p = store
            .add_proc("sleep 1; echo orphan", "chat", "o")
            .unwrap()
            .unwrap();
        let log = log_path(&cfg, p.id);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        // Started by a shell that exits at once, as a crashed daemon's command is left: not
        // this process's child, so only the wrapper's record can tell its exit code.
        let out = std::process::Command::new("bash")
            .args([
                "-c",
                r#"setsid bash -c "$0" "$1" "$2" > "$3" 2>&1 & echo $!"#,
                WRAPPER,
                "sleep 1; echo orphan",
            ])
            .arg(log.with_extension("exit"))
            .arg(&log)
            .output()
            .unwrap();
        let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        // SAFETY: a plain syscall; WNOHANG never blocks. ECHILD: not ours to reap.
        let r = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        assert_eq!(
            (r, std::io::Error::last_os_error().raw_os_error()),
            (-1, Some(libc::ECHILD)),
            "the test must not be its parent"
        );
        let ticks = start_ticks(pid).unwrap();
        store
            .set_proc_pid(p.id, pid, ticks, crate::now() + 60)
            .unwrap();
        let procs = Procs::default();
        procs.adopt(&cfg, &mut store).unwrap();
        assert!(procs.running.lock().unwrap().contains_key(&p.id));
        tokio::time::timeout(Duration::from_secs(10), procs.reported.notified())
            .await
            .expect("no report");
        let rows = store.inbox_with_status("new").unwrap();
        assert!(
            rows[0]
                .text
                .contains(&format!("[process #{} exited 0 after", p.id)),
            "{rows:?}"
        );
        assert!(rows[0].text.contains("> orphan"), "{rows:?}");
    }

    #[test]
    fn a_command_that_ended_while_no_daemon_watched_is_reported_from_its_record() {
        let (cfg, mut store) = setup("procs-gone");
        let p = store.add_proc("make", "chat", "o").unwrap().unwrap();
        let log = log_path(&cfg, p.id);
        std::fs::create_dir_all(log.parent().unwrap()).unwrap();
        std::fs::write(&log, "built\n").unwrap();
        std::fs::write(log.with_extension("exit"), "4\n").unwrap();
        // A pid that is surely gone, with a start time nothing has.
        store
            .set_proc_pid(p.id, i32::MAX - 1, 1, crate::now() + 60)
            .unwrap();
        let procs = Procs::default();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async { procs.adopt(&cfg, &mut store).unwrap() });
        assert!(procs.running.lock().unwrap().is_empty());
        assert_eq!(store.procs(5).unwrap()[0].status, "exited");
        let rows = store.inbox_with_status("new").unwrap();
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0]
                .text
                .contains(&format!("[process #{} exited 4 after", p.id)),
            "{rows:?}"
        );
        assert!(rows[0].text.contains("> built"), "{rows:?}");
    }

    #[test]
    fn start_ticks_reads_field_22_past_the_command_name() {
        let me = start_ticks(std::process::id() as i32).unwrap();
        assert!(me > 0);
        assert_eq!(
            start_ticks(std::process::id() as i32),
            Some(me),
            "stable for one process"
        );
        assert_eq!(start_ticks(i32::MAX), None);
    }

    #[test]
    fn tail_keeps_the_end_on_a_line_boundary() {
        let dir = crate::test_dir("procs-tail");
        let path = dir.join("1.log");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let t = tail(&path, 1024).unwrap();
        assert_eq!((t.text.as_str(), t.cut), ("first\nsecond\nthird", false));
        let t = tail(&path, 9).unwrap();
        assert_eq!(
            (t.text.as_str(), t.cut),
            ("third", true),
            "the partial line is dropped"
        );
        let many: String = (0..MAX_LINES + 5).map(|i| format!("{i}\n")).collect();
        std::fs::write(&path, many).unwrap();
        let t = tail(&path, MAX_BYTES).unwrap();
        assert!(t.cut && t.text.starts_with("5\n") && t.text.lines().count() == MAX_LINES);
        assert!(t.text(1).ends_with("the whole log is .procs/1.log]"));
    }
}
