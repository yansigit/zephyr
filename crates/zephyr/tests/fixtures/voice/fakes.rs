//! The fake ffmpeg, whisper and Piper for the integration tests (`#[path]`-included by each).

use std::path::{Path, PathBuf};

/// A copy of the fakes (`tests/fixtures/voice`) in `home`, as the image lays its voice folder
/// out, with the English voice and a log `nobody` can append to.
pub fn fakes(home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join("voice");
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice");
    for file in [
        "bin/ffmpeg",
        "bin/whisper-cli",
        "bin/whisper-server",
        "py/bin/python",
    ] {
        std::fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
        std::fs::copy(from.join(file), dir.join(file)).unwrap();
    }
    std::fs::create_dir_all(dir.join("voices")).unwrap();
    std::fs::write(dir.join("voices/en_US-lessac-medium.onnx"), "").unwrap();
    std::fs::write(dir.join("log"), "").unwrap();
    std::fs::set_permissions(dir.join("log"), std::fs::Permissions::from_mode(0o666)).unwrap();
    dir
}

/// Each run of the fake `prog`, as it logged it: `uid`, `argv` and the `sha256` of its input.
#[allow(dead_code)]
pub fn ran(home: &Path, prog: &str) -> Vec<serde_json::Value> {
    let log = std::fs::read_to_string(home.join("voice/log")).unwrap_or_default();
    (log.lines())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["prog"] == prog)
        .collect()
}

/// Waits until the process whose pid the fake wrote to `file` (`whisper-server.pid`,
/// `piper.pid`) is gone: killed, though it may be a zombie its new parent has yet to reap.
#[allow(dead_code)]
pub async fn gone(home: &Path, file: &str) {
    let pid = std::fs::read_to_string(home.join("voice").join(file)).unwrap();
    let stat = || std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
    let gone = || stat().map_or(true, |s| s.contains(") Z ") || s.contains(") X "));
    for _ in 0..400 {
        if gone() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("{file}: the process is still there: {:?}", stat());
}

/// Whether the process whose pid the fake wrote to `file` runs now: not gone, not a zombie.
#[allow(dead_code)]
pub fn running(home: &Path, file: &str) -> bool {
    let pid = std::fs::read_to_string(home.join("voice").join(file)).unwrap();
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
    stat.is_ok_and(|s| !s.contains(") Z ") && !s.contains(") X "))
}

/// The hash the fakes log for `bytes`.
#[allow(dead_code)]
pub fn sha256(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&zephyr::web::passkey::sha256(bytes))
}
