//! `zephyr supervise`, the image's command (DESIGN §4.4): `docker-init` (PID 1, from `--init`)
//! runs it, and it runs `zephyr run`. When the daemon crashes it is started again, without the
//! container restarting, so the `exec` commands it started live on: orphaned, they are kept and
//! reaped by PID 1, and the new daemon adopts them (`procs::Procs::adopt`). SIGTERM and SIGINT
//! are passed to the daemon, and then nothing is restarted, so `docker stop` stays a clean stop.
//! A daemon that hangs instead of crashing stops rewriting `<state>/heartbeat` (each router tick):
//! once it is `stale`, its process group is killed and it is started again, as after a crash.

use std::collections::VecDeque;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// When to start the daemon again.
pub struct Policy {
    /// The first wait after a crash; it doubles up to `max`.
    pub first: Duration,
    pub max: Duration,
    /// A run this long resets the wait to `first`.
    pub healthy: Duration,
    /// `max_exits` crashes within `window`: give up, and let Docker's restart policy take over.
    pub window: Duration,
    pub max_exits: usize,
    /// The daemon's heartbeat file; `None` checks nothing.
    pub heartbeat: Option<PathBuf>,
    /// How often it is checked, how long after a start it is first, and the age that is a hang.
    pub check: Duration,
    pub grace: Duration,
    pub stale: Duration,
    /// Loads the agent's network floor before the first start (DESIGN §4.20); `None` outside
    /// bridge mode. When it fails, the daemon is never started.
    pub floor: Option<fn() -> anyhow::Result<()>>,
}

/// The file in the state dir the router rewrites with the unix time on each tick.
pub const HEARTBEAT: &str = "heartbeat";
/// A heartbeat this old means the daemon hangs (`supervise`, `zephyr status`). Well past the
/// longest a tick may legitimately wait: an auth check is up to 60 s per account, one after another.
pub const STALE: Duration = Duration::from_secs(300);

/// Seconds since the daemon last wrote `path`; `None` when it is missing or unreadable.
pub fn heartbeat_age(path: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(path).ok()?;
    let at: i64 = text.split_whitespace().next()?.parse().ok()?;
    Some((crate::now() - at).max(0) as u64)
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            first: Duration::from_secs(1),
            max: Duration::from_secs(30),
            healthy: Duration::from_secs(60),
            window: Duration::from_secs(120),
            max_exits: 5,
            heartbeat: None,
            check: Duration::from_secs(15),
            grace: Duration::from_secs(120),
            stale: STALE,
            floor: None,
        }
    }
}

/// The image's command: the daemon (`<this binary> run`), started again when it crashes.
pub async fn main() -> ExitCode {
    let (tx, rx) = mpsc::channel(4);
    for kind in [
        tokio::signal::unix::SignalKind::terminate(),
        tokio::signal::unix::SignalKind::interrupt(),
    ] {
        let mut sig = match tokio::signal::unix::signal(kind) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("zephyr: supervise: signal handler: {e}");
                return ExitCode::FAILURE;
            }
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            while sig.recv().await.is_some() {
                let _ = tx.send(kind.as_raw_value()).await;
            }
        });
    }
    // Its own process group, so a hung one is killed with whatever it started in it.
    let bridge = crate::egress::bridge();
    let daemon = move || {
        let mut cmd = tokio::process::Command::new(std::env::current_exe()?);
        cmd.as_std_mut().process_group(0);
        if bridge {
            // The daemon, and all it starts, cannot change the rules loaded below: NET_ADMIN
            // leaves the bounding set of the process that becomes it (the daemon checks).
            // SAFETY: one prctl syscall between fork and exec.
            unsafe { cmd.pre_exec(crate::egress::drop_net_admin) };
        }
        cmd.arg("run").spawn()
    };
    if bridge {
        // The agent's direct routes follow `network.allow`; only this process may change them.
        tokio::spawn(crate::egress::keep_allow(
            crate::config::Config::default_path(),
        ));
    }
    // Read once: with no readable config the daemon fails at start anyway.
    let config = crate::config::Config::load(&crate::config::Config::default_path());
    let policy = Policy {
        heartbeat: config.ok().map(|c| c.state_dir().join(HEARTBEAT)),
        floor: bridge.then_some(crate::egress::load_floor),
        ..Policy::default()
    };
    ExitCode::from(run(&policy, daemon, rx).await)
}

