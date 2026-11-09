//! End to end: the real pinned `rpi`, this crate's cdylib, and a fake Cursor agent service.
//!
//! What this proves, in one run: rpi loads the plugin and lists its models; the provider answers the
//! `requestContext` and KV handshakes; a `mcpArgs` call becomes `toolcall_end` with `toolUse`, so rpi
//! runs the tool itself; and the second turn replays the assistant's call and the tool result as
//! history blobs, which the service can read back.
//!
//! Ignored, like zephyr's other real-binary tests: it needs `rpi` 0.21 at the pinned path (or
//! `RPI_BIN`). No network: the service is a loopback HTTP/2 listener.
//!
//! ```sh
//! cargo test -p zephyr-cursor --test e2e -- --ignored --nocapture
//! ```

mod support {
    // Inside `mod support` the directory is `tests/support/`, so the path is relative to that.
    #[path = "cursor_service.rs"]
    pub mod cursor_service;
}

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use support::cursor_service::{self, Observed, Script};

const PINNED: &str = "/home/user/Projects/.always-on/rpi-0.21.2/bin/rpi";
const TIMEOUT: Duration = Duration::from_secs(120);

/// A bearer token the exchange step must pass through untouched: three segments, `exp` far ahead.
const TOKEN: &str = "eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjk5OTk5OTk5OTl9.c2ln";

#[test]
#[ignore = "needs the pinned rpi binary"]
fn rpi_runs_a_tool_through_the_cursor_provider() {
    let rpi = std::env::var_os("RPI_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(PINNED));
    if !rpi.is_file() {
        eprintln!("skipping: no rpi at {} (set RPI_BIN)", rpi.display());
        return;
    }
    let plugin = built_plugin();
    assert!(
        plugin.is_file(),
        "the cdylib was not built: {}",
        plugin.display()
    );

    let root = scratch("zephyr-cursor-e2e");
    let home = root.join("home");
    let sessions = root.join("sessions");
    let extensions = root.join("ext");
    let target = root.join("ran.txt");
    for dir in [&home, &sessions, &extensions] {
        std::fs::create_dir_all(dir).expect("scratch dir");
    }
    std::fs::copy(&plugin, extensions.join("libzephyr_cursor.so")).expect("stage the plugin");

    // The service needs a runtime of its own: the test thread blocks on rpi, so a service sharing
    // this thread would never accept the connection.
    let script = Script {
        file: target.display().to_string(),
    };
    let (ready, service) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let started = cursor_service::start(script).await.expect("fake service");
            ready.send(started).expect("hand the service to the test");
            futures_util::future::pending::<()>().await;
        });
    });
    let service = service.recv().expect("the fake service did not start");
    let url = format!(
        "http://127.0.0.1:{}/agent.v1.AgentService/Run",
        service.port
    );

    let output = run(Command::new(&rpi)
        .args(["--mode", "json", "-p", "create the file"])
        .args([
            "--provider",
            "cursor",
            "--model",
            "composer-2.5",
            "--api-key",
            TOKEN,
        ])
        .args(["--extensions-dir"])
        .arg(&extensions)
        .args([
            "--local-only",
            "--no-skills",
            "--no-context-files",
            "--no-prompt-templates",
        ])
        .args(["--session-dir"])
        .arg(&sessions)
        .args(["--approve"])
        .env("RPI_CODING_AGENT_DIR", &home)
        .env("CURSOR_AGENT_URL", &url)
        .env("CURSOR_API_KEY", TOKEN));

    let observed = service.observed.lock().unwrap().clone();
    let ran = std::fs::read_to_string(&target).ok();
    let _ = std::fs::remove_dir_all(&root);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        observed.errors.is_empty(),
        "the service saw protocol errors: {:?}\n--- stdout\n{stdout}\n--- stderr\n{stderr}",
        observed.errors
    );
    assert_eq!(
        observed.runs.len(),
        2,
        "one turn to call the tool and one to finish\n--- stdout\n{stdout}"
    );
    assert_eq!(
        observed
            .headers
            .get("x-cursor-agent-allowed-tools")
            .map(String::as_str),
        Some("mcp_tool_call,get_mcp_tools_tool_call"),
        "Cursor's own tools must be off: {:?}",
        observed.headers
    );
    assert!(
        observed.headers.get("authorization").map(String::as_str)
            == Some(&format!("Bearer {TOKEN}")),
        "the bearer token is forwarded: {:?}",
        observed.headers.keys().collect::<Vec<_>>()
    );
    assert!(observed.headers.contains_key("x-ghost-mode"));
    let first = &observed.runs[0];
    assert_eq!(first.model, "composer-2.5");
    assert!(
        first.tools.iter().any(|tool| tool == "zephyr_bash"),
        "{:?}",
        first.tools
    );
    assert_eq!(first.action.as_deref(), Some("create the file"));
    assert!(
        first.blob_ids >= 1,
        "the system prompt should be replayed as history"
    );
    assert!(!first.conversation_id.is_empty());
    let second = &observed.runs[1];
    assert!(
        second
            .action
            .as_deref()
            .unwrap_or_default()
            .contains("Continue"),
        "a turn that ends on a tool result continues: {:?}",
        second.action
    );
    assert!(observed
        .blobs
        .iter()
        .any(|blob| blob.contains("tool-result") && blob.contains("bash")));
    assert!(
        observed.replies >= 2,
        "requestContext and blob replies: {}",
        observed.replies
    );
    assert!(
        observed
            .context_tools
            .iter()
            .any(|tool| tool == "zephyr_bash"),
        "{:?}",
        observed.context_tools
    );
    // The service asks for the client's MCP state on every turn; each answer lists our tools.
    assert_eq!(
        observed.mcp_state_tools.len(),
        2,
        "an mcp_state reply per turn: {:?}",
        observed.mcp_state_tools
    );
    for tools in &observed.mcp_state_tools {
        assert!(tools.iter().any(|tool| tool == "zephyr_bash"), "{tools:?}");
    }
    // The composite call id comes back verbatim (newline and all) in both the replayed tool call
    // and its result, never in the `zcx_` form rpi carried it in.
    let raw_id = serde_json::to_string(cursor_service::CALL_ID).expect("json string");
    let carrying: Vec<&String> = observed
        .blobs
        .iter()
        .filter(|blob| blob.contains(&raw_id))
        .collect();
    assert!(
        carrying.iter().any(|blob| blob.contains("tool-call"))
            && carrying.iter().any(|blob| blob.contains("tool-result")),
        "the raw call id {raw_id} should be in the tool-call and tool-result blobs: {:?}",
        observed.blobs
    );
    assert!(
        !observed.blobs.iter().any(|blob| blob.contains("zcx_")),
        "{:?}",
        observed.blobs
    );
    assert_eq!(
        ran.as_deref().map(str::trim),
        Some("hi"),
        "the tool did not run"
    );
    assert!(
        stdout.contains("tool_execution_start"),
        "rpi did not execute the tool\n{stdout}"
    );
    assert!(stdout.contains("\"bash\""), "{stdout}");
    assert!(
        output.status.success(),
        "rpi failed\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
}

