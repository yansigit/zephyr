//! End-to-end: the pinned rpi binary loads the cdylib and vetoes a tool call the
//! scripted model asks for. Ignored by default; skipped when rpi is absent.
//! No real provider is contacted: a scripted server on 127.0.0.1 plays the model.
//!
//! Run with: cargo test -p zephyr-policy --test rpi_load -- --ignored

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use zephyr_policy::tools::{GATE_SOCKET_ENV, SESSION_ENV};

const RPI: &str = "/home/user/Projects/.always-on/rpi-0.18.0/bin/rpi";

/// A test that needs the pinned rpi skips without it, unless ZEPHYR_REAL_RPI=1 (deploy/release.sh
/// sets it): then it fails, so a run cannot go green without testing anything.
fn skip_without_rpi(rpi: impl std::fmt::Display) {
    if std::env::var("ZEPHYR_REAL_RPI").as_deref() == Ok("1") {
        panic!("ZEPHYR_REAL_RPI=1 but {rpi} is not installed");
    }
    eprintln!("skipping: {rpi} is not installed");
}

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
    call_turn(
        "toolu_1",
        "write",
        "{\"path\":\"~/.ssh/zephyr-probe\",\"content\":\"x\"}",
    )
}

/// A model turn that calls tool `name` (id `id`) with the JSON `input`.
fn call_turn(id: &str, name: &str, input: &str) -> String {
    message_start()
        + &sse(
            "content_block_start",
            json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}}),
        )
        + &sse(
            "content_block_delta",
            json!({"type": "content_block_delta", "index": 0,
            "delta": {"type": "input_json_delta", "partial_json": input}}),
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

/// Reads one HTTP request (headers plus Content-Length body) from the stream; returns it all.
fn read_request(stream: &mut std::net::TcpStream) -> String {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return String::from_utf8_lossy(&buf).into_owned();
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
            return String::from_utf8_lossy(&buf).into_owned();
        }
    }
}

fn serve(listener: TcpListener, requests: Arc<AtomicUsize>) {
    serve_turns(listener, requests, vec![tool_turn()], Default::default());
}