/// Runs what `spawn` starts until a signal from `signals` ends it (passed on, never restarted)
/// or it crashes `max_exits` times within `window`. Returns the exit code to leave with.
pub async fn run(
    policy: &Policy,
    mut spawn: impl FnMut() -> std::io::Result<tokio::process::Child>,
    mut signals: mpsc::Receiver<i32>,
) -> u8 {
    if let Some(Err(e)) = policy.floor.map(|load| load()) {
        eprintln!(
            "zephyr: network floor could not be loaded: {e:#}; network = \"host\" in host.toml runs without it"
        );
        return 1;
    }
    let mut wait = policy.first;
    let mut exits = VecDeque::new();
    loop {
        let mut child = match spawn() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("zephyr: supervise: could not start the daemon: {e}");
                return 1;
            }
        };
        let started = Instant::now();
        let (mut stopping, mut killed) = (false, false);
        let mut check = tokio::time::interval(policy.check);
        let status = loop {
            tokio::select! {
                s = child.wait() => break s,
                Some(sig) = signals.recv() => {
                    stopping = true;
                    if let Some(pid) = child.id() {
                        // SAFETY: a plain syscall to our own child.
                        unsafe { libc::kill(pid as i32, sig) };
                    }
                }
                _ = check.tick(), if policy.heartbeat.is_some() && !stopping && !killed => {
                    let age = policy.heartbeat.as_deref().and_then(heartbeat_age);
                    let Some(age) = age.filter(|a| *a >= policy.stale.as_secs()) else {
                        continue;
                    };
                    if started.elapsed() < policy.grace {
                        continue;
                    }
                    eprintln!("zephyr: the daemon's heartbeat is {age}s old: it hangs; killing it");
                    if let Some(pid) = child.id() {
                        // SAFETY: plain syscalls to our own child's group, or to it alone when
                        // it leads none.
                        if unsafe { libc::kill(-(pid as i32), libc::SIGKILL) } != 0 {
                            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
                        }
                    }
                    killed = true;
                }
            }
        };
        let code = match &status {
            Ok(s) => s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0)) as u8,
            Err(_) => 1,
        };
        if stopping {
            return code;
        }
        let now = Instant::now();
        if now - started >= policy.healthy {
            wait = policy.first;
        }
        exits.push_back(now);
        while exits.front().is_some_and(|t| now - *t > policy.window) {
            exits.pop_front();
        }
        let how = match &status {
            Ok(s) => s.to_string(),
            Err(e) => e.to_string(),
        };
        if exits.len() >= policy.max_exits {
            eprintln!(
                "zephyr: the daemon exited ({how}) {} times within {}s; giving up",
                exits.len(),
                policy.window.as_secs()
            );
            return code.max(1);
        }
        eprintln!(
            "zephyr: the daemon exited ({how}); starting it again in {}s",
            wait.as_secs_f32()
        );
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            Some(_) = signals.recv() => return 0, // asked to stop while nothing runs
        }
        wait = (wait * 2).min(policy.max);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn quick() -> Policy {
        Policy {
            first: Duration::from_millis(10),
            max: Duration::from_millis(40),
            healthy: Duration::from_secs(60),
            window: Duration::from_secs(60),
            max_exits: 3,
            ..Policy::default()
        }
    }

    /// `sh -c script`, counting starts.
    fn counted(
        script: &str,
    ) -> (
        Arc<AtomicUsize>,
        impl FnMut() -> std::io::Result<tokio::process::Child>,
    ) {
        let n = Arc::new(AtomicUsize::new(0));
        let starts = n.clone();
        let script = script.to_string();
        (n, move || {
            starts.fetch_add(1, Ordering::SeqCst);
            let mut cmd = tokio::process::Command::new("sh");
            cmd.as_std_mut().process_group(0);
            cmd.args(["-c", &script]).spawn()
        })
    }

    #[tokio::test]
    async fn a_floor_that_fails_to_load_never_starts_the_daemon() {
        let (n, spawn) = counted("exec sleep 30");
        let (_tx, rx) = mpsc::channel(1);
        let policy = Policy {
            floor: Some(|| anyhow::bail!("nft: no")),
            ..quick()
        };
        assert_eq!(run(&policy, spawn, rx).await, 1);
        assert_eq!(n.load(Ordering::SeqCst), 0, "the daemon was started");
    }

    #[tokio::test]
    async fn a_daemon_that_keeps_crashing_is_given_up_on() {
        let (n, spawn) = counted("exit 3");
        let (_tx, rx) = mpsc::channel(1);
        assert_eq!(run(&quick(), spawn, rx).await, 3);
        assert_eq!(
            n.load(Ordering::SeqCst),
            3,
            "max_exits crashes, then no more"
        );
    }

    #[tokio::test]
    async fn a_crash_starts_it_again_and_a_signal_ends_it_without_a_restart() {
        let dir = crate::test_dir("supervise-restart");
        let count = dir.join("n");
        // Crashes twice, then stays up until signalled.
        let script = format!(
            "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; [ $n -lt 3 ] && exit 3; exec sleep 30",
            c = count.display()
        );
        let (n, spawn) = counted(&script);
        let (tx, rx) = mpsc::channel(1);
        let up = tokio::spawn(async move {
            for _ in 0..200 {
                if std::fs::read_to_string(&count).is_ok_and(|s| s.trim() == "3") {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    return tx.send(libc::SIGTERM).await.is_ok();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        });
        let code = tokio::time::timeout(Duration::from_secs(10), run(&quick(), spawn, rx))
            .await
            .expect("supervise did not end on the signal");
        assert!(up.await.unwrap(), "the third start never came up");
        assert_eq!(
            code,
            128 + libc::SIGTERM as u8,
            "the daemon's own end, passed on"
        );
        assert_eq!(n.load(Ordering::SeqCst), 3, "no restart after the signal");
    }

    #[tokio::test]
    async fn a_signal_during_the_wait_ends_it() {
        let (n, spawn) = counted("exit 3");
        let (tx, rx) = mpsc::channel(1);
        let policy = Policy {
            first: Duration::from_secs(30),
            ..quick()
        };
        let starts = n.clone();
        tokio::spawn(async move {
            // After the first crash, inside its 30 s wait.
            while starts.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = tx.send(libc::SIGTERM).await;
        });
        let code = tokio::time::timeout(Duration::from_secs(5), run(&policy, spawn, rx))
            .await
            .expect("waited out the backoff");
        assert_eq!(code, 0, "asked to stop with nothing running");
        assert_eq!(n.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_daemon_whose_heartbeat_stops_is_killed_and_started_again() {
        let dir = crate::test_dir("supervise-hung");
        let (count, beat) = (dir.join("n"), dir.join(HEARTBEAT));
        // Beats every 0.2 s; the first start freezes itself (SIGSTOP), as a hung daemon would.
        let script = format!(
            "n=$(cat {c} 2>/dev/null || echo 0); n=$((n+1)); echo $n > {c}; \
             while :; do date +%s > {b}; [ $n = 1 ] && kill -STOP $$; sleep 0.2; done",
            c = count.display(),
            b = beat.display()
        );
        let (n, spawn) = counted(&script);
        let policy = Policy {
            heartbeat: Some(beat),
            check: Duration::from_millis(100),
            grace: Duration::from_millis(500),
            stale: Duration::from_secs(2),
            ..quick()
        };
        let (tx, rx) = mpsc::channel(1);
        let up = tokio::spawn(async move {
            for _ in 0..300 {
                if std::fs::read_to_string(&count).is_ok_and(|s| s.trim() == "2") {
                    // Beating: it must be left alone for longer than `stale`.
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    return tx.send(libc::SIGTERM).await.is_ok();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            false
        });
        let code = tokio::time::timeout(Duration::from_secs(15), run(&policy, spawn, rx))
            .await
            .expect("supervise did not end on the signal");
        assert!(
            up.await.unwrap(),
            "the frozen daemon was never started again"
        );
        assert_eq!(code, 128 + libc::SIGTERM as u8);
        assert_eq!(n.load(Ordering::SeqCst), 2, "one kill, one restart");
    }
}