fn run(command: &mut Command) -> std::process::Output {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn rpi");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("wait for rpi") {
            Some(_) => return child.wait_with_output().expect("collect rpi output"),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let output = child.wait_with_output().expect("collect killed rpi output");
                panic!(
                    "rpi did not finish within {TIMEOUT:?}\n--- stdout\n{}\n--- stderr\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

/// `target/<profile>/libzephyr_cursor.so`, found from the test binary's own directory (cargo builds
/// the cdylib beside the tests, not inside `deps/`).
fn built_plugin() -> PathBuf {
    let exe = std::env::current_exe().expect("test binary path");
    let profile = exe.parent().and_then(Path::parent).expect("target dir");
    // `cargo test` builds the rlib, not the cdylib: build it here so rpi never loads a stale one.
    let status = Command::new(env!("CARGO"))
        .args(["build", "-p", "zephyr-cursor"])
        .env("CARGO_TARGET_DIR", profile.parent().expect("target root"))
        .status()
        .expect("run cargo build");
    assert!(status.success(), "cargo build -p zephyr-cursor failed");
    profile.join("libzephyr_cursor.so")
}

fn scratch(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_nanos())
        .unwrap_or_default();
    let base = std::env::temp_dir().join(format!("{name}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&base).expect("scratch root");
    base
}

#[allow(dead_code)]
fn observed_summary(observed: &Observed) -> String {
    format!(
        "{} requests, {} runs, {} blobs",
        observed.requests,
        observed.runs.len(),
        observed.blobs.len()
    )
}
