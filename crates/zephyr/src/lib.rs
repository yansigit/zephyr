//! Zephyr: an always-on agent gateway over upstream `rpi --mode rpc`. See DESIGN.md.

pub mod agent;
pub mod attach;
pub mod auth;
pub mod auth_keeper;
pub mod btw;
pub mod channel;
pub mod chat;
pub mod config;
pub mod desktop;
pub mod discord;
pub mod dotenv;
pub mod drafts;
pub mod host;
pub mod imessage;
pub mod memory;
pub mod pool;
pub mod procs;
pub mod reload;
pub mod retention;
pub mod router;
pub mod rpc;
pub mod scheduler;
pub mod store;
pub mod supervise;
pub mod tasks;
pub mod tools;
pub mod tui;

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
