//! `zephyr supervise`, the image's command (DESIGN §4.4): `docker-init` (PID 1, from `--init`)
//! runs it, and it runs `zephyr run`. When the daemon crashes it is started again, without the
//! container restarting, so the `exec` commands it started live on: orphaned, they are kept and
//! reaped by PID 1, and the new daemon adopts them (`procs::Procs::adopt`). SIGTERM and SIGINT
//! are passed to the daemon, and then nothing is restarted, so `docker stop` stays a clean stop.

use std::collections::VecDeque;
use std::os::unix::process::ExitStatusExt;
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
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            first: Duration::from_secs(1),
            max: Duration::from_secs(30),
            healthy: Duration::from_secs(60),
            window: Duration::from_secs(120),
            max_exits: 5,
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
    let daemon = || {
        let mut cmd = tokio::process::Command::new(std::env::current_exe()?);
        cmd.arg("run").spawn()
    };
    ExitCode::from(run(&Policy::default(), daemon, rx).await)
}

/// Runs what `spawn` starts until a signal from `signals` ends it (passed on, never restarted)
/// or it crashes `max_exits` times within `window`. Returns the exit code to leave with.
pub async fn run(
    policy: &Policy,
    mut spawn: impl FnMut() -> std::io::Result<tokio::process::Child>,
    mut signals: mpsc::Receiver<i32>,
) -> u8 {
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
        let mut stopping = false;
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
            tokio::process::Command::new("sh")
                .args(["-c", &script])
                .spawn()
        })
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
}
