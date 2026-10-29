//! Zephyr: an always-on agent gateway over upstream `rpi --mode rpc`. See DESIGN.md.

pub mod channel;
pub mod chat;
pub mod config;
pub mod router;
pub mod rpc;
pub mod store;

pub fn unix_millis() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
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