/// Answers the requests in order with `turns`, every later one with text; keeps each request.
fn serve_turns(
    listener: TcpListener,
    requests: Arc<AtomicUsize>,
    turns: Vec<String>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else {
            return;
        };
        seen.lock().unwrap().push(read_request(&mut stream));
        let turn = turns
            .get(requests.fetch_add(1, Ordering::SeqCst))
            .cloned()
            .unwrap_or_else(text_turn);
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
        skip_without_rpi(RPI);
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
        skip_without_rpi(RPI);
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

/// `rpi --mode rpc` with the plugin, whose model makes the calls in `turns`, in order; the test
/// plays the daemon.
struct GatewayRpi {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    events: std::sync::mpsc::Receiver<Value>,
    seen: Arc<std::sync::Mutex<Vec<String>>>,
    gw: PathBuf,
    root: PathBuf,
}

impl GatewayRpi {
    fn start(name: &str, turns: Vec<String>) -> Self {
        Self::start_with(name, turns, false)
    }

    /// `main` starts it as zephyr starts the main child: `exec`/`process`, no `bash`.
    fn start_with(name: &str, turns: Vec<String>, main: bool) -> Self {
        Self::launch(name, turns, |cmd, _| {
            if main {
                cmd.env("ZEPHYR_EXEC", "1")
                    .args(["--exclude-tools", "bash"]);
            }
        })
    }

    /// A task's child, with the gate env `rpc::spawn_as` gives every child (`gate::agent_env`):
    /// the plugin asks the socket `<root>/gate.sock`, bound here and handed back for the test to
    /// serve, as `session`. `bin/himalaya` (first on `PATH`) leaves `bin/himalaya.ran` if run;
    /// the plugin's audit lines go to `audit.jsonl`.
    fn start_gated(name: &str, turns: Vec<String>, session: &str) -> (Self, UnixListener) {
        use std::os::unix::fs::PermissionsExt;
        let mut gate = None;
        let rpi = Self::launch(name, turns, |cmd, root| {
            let socket = root.join("gate.sock");
            gate = Some(UnixListener::bind(&socket).expect("bind the gate socket"));
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let fake = bin.join("himalaya");
            std::fs::write(&fake, "#!/bin/sh\ntouch \"$0.ran\"\n").unwrap();
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut path = std::ffi::OsString::from(&bin);
            path.push(":");
            path.push(std::env::var_os("PATH").unwrap_or_default());
            cmd.env("PATH", path)
                .env("ZEPHYR_AUDIT_LOG", root.join("audit.jsonl"))
                .env(GATE_SOCKET_ENV, &socket)
                .env(SESSION_ENV, session);
        });
        (rpi, gate.unwrap())
    }

    /// Starts the child; `setup` adds to its command last, so its env wins.
    fn launch(name: &str, turns: Vec<String>, setup: impl FnOnce(&mut Command, &Path)) -> Self {
        use std::io::BufRead;
        let lib = common::build_cdylib();
        let root = scratch(name);
        let (home, agent, sessions, gw) = (
            root.join("home"),
            root.join("agent"),
            root.join("sessions"),
            root.join("gateway"),
        );
        for dir in [&home, &agent, &sessions, &gw] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        std::thread::spawn({
            let seen = seen.clone();
            move || serve_turns(listener, Default::default(), turns, seen)
        });
        let mut cmd = Command::new(RPI);
        cmd.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &home)
            .env("RPI_CODING_AGENT_DIR", &agent)
            .env("ANTHROPIC_API_KEY", "test-key-not-real")
            .env("ZEPHYR_GATEWAY_DIR", &gw)
            .args([
                "--provider",
                "anthropic",
                "--model",
                "claude-haiku-4-5",
                "--base-url",
            ])
            .arg(format!("http://127.0.0.1:{port}"))
            .arg("--extension")
            .arg(&lib)
            .args(["--local-only", "--no-session", "--session-dir"])
            .arg(&sessions)
            .args(["--mode", "rpc"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        setup(&mut cmd, &root);
        let mut child = cmd.spawn().expect("spawn rpi");
        let stdin = child.stdin.take().unwrap();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let (tx, events) = std::sync::mpsc::channel::<Value>();
        std::thread::spawn(move || {
            for line in stdout.lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str(&line) {
                    let _ = tx.send(v);
                }
            }
        });
        let mut rpi = Self {
            child,
            stdin,
            events,
            seen,
            gw,
            root,
        };
        rpi.next(|v| v["type"] == "ready");
        rpi
    }

    /// The next frame matching `what`, within 20 s.
    fn next(&mut self, what: impl Fn(&Value) -> bool) -> Value {
        loop {
            let ev = self
                .events
                .recv_timeout(std::time::Duration::from_secs(20))
                .expect("rpi went quiet");
            if what(&ev) {
                return ev;
            }
        }
    }

    fn send(&mut self, line: &str) {
        self.stdin.write_all(line.as_bytes()).unwrap();
        self.stdin.write_all(b"\n").unwrap();
    }

    /// Sends the prompt and returns once the `tool` call `id` has started.
    fn until_call(&mut self, tool: &str, id: &str) {
        self.send(r#"{"id":"1","type":"prompt","message":"status?"}"#);
        let start = self.next(|v| v["type"] == "tool_execution_start");
        assert_eq!(start["toolName"], tool, "{start}");
        assert_eq!(start["toolCallId"], id, "{start}");
    }
}

impl Drop for GatewayRpi {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The `gateway` tool waits in `poll` while rpi keeps streaming events, so the daemon can answer
/// it mid-turn: here the test plays the daemon, writing the answer only once it has read the
/// call's `tool_execution_start` from rpi's stdout.
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn rpi_waits_for_a_gateway_answer_mid_turn() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let first = call_turn("toolu_gw", "gateway", "{\"command\":\"/status\"}");
    let mut rpi = GatewayRpi::start("gateway", vec![first]);
    rpi.until_call("gateway", "toolu_gw");
    // While the call waits, rpi still answers commands (the router makes some mid-run).
    rpi.send(r#"{"id":"2","type":"get_state"}"#);
    let state = rpi.next(|v| v["command"] == "get_state" || v["type"] == "tool_execution_end");
    assert_eq!(
        state["type"], "response",
        "rpi ended the call before the answer: {state}"
    );
    std::fs::write(rpi.gw.join("toolu_gw"), "agent: answered by the test").unwrap();
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    assert_ne!(end["isError"], true, "{end}");
    rpi.next(|v| v["type"] == "agent_settled");
    let seen = rpi.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "no second model request");
    assert!(
        seen[1].contains("agent: answered by the test"),
        "the answer did not reach the model: {}",
        seen[1]
    );
    assert!(
        !rpi.gw.join("toolu_gw").exists(),
        "the answer file is consumed"
    );
}

/// In the main child, `exec` and `process` replace rpi's `bash`, and an `exec` waits for the
/// daemon past `gateway`'s 10 s: until its command ends or yields.
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn the_main_child_runs_commands_through_exec_and_waits_for_them() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let first = call_turn("toolu_exec", "exec", "{\"command\":\"make\"}");
    let mut rpi = GatewayRpi::start_with("exec", vec![first], true);
    rpi.until_call("exec", "toolu_exec");
    std::thread::sleep(std::time::Duration::from_secs(11));
    std::fs::write(rpi.gw.join("toolu_exec"), "exited 0\nbuilt").unwrap();
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    assert_ne!(end["isError"], true, "{end}");
    assert!(end.to_string().contains("exited 0"), "{end}");
    rpi.next(|v| v["type"] == "agent_settled");
    let seen = rpi.seen.lock().unwrap().clone();
    let tools = |name: &str| seen[0].contains(&format!("\"name\":\"{name}\""));
    assert!(tools("exec") && tools("process"), "{}", seen[0]);
    assert!(!tools("bash"), "rpi's bash is excluded: {}", seen[0]);
    assert!(
        seen[1].contains("built"),
        "the answer reached the model: {}",
        seen[1]
    );
}

/// Outside the main child (jobs, tasks), rpi keeps its `bash` and there is no `exec`.
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn other_children_keep_bash_and_get_no_exec() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let mut rpi = GatewayRpi::start("no-exec", vec![text_turn()]);
    rpi.send(r#"{"id":"1","type":"prompt","message":"hi"}"#);
    rpi.next(|v| v["type"] == "agent_settled");
    let seen = rpi.seen.lock().unwrap().clone();
    assert!(seen[0].contains("\"name\":\"bash\""), "{}", seen[0]);
    assert!(seen[0].contains("\"name\":\"recall\""), "{}", seen[0]);
    assert!(!seen[0].contains("\"name\":\"exec\""), "{}", seen[0]);
}

/// `/stop` (an `abort`) ends a waiting `gateway` call at once, not after its 10 s wait.
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn an_abort_ends_a_waiting_gateway_call() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let first = call_turn("toolu_gw", "gateway", "{\"command\":\"/status\"}");
    let mut rpi = GatewayRpi::start("gateway-abort", vec![first]);
    rpi.until_call("gateway", "toolu_gw");
    let asked = std::time::Instant::now();
    rpi.send(r#"{"id":"2","type":"abort"}"#);
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    assert_eq!(end["isError"], true, "{end}");
    rpi.next(|v| v["type"] == "agent_settled");
    let took = asked.elapsed();
    assert!(
        took < std::time::Duration::from_secs(5),
        "the abort waited out the call: {took:?}"
    );
    // rpi 0.15 settles a run aborted mid-tool as `completed`; what matters is that the model is
    // not asked again.
    assert_eq!(
        rpi.seen.lock().unwrap().len(),
        1,
        "the run went on after the abort"
    );
}

/// `task` waits like `gateway`: the model lists tasks as `toolu_task`, and the test answers it
/// mid-turn. The model's next call, `task {"action": "stop"}` with no `id`, is invalid and must
/// fail at once, not after the 10 s wait.
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn rpi_waits_for_a_task_answer_mid_turn() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let list = call_turn("toolu_task", "task", "{\"action\":\"list\"}");
    let stop = call_turn("toolu_stop", "task", "{\"action\":\"stop\"}");
    let mut rpi = GatewayRpi::start("task", vec![list, stop]);
    rpi.until_call("task", "toolu_task");
    // While the call waits, rpi still answers commands (the router makes some mid-run).
    rpi.send(r#"{"id":"2","type":"get_state"}"#);
    let state = rpi.next(|v| v["command"] == "get_state" || v["type"] == "tool_execution_end");
    assert_eq!(
        state["type"], "response",
        "rpi ended the call before the answer: {state}"
    );
    std::fs::write(rpi.gw.join("toolu_task"), "task 1: running").unwrap();
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    assert_ne!(end["isError"], true, "{end}");
    assert!(
        end["result"].to_string().contains("task 1: running"),
        "the answer is not in the tool result: {end}"
    );
    let start = rpi.next(|v| v["type"] == "tool_execution_start");
    assert_eq!(start["toolCallId"], "toolu_stop", "{start}");
    let asked = std::time::Instant::now();
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    let took = asked.elapsed();
    assert_eq!(end["isError"], true, "{end}");
    // The plugin's own refusal, not some other failure.
    assert!(
        end.to_string().contains("stop needs the task's `id`"),
        "{end}"
    );
    assert!(
        took < std::time::Duration::from_secs(5),
        "the invalid stop waited: {took:?}"
    );
    rpi.next(|v| v["type"] == "agent_settled");
    let seen = rpi.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "no second model request");
    assert!(
        seen[1].contains("task 1: running"),
        "the answer did not reach the model: {}",
        seen[1]
    );
    assert!(
        !rpi.gw.join("toolu_task").exists(),
        "the answer file is consumed"
    );
}

