//! End-to-end: the pinned rpi binary loads the cdylib and vetoes a tool call the
//! scripted model asks for. Ignored by default; skipped when rpi is absent.
//! No real provider is contacted: a scripted server on 127.0.0.1 plays the model.
//!
//! Run with: cargo test -p zephyr-policy --test rpi_load -- --ignored

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

const RPI: &str = "/home/user/Projects/.always-on/rpi-0.14.0/bin/rpi";

fn sse(event: &str, data: Value) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

fn message_start() -> String {
    sse(
        "message_start",
        json!({"type": "message_start", "message": {"id": "msg_1", "usage": {
            "input_tokens": 1, "output_tokens": 0,
            "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}}}),
    )
}

/// First model turn: asks for a `write` into a `.ssh` directory of the temp HOME.
fn tool_turn() -> String {
    message_start()
        + &sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "write", "input": {}}}),
        )
        + &sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta",
                "partial_json": "{\"path\":\"~/.ssh/zephyr-probe\",\"content\":\"x\"}"}}),
        )
        + &sse(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        + &sse(
            "message_delta",
            json!({"type": "message_delta",
            "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 1}}),
        )
        + &sse("message_stop", json!({"type": "message_stop"}))
}

/// Later turns: plain text, so the run ends.
fn text_turn() -> String {
    message_start()
        + &sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}}),
        )
        + &sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "text_delta", "text": "done"}}),
        )
        + &sse(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        )
        + &sse(
            "message_delta",
            json!({"type": "message_delta",
            "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
        )
        + &sse("message_stop", json!({"type": "message_stop"}))
}

/// Reads one HTTP request (headers plus Content-Length body) from the stream.
fn read_request(stream: &mut std::net::TcpStream) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
        let len: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("content-length:"))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        if buf.len() >= end + 4 + len {
            return;
        }
    }
}

fn serve(listener: TcpListener, requests: Arc<AtomicUsize>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            return;
        };
        read_request(&mut stream);
        let turn = if requests.fetch_add(1, Ordering::SeqCst) == 0 {
            tool_turn()
        } else {
            text_turn()
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{turn}"
        );
        let _ = stream.write_all(response.as_bytes());
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("zephyr-policy-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn rpi_loads_plugin_and_vetoes_write_into_ssh() {
    if !Path::new(RPI).exists() {
        eprintln!("SKIP: {RPI} not installed");
        return;
    }
    let lib = common::build_cdylib();
    let root = scratch("rpi");
    let home = root.join("home");
    let agent = root.join("agent");
    let sessions = root.join("sessions");
    let audit = root.join("audit.jsonl");
    for dir in [&home, &agent, &sessions] {
        std::fs::create_dir_all(dir).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(AtomicUsize::new(0));
    std::thread::spawn({
        let requests = requests.clone();
        move || serve(listener, requests)
    });

    let output = Command::new(RPI)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("RPI_CODING_AGENT_DIR", &agent)
        .env("ANTHROPIC_API_KEY", "test-key-not-real")
        .env("ZEPHYR_AUDIT_LOG", &audit)
        .args([
            "--provider",
            "anthropic",
            "--model",
            "claude-haiku-4-5",
            "--base-url",
            &format!("http://127.0.0.1:{port}"),
            "--extension",
            lib.to_str().unwrap(),
            "--local-only",
            "--no-session",
            "--session-dir",
            sessions.to_str().unwrap(),
            "-p",
            "write the probe file",
        ])
        .output()
        .expect("spawn rpi");
    assert!(
        output.status.success(),
        "rpi failed: {}\n{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        requests.load(Ordering::SeqCst) >= 1,
        "model server was never called"
    );

    let log = std::fs::read_to_string(&audit).unwrap_or_default();
    assert!(
        log.lines()
            .any(|l| l.contains("\"tool\":\"write\"") && l.contains("\"decision\":\"deny\"")),
        "no deny line for the write call in {}: {log}",
        audit.display()
    );
    assert!(
        !home.join(".ssh/zephyr-probe").exists(),
        "the vetoed write still created the file"
    );
    let _ = std::fs::remove_dir_all(&root);
}

/// Runs `rpi --mode rpc`, asks for `get_commands`, and returns the command names.
fn rpc_command_names(extension: Option<&Path>) -> Vec<String> {
    let root = scratch(if extension.is_some() {
        "rpc-on"
    } else {
        "rpc-off"
    });
    let home = root.join("home");
    let agent = root.join("agent");
    let sessions = root.join("sessions");
    for dir in [&home, &agent, &sessions] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let mut args: Vec<String> = [
        "--provider",
        "anthropic",
        "--model",
        "claude-haiku-4-5",
        "--base-url",
        "http://127.0.0.1:9",
        "--local-only",
        "--no-session",
        "--session-dir",
    ]
    .map(String::from)
    .to_vec();
    args.push(sessions.to_string_lossy().into_owned());
    if let Some(lib) = extension {
        args.push("--extension".into());
        args.push(lib.to_string_lossy().into_owned());
    }
    args.extend(["--mode".into(), "rpc".into()]);

    let mut child = Command::new(RPI)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("RPI_CODING_AGENT_DIR", &agent)
        .env("ANTHROPIC_API_KEY", "test-key-not-real")
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn rpi");
    {
        let mut stdin = child.stdin.take().unwrap();
        // get_commands needs no model; the base URL points at a closed port anyway.
        stdin
            .write_all(b"{\"id\":\"1\",\"type\":\"get_commands\"}\n")
            .unwrap();
    }
    let output = child.wait_with_output().expect("wait rpi");
    let _ = std::fs::remove_dir_all(&root);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let response = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["type"] == "response" && v["command"] == "get_commands")
        .unwrap_or_else(|| {
            panic!(
                "no get_commands response; stdout: {stdout}\nstderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    response["data"]["commands"]
        .as_array()
        .expect("commands array")
        .iter()
        .filter_map(|c| c["name"].as_str().map(String::from))
        .collect()
}

#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn rpc_lists_status_command_only_when_plugin_is_loaded() {
    if !Path::new(RPI).exists() {
        eprintln!("SKIP: {RPI} not installed");
        return;
    }
    let lib = common::build_cdylib();
    let with = rpc_command_names(Some(&lib));
    assert!(
        with.iter().any(|n| n == "zephyr-policy"),
        "zephyr-policy missing with --extension: {with:?}"
    );
    let without = rpc_command_names(None);
    assert!(
        !without.iter().any(|n| n == "zephyr-policy"),
        "zephyr-policy listed without --extension: {without:?}"
    );
}
