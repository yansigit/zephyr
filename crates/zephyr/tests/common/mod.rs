//! Helpers the integration tests share. Under nextest each test runs in its own process, so
//! nothing here may rely on another test's state.
#![allow(dead_code)] // each test binary uses only some of these

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Polls `done` every 50 ms for up to `secs` seconds, then fails.
pub async fn wait(secs: u64, mut done: impl FnMut() -> bool) {
    for _ in 0..secs * 20 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out");
}

/// A port for a gateway's listener, below the kernel's ephemeral range (32768 up), so a `bind(0)`
/// elsewhere cannot take it before the router binds it. A process never hands a port out twice (a
/// second gateway could get it while the first has yet to bind it), and each process starts at a
/// random one: nextest runs every test in a process of its own, at the same time.
pub fn free_port() -> u16 {
    use std::hash::{BuildHasher, Hasher};
    static START: std::sync::LazyLock<u32> = std::sync::LazyLock::new(|| {
        let r = std::collections::hash_map::RandomState::new()
            .build_hasher()
            .finish();
        (r % 12_000) as u32
    });
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    loop {
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let port = 20_000 + ((*START + n) % 12_000) as u16;
        if std::net::TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// A fresh directory under the temp dir for the test `name`.
pub fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zephyr-it-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The messages fake-rpi.py was sent in `home` (`prompt` and `queue_message`), in order.
pub fn prompts(home: &Path) -> Vec<String> {
    std::fs::read_to_string(home.join("log.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
        .filter(|v| v["type"] == "prompt" || v["type"] == "queue_message")
        .map(|v| v["message"].as_str().unwrap_or_default().to_string())
        .collect()
}

/// Writes `text` to `path` as an executable script, from a child process, as the library's
/// `crate::test_exe` does. Not written here: another test's thread can fork while this process
/// holds the file open for writing, and until that fork execs, running the file fails with
/// ETXTBSY ("Text file busy").
pub fn test_exe(path: &Path, text: &str) {
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