/// A held send, as the gate words it (`gate::held_text`).
const HELD: &str =
    "held for the owner as #1 (email to a@x.org); don't retry it; carry on or end your turn";

/// Why a test saw no request: the plugin asks only a gate served by root or by the rpi binary's
/// owner (`gate_client::trusted`), so the tests must run as one of them.
const NEVER_ASKED: &str = "the plugin never asked the gate (run the test as root or as the user \
                           that owns the rpi binary)";

/// The model's turn: a task's `bash` sends a message, which `policy::gated` recognises.
fn send_turn() -> String {
    let command = "himalaya message send <<'EOF'\nTo: a@x.org\nSubject: hi\n\nhi\nEOF";
    call_turn(
        "toolu_send",
        "bash",
        &json!({ "command": command }).to_string(),
    )
}

/// The daemon's gate, played by a thread: reads the one request on `gate`, hands it to the
/// returned receiver, waits for `go` (when given), then answers `answer`.
fn serve_gate(
    gate: UnixListener,
    answer: Value,
    go: Option<std::sync::mpsc::Receiver<()>>,
) -> std::sync::mpsc::Receiver<Value> {
    use std::io::BufRead;
    let (tx, asked) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = gate.accept() else {
            return;
        };
        let mut line = String::new();
        // A plugin that hangs up unasked (it trusted no gate here) asked nothing.
        match std::io::BufReader::new(&stream).read_line(&mut line) {
            Ok(n) if n > 0 => {}
            _ => return,
        }
        let _ = tx.send(serde_json::from_str(&line).unwrap_or(Value::Null));
        if let Some(go) = go {
            let _ = go.recv();
        }
        let _ = writeln!(stream, "{answer}");
    });
    asked
}

