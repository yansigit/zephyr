//! The agent's X desktop (DESIGN §4.11): Xvfb plus a window manager on `$ZEPHYR_DESKTOP`, run as
//! the agent. The image sets the variable; without it (tests, the host) there is no desktop.

use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

static RUNNING: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// The display children should use, after starting (or restarting) what serves it. A server left
/// over from before a re-exec keeps the display; the new one then exits at once, which is harmless.
pub fn ensure() -> Option<String> {
    let display = std::env::var("ZEPHYR_DESKTOP").ok()?;
    let mut running = RUNNING.lock().unwrap();
    running.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    if running.len() < 2 {
        for c in running.iter_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        running.clear();
        let xvfb = ["-screen", "0", "1920x1080x24", "-nolisten", "tcp"];
        running.extend(start("Xvfb", &[&display], &xvfb));
        // The window manager needs the server up first.
        std::thread::sleep(std::time::Duration::from_millis(300));
        running.extend(start("openbox", &[], &[]));
    }
    Some(display)
}

fn start(program: &str, first: &[&str], rest: &[&str]) -> Option<Child> {
    let display = std::env::var("ZEPHYR_DESKTOP").ok()?;
    crate::agent::cmd(&mut Command::new(program))
        .args(first)
        .args(rest)
        .env("DISPLAY", display)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("zephyr: starting {program} for the desktop: {e}"))
        .ok()
}
