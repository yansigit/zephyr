//! Zephyr: an always-on agent gateway over upstream `rpi --mode rpc`. See DESIGN.md.

pub mod actions;
pub mod activity;
pub mod agent;
pub mod approvals;
pub mod attach;
pub mod auth;
pub mod auth_keeper;
pub mod broker;
pub mod browser_gate;
pub mod btw;
pub mod ca;
pub mod calls;
pub mod cards;
pub mod cards_link;
pub mod catalog;
pub mod channel;
pub mod chat;
pub mod checkpoint;
pub mod config;
pub mod desktop;
pub mod discord;
pub mod dotenv;
pub mod drafts;
pub mod egress;
pub mod facts;
pub mod gate;
pub mod host;
pub mod imessage;
pub mod inbox;
pub mod memory;
pub mod pairing;
pub mod pool;
pub mod procs;
pub mod reload;
pub mod retention;
pub mod review;
pub mod router;
pub mod rpc;
pub mod scheduler;
pub mod settings;
pub mod signal;
pub mod store;
pub mod supervise;
pub mod tasks;
pub mod telegram;
pub mod tools;
pub mod tui;
pub mod vault;
pub mod voice;
pub mod web;

pub fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

pub fn now() -> i64 {
    (unix_millis() / 1000) as i64
}

#[cfg(test)]
pub(crate) fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zephyr-{name}-{}-{}",
        std::process::id(),
        unix_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Writes `text` to `path` as an executable for a test to run. A child writes it, never this
/// process: another test's thread can fork while a descriptor open for writing is held here,
/// and until that fork execs, running the file fails with ETXTBSY ("Text file busy").
#[cfg(test)]
pub(crate) fn test_exe(path: &std::path::Path, text: &str) {
    use std::io::Write;
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "cat > \"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    assert!(
        child.wait().unwrap().success(),
        "writing {}",
        path.display()
    );
}