/// The `messages` of a request the model server kept.
fn model_messages(request: &str) -> Vec<Value> {
    let body = request.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    let body: Value = serde_json::from_str(body).unwrap_or_else(|_| panic!("{request}"));
    body["messages"].as_array().cloned().unwrap_or_default()
}

/// Approvals spec §4.2 path 2 under the pinned rpi: a task's `bash` send makes the plugin ask the
/// gate socket, naming the session `rpc::spawn_as` gave the child, and the gate's `hold` vetoes
/// the call before it runs. rpi shows the model only its own block line, never the gate's text:
/// what a hold means reaches the model as the daemon's steered note (next test).
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn rpi_vetoes_a_held_bash_call_after_asking_the_gate() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let (mut rpi, gate) = GatewayRpi::start_gated("gate-hold", vec![send_turn()], "task-7");
    let asked = serve_gate(
        gate,
        json!({"verdict": "hold", "id": 1, "text": HELD}),
        None,
    );
    rpi.until_call("bash", "toolu_send");
    let req = asked
        .recv_timeout(Duration::from_secs(20))
        .expect(NEVER_ASKED);
    assert_eq!(req["v"], 1, "{req}");
    assert_eq!(req["tool"], "bash", "{req}");
    assert_eq!(req["session"], "task-7", "{req}");
    assert_eq!(req["call_id"], "toolu_send", "{req}");
    assert!(
        req["args"]["command"]
            .as_str()
            .is_some_and(|c| c.starts_with("himalaya message send")),
        "{req}"
    );
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    assert_eq!(end["toolCallId"], "toolu_send", "{end}");
    assert_eq!(end["isError"], true, "{end}");
    assert!(
        end["result"].to_string().contains("blocked tool `bash`"),
        "not rpi's block line: {end}"
    );
    assert!(
        !end.to_string().contains(HELD),
        "the gate's text is the daemon's to steer: {end}"
    );
    rpi.next(|v| v["type"] == "agent_settled");
    // Refused for the gate's answer, not for want of one (a missing, silent or untrusted gate
    // is "the gate did not answer"), and never run.
    let audit = std::fs::read_to_string(rpi.root.join("audit.jsonl")).unwrap_or_default();
    assert!(
        audit
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .any(|l| l["tool"] == "bash" && l["reason"] == "the gate did not allow it"),
        "{audit}"
    );
    assert!(
        !rpi.root.join("bin/himalaya.ran").exists(),
        "the held send ran"
    );
}

