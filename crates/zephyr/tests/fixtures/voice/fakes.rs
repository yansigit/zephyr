//! The fake ffmpeg, whisper and Piper for the integration tests (`#[path]`-included by each).

use std::path::{Path, PathBuf};

/// A copy of the fakes (`tests/fixtures/voice`) in `home`, as the image lays its voice folder
/// out, with the English voice and a log `nobody` can append to.
pub fn fakes(home: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = home.join("voice");
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice");
    for file in ["bin/ffmpeg", "bin/whisper-cli", "py/bin/python"] {
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

/// The hash the fakes log for `bytes`.
#[allow(dead_code)]
pub fn sha256(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&zephyr::web::passkey::sha256(bytes))
}
