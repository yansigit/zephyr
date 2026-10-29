//! Shared helpers for the integration tests.

use std::path::PathBuf;
use std::process::Command;

pub const LIB_FILE: &str = if cfg!(target_os = "windows") {
    "zephyr_policy.dll"
} else if cfg!(target_os = "macos") {
    "libzephyr_policy.dylib"
} else {
    "libzephyr_policy.so"
};

/// Builds the cdylib into the same target dir as this test run and returns its path.
pub fn build_cdylib() -> PathBuf {
    // test exe: <target>/<profile>/deps/<name>; the cdylib lands in <target>/<profile>.
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.ancestors().nth(2).unwrap().to_path_buf();
    let target_dir = profile_dir.parent().unwrap().to_path_buf();
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut build = Command::new(cargo);
    build
        .args(["build", "-p", "zephyr-policy", "--lib"])
        .env("CARGO_TARGET_DIR", &target_dir);
    if profile_dir.ends_with("release") {
        build.arg("--release");
    }
    let status = build.status().expect("spawn cargo build");
    assert!(
        status.success(),
        "cargo build -p zephyr-policy --lib failed"
    );
    profile_dir.join(LIB_FILE)
}