/// R51 under the pinned rpi: while the plugin's `ToolCall` handler waits on the gate, rpi still
/// reads its stdin and answers the daemon's `queue_message` steer at once, inside the 2 s
/// `gate::steer` waits for it; once the hold vetoes the call, the note is the model's next input,
/// right after the refused call's result.
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn rpi_takes_a_steer_while_the_plugin_waits_on_the_gate() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let (mut rpi, gate) = GatewayRpi::start_gated("gate-steer", vec![send_turn()], "task-7");
    let (answer, go) = std::sync::mpsc::channel();
    let hold = json!({"verdict": "hold", "id": 1, "text": HELD});
    let asked = serve_gate(gate, hold, Some(go));
    rpi.until_call("bash", "toolu_send");
    asked
        .recv_timeout(Duration::from_secs(20))
        .expect(NEVER_ASKED);
    // The handler waits for the gate's answer now, as the daemon's gate holds it during R51.
    let steer =
        json!({"id": "steer", "type": "queue_message", "message": HELD, "deliverAs": "steer"});
    let sent = Instant::now();
    rpi.send(&steer.to_string());
    let queued = rpi.next(|v| v["id"] == "steer" || v["type"] == "tool_execution_end");
    let took = sent.elapsed();
    assert_eq!(
        queued["type"], "response",
        "rpi ended the call before it took the steer: {queued}"
    );
    assert_eq!(queued["success"], true, "{queued}");
    assert_eq!(queued["data"]["queue"], "steer", "{queued}");
    assert!(
        took < Duration::from_secs(2),
        "the steer waited past gate::steer's 2 s: {took:?}"
    );
    answer.send(()).unwrap();
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    assert_eq!(end["isError"], true, "{end}");
    rpi.next(|v| v["type"] == "agent_settled");
    let seen = rpi.seen.lock().unwrap().clone();
    assert!(seen.len() >= 2, "no second model request");
    let messages = model_messages(&seen[1]);
    let [.., result, note] = messages.as_slice() else {
        panic!("{messages:?}");
    };
    assert_eq!(result["content"][0]["type"], "tool_result", "{result}");
    assert_eq!(
        result["content"][0]["tool_use_id"], "toolu_send",
        "{result}"
    );
    assert_eq!(result["content"][0]["is_error"], true, "{result}");
    assert_eq!(note["role"], "user", "{note}");
    assert_eq!(note["content"][0]["text"], HELD, "{note}");
}

/// The round trip's other answer: `allow` lets the call run.
#[test]
#[ignore = "needs the pinned rpi binary; run with --ignored"]
fn rpi_runs_a_bash_call_the_gate_allows() {
    if !Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let (mut rpi, gate) = GatewayRpi::start_gated("gate-allow", vec![send_turn()], "task-7");
    let asked = serve_gate(gate, json!({"verdict": "allow"}), None);
    rpi.until_call("bash", "toolu_send");
    let req = asked
        .recv_timeout(Duration::from_secs(20))
        .expect(NEVER_ASKED);
    assert_eq!(req["tool"], "bash", "{req}");
    let end = rpi.next(|v| v["type"] == "tool_execution_end");
    assert_ne!(end["isError"], true, "{end}");
    assert!(
        rpi.root.join("bin/himalaya.ran").exists(),
        "the allowed send did not run: {end}"
    );
}
