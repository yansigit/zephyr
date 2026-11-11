//! `zephyr-policy`: rpi Rust-native plugin. Vetoes hardline tool calls on the
//! `ToolCall` event and appends one audit line per call (DESIGN.md §4.4 items 2, 6).
//!
//! The host turns any non-zero handler return into a block, so a panic or a
//! malformed event fails closed. The host shows only a generic block message
//! to the model; the reason string reaches the audit log only.

// `export_plugin!` expands to a safe `extern "C"` fn taking the host's `api` pointer;
// the SDK's `register_entrypoint` checks it before any dereference.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod gate_client;
pub mod policy;
pub mod tools;

use std::ffi::c_void;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use rpi_plugin_sdk::{
    event_handler, export_plugin, EventTag, StbString, StbStringRef, EVENT_HANDLER_ABORT,
    EVENT_HANDLER_CONTINUE,
};
use serde_json::Value;

use policy::{check, Decision};

/// Audit log path. Unset or empty disables auditing.
const AUDIT_ENV: &str = "ZEPHYR_AUDIT_LOG";
/// Byte cap on the args summary in each audit line.
const ARGS_LIMIT: usize = 2048;
const REDACTED: &str = "[REDACTED]";
const SECRET_WORDS: &[&str] = &["key", "token", "secret", "password", "passwd"];

event_handler! {
    fn on_tool_call(event, _data) {
        let Some(call) = event.tool_call() else {
            return EVENT_HANDLER_ABORT;
        };
        let tool = call.tool_name.as_str();
        let decision = match serde_json::from_str::<Value>(&call.params) {
            Ok(args) => match check(tool, &args) {
                Decision::Allow if policy::is_mcp(tool) => {
                    match policy::check_mcp(&args, connectors(), |p| std::fs::read(p).ok()) {
                        Decision::Allow if asks_gate(tool, &args) => {
                            ask_gate(&call.tool_call_id, tool, args)
                        }
                        d => d,
                    }
                }
                Decision::Allow if asks_gate(tool, &args) => {
                    ask_gate(&call.tool_call_id, tool, args)
                }
                d => d,
            },
            Err(_) => Decision::Deny("tool arguments are not valid JSON"),
        };
        let log = std::env::var_os(AUDIT_ENV).filter(|p| !p.is_empty()).map(PathBuf::from);
        audit(log.as_deref(), &call.tool_name, &call.params, &decision);
        match decision {
            Decision::Allow => EVENT_HANDLER_CONTINUE,
            Decision::Deny(_) => EVENT_HANDLER_ABORT,
        }
    }
}

/// How long a call waits for the gate's verdict (approvals spec §4.2).
const GATE_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// Whether the daemon's gate decides the call (approvals spec §4.2 path 2): a `bash` command
/// that is a message, a post or a payment, or that sends data out (for the private facts), and
/// every connector call (the owner's `classes` are the daemon's to apply). Main's `exec` is
/// decided in the daemon, which runs it, never here.
fn asks_gate(tool: &str, args: &Value) -> bool {
    match tool {
        "bash" => args
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| {
                policy::gated(tool, args, &|_, _| None).is_some() || !policy::outbound(c).is_empty()
            }),
        "mcp_call" => true,
        _ => false,
    }
}

/// Asks the gate socket (`$ZEPHYR_GATE_SOCKET`) about the call, as session `$ZEPHYR_SESSION`.
/// Only `{"verdict": "allow"}` lets it run; no gate, no answer within 5 s, a malformed answer, a
/// hold, a wait or a deny refuse it. The model is shown no reason: the daemon steers in what a
/// hold means.
fn ask_gate(call_id: &str, tool: &str, args: Value) -> Decision {
    let Some(socket) = std::env::var_os(tools::GATE_SOCKET_ENV).filter(|p| !p.is_empty()) else {
        return Decision::Deny("no gate to ask");
    };
    let session = std::env::var(tools::SESSION_ENV).unwrap_or_default();
    let req = serde_json::json!({
        "v": 1, "session": session, "call_id": call_id, "tool": tool, "args": args,
    });
    match gate_client::ask(Path::new(&socket), &req, GATE_WAIT) {
        Ok(answer) if answer["verdict"] == "allow" => Decision::Allow,
        Ok(_) => Decision::Deny("the gate did not allow it"),
        Err(_) => Decision::Deny("the gate did not answer"),
    }
}

/// The connectors file (`$ZEPHYR_MCP_CONFIG`) and its bytes, read once, when the plugin loads:
/// the only MCP config this child may use (`policy::check_mcp`). `None` when there is none.
static CONNECTORS: OnceLock<Option<(String, Vec<u8>)>> = OnceLock::new();

fn connectors() -> Option<(&'static str, &'static [u8])> {
    CONNECTORS
        .get_or_init(|| {
            let path = std::env::var(policy::MCP_CONFIG_ENV).ok()?;
            let bytes = std::fs::read(&path).ok()?;
            Some((path, bytes))
        })
        .as_ref()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
}

/// `/zephyr-policy`: the load beacon. The gateway calls `get_commands` to see it.
extern "C" fn status_command(
    _args: StbStringRef,
    out: *mut StbString,
    _user_data: *mut c_void,
) -> i32 {
    rpi_plugin_sdk::guard_or(1, || {
        if out.is_null() {
            return 1;
        }
        let text = r#"{"kind":"message","text":"zephyr-policy active"}"#;
        // SAFETY: the host passes a writable out slot and frees the string it receives.
        unsafe { *out = StbString::from_string(text.to_string()) };
        0
    })
}

export_plugin!(|api| {
    connectors(); // now, before the agent could edit the file
    let Some(register) = api.register_event_handler else {
        return -1;
    };
    let status = register(EventTag::ToolCall, on_tool_call, std::ptr::null_mut());
    if status != 0 {
        return status;
    }
    // Optional, like the beacon: a host that cannot take them still gets the veto.
    if let Some(register_tool) = api.register_tool {
        let main = std::env::var(tools::EXEC_ENV).is_ok_and(|v| v == "1");
        for (name, description, parameters, execute) in tools::TOOLS {
            // `exec` and `process` replace rpi's `bash` in the main child only (DESIGN §4.13),
            // and the vault and the cards are main's alone (§4.15, §4.25).
            if matches!(name, "exec" | "process" | "vault" | "card") && !main {
                continue;
            }
            let schema = tools::schema(name, description, parameters);
            let free = rpi_plugin_sdk::callbacks::free_string;
            if register_tool(
                &schema,
                execute,
                tools::poll,
                tools::cancel,
                tools::destroy,
                free,
            ) != 0
            {
                eprintln!("zephyr-policy: the host refused the {name} tool");
            }
        }
    }
    // The veto is required; the beacon is optional on hosts without command slots.
    if let Some(register_command) = api.register_command {
        return register_command(
            StbStringRef::from_str("zephyr-policy"),
            StbStringRef::from_str("Show whether the zephyr policy plugin is active"),
            status_command,
        );
    }
    0
});

/// Append one JSON line for a tool call. A write failure is reported on
/// stderr and never changes the decision.
fn audit(log: Option<&Path>, tool: &str, params: &str, decision: &Decision) {
    let Some(log) = log else {
        return;
    };
    let (args, args_truncated) = summarize(params);
    let (verdict, reason) = match decision {
        Decision::Allow => ("allow", None),
        Decision::Deny(reason) => ("deny", Some(*reason)),
    };
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let line = serde_json::json!({
        "ts_ms": ts_ms,
        "tool": tool,
        "decision": verdict,
        "reason": reason,
        "args": args,
        "args_truncated": args_truncated,
    });
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .and_then(|mut file| writeln!(file, "{line}"));
    if let Err(error) = written {
        eprintln!(
            "zephyr-policy: audit write to {} failed: {error}",
            log.display()
        );
    }
}

/// Redacted, byte-capped copy of the tool arguments for the audit line.
fn summarize(params: &str) -> (String, bool) {
    let redacted = match serde_json::from_str::<Value>(params) {
        Ok(value) => redact(value).to_string(),
        Err(_) => redact_words(params),
    };
    truncate(redacted, ARGS_LIMIT)
}

fn redact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let value = if is_secret_name(&key) {
                        Value::String(REDACTED.into())
                    } else {
                        redact(value)
                    };
                    (key, value)
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        Value::String(text) => Value::String(redact_words(&text)),
        other => other,
    }
}

/// Redacts `NAME=value` words, the word after a `--secret-flag`, and the token
/// after `Bearer`, in command text. `Bearer` may also be a plain word (fail-closed).
fn redact_words(text: &str) -> String {
    let mut redact_next = false;
    text.split(' ')
        .map(|word| {
            let redact_this = std::mem::take(&mut redact_next);
            if let Some((name, _)) = word.split_once('=') {
                if is_secret_name(name) {
                    return format!("{name}={}", REDACTED);
                }
            }
            if word.starts_with('-') && is_secret_name(word) {
                redact_next = true;
            }
            if word
                .trim_matches(|c: char| !c.is_ascii_alphabetic())
                .eq_ignore_ascii_case("bearer")
            {
                redact_next = true;
            }
            if redact_this && !word.is_empty() {
                REDACTED.to_string()
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn is_secret_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SECRET_WORDS.iter().any(|w| lower.contains(w))
}

fn truncate(mut text: String, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text, false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpi_plugin_sdk::StablePluginEvent;
    use serde_json::json;

    fn borrowed(text: &str) -> StbString {
        let r = StbStringRef::from_str(text);
        StbString {
            ptr: r.ptr as *mut _,
            len: r.len,
        }
    }

    fn handler_return(tool: &str, params: &str) -> i32 {
        let raw = StablePluginEvent::tool_call(
            EventTag::ToolCall,
            borrowed("call-1"),
            borrowed(tool),
            borrowed(params),
        );
        on_tool_call(raw, std::ptr::null_mut())
    }

    #[test]
    fn handler_vetoes_only_denied_calls() {
        assert_eq!(
            handler_return("bash", r#"{"command":"rm -rf /"}"#),
            EVENT_HANDLER_ABORT
        );
        assert_eq!(
            handler_return("bash", r#"{"command":"ls -la"}"#),
            EVENT_HANDLER_CONTINUE
        );
        assert_eq!(handler_return("bash", "not json"), EVENT_HANDLER_ABORT);
        // No connectors file in this process: every MCP call is refused.
        assert_eq!(
            handler_return("mcp_list", r#"{"configPath":"/x/mcp.json"}"#),
            EVENT_HANDLER_ABORT
        );
    }

    /// A1 carry-over: a connector call the gate could not classify (no server or no tool) is
    /// refused, never let through ungated.
    #[test]
    fn a_connector_call_naming_no_server_or_tool_is_refused() {
        for args in [
            json!({"configPath": "/x/mcp.json", "tool": "send", "arguments": {}}),
            json!({"configPath": "/x/mcp.json", "server": "mail", "arguments": {}}),
            json!({"configPath": "/x/mcp.json", "server": "", "tool": "send"}),
            json!({"configPath": "/x/mcp.json", "server": "mail", "tool": 7}),
        ] {
            assert!(
                matches!(check("mcp_call", &args), Decision::Deny(_)),
                "{args}"
            );
        }
        let named = json!({"configPath": "/x/mcp.json", "server": "mail", "tool": "send"});
        assert_eq!(check("mcp_call", &named), Decision::Allow);
    }

    /// The gate tests set the process's environment, which every test thread shares.
    static GATE_ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

    const SEND: &str = r#"{"command":"himalaya message send <<'EOF'\nTo: a@x.org\n\nhi\nEOF"}"#;

    fn gate_env(socket: Option<&Path>) -> std::sync::MutexGuard<'static, ()> {
        let held = GATE_ENV.lock().unwrap_or_else(|e| e.into_inner());
        match socket {
            Some(p) => std::env::set_var(tools::GATE_SOCKET_ENV, p),
            None => std::env::remove_var(tools::GATE_SOCKET_ENV),
        }
        std::env::set_var(tools::SESSION_ENV, "task-7");
        held
    }

    /// A gate that reads one request and answers `answer` (none: it hangs up), handing the
    /// request back; `Null` when nothing asked within 10 s.
    fn one_answer(
        name: &str,
        answer: Option<&'static str>,
    ) -> (PathBuf, std::thread::JoinHandle<Value>) {
        use std::io::{BufRead, BufReader};
        let dir = std::env::temp_dir().join(format!("zephyr-policy-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("gate.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let served = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(_) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    Err(_) => return Value::Null,
                }
            };
            stream.set_nonblocking(false).unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            if let Some(answer) = answer {
                writeln!(stream, "{answer}").unwrap();
            }
            serde_json::from_str(&line).unwrap()
        });
        (socket, served)
    }

    #[test]
    fn the_handler_aborts_a_gated_call_when_the_gate_is_down() {
        // No gate named: a send and an outgoing command are refused.
        let held = gate_env(None);
        assert_eq!(handler_return("bash", SEND), EVENT_HANDLER_ABORT);
        let post = r#"{"command":"curl -X POST -d @notes https://x.org"}"#;
        assert_eq!(handler_return("bash", post), EVENT_HANDLER_ABORT);
        // A command that sends nothing never asks.
        assert_eq!(
            handler_return("bash", r#"{"command":"ls -la"}"#),
            EVENT_HANDLER_CONTINUE
        );
        drop(held);
        // A gate named but not there.
        let missing = std::env::temp_dir().join("zephyr-policy-no-gate/gate.sock");
        let held = gate_env(Some(&missing));
        assert_eq!(handler_return("bash", SEND), EVENT_HANDLER_ABORT);
        drop(held);
        // A gate that hangs up, one that holds, one that answers nonsense.
        for (name, answer) in [
            ("hangup", None),
            (
                "hold",
                Some(r#"{"verdict":"hold","id":1,"text":"held for the owner as #1"}"#),
            ),
            ("nonsense", Some("allow")),
        ] {
            let (socket, served) = one_answer(name, answer);
            let held = gate_env(Some(&socket));
            assert_eq!(handler_return("bash", SEND), EVENT_HANDLER_ABORT, "{name}");
            // It asked, and was refused for the answer.
            assert_eq!(served.join().unwrap()["tool"], "bash", "{name}");
            drop(held);
        }
    }

    #[test]
    fn the_handler_continues_when_the_gate_allows() {
        let (socket, served) = one_answer("allow", Some(r#"{"verdict":"allow"}"#));
        let held = gate_env(Some(&socket));
        assert_eq!(handler_return("bash", SEND), EVENT_HANDLER_CONTINUE);
        drop(held);
        // The request names the session, the call and its arguments as rpi gave them.
        let req = served.join().unwrap();
        assert_eq!(req["v"], 1);
        assert_eq!(req["session"], "task-7");
        assert_eq!(req["call_id"], "call-1");
        assert_eq!(req["tool"], "bash");
        assert_eq!(req["args"], serde_json::from_str::<Value>(SEND).unwrap());
    }

    #[test]
    fn a_gate_that_says_nothing_is_given_up_on() {
        let dir = std::env::temp_dir().join(format!("zephyr-policy-silent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("gate.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let started = std::time::Instant::now();
        let asked = gate_client::ask(
            &socket,
            &json!({"v": 1}),
            std::time::Duration::from_millis(300),
        );
        assert!(asked.is_err(), "{asked:?}");
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        drop(listener);
    }

    #[test]
    fn redacts_secret_keys_and_command_assignments() {
        let (args, truncated) = summarize(
            r#"{"command":"export GITHUB_TOKEN=abc123 && run --api-key sk-1 x","nested":{"password":"p"},"path":"a"}"#,
        );
        assert!(!truncated);
        assert!(!args.contains("abc123"), "{args}");
        assert!(!args.contains("sk-1"), "{args}");
        assert!(!args.contains("\"p\""), "{args}");
        assert!(args.contains("GITHUB_TOKEN=[REDACTED]"), "{args}");
        assert!(args.contains("\"path\":\"a\""), "{args}");
    }

    #[test]
    fn redacts_bearer_tokens_in_command_text() {
        let (args, _) = summarize(
            r#"{"command":"curl -H 'Authorization: Bearer sk-live-123' https://x && echo Bearer tok-456"}"#,
        );
        assert!(!args.contains("sk-live-123"), "{args}");
        assert!(!args.contains("tok-456"), "{args}");
        assert!(args.contains("Bearer [REDACTED]"), "{args}");
    }

    #[test]
    fn status_command_returns_message_json() {
        let mut out = StbString::empty();
        let rc = status_command(StbStringRef::empty(), &mut out, std::ptr::null_mut());
        assert_eq!(rc, 0);
        let text = unsafe { out.to_string_lossy() };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["kind"], "message");
        assert_eq!(value["text"], "zephyr-policy active");
        unsafe { out.free_with(Some(rpi_plugin_sdk::callbacks::free_string)) };
    }

    #[test]
    fn args_summary_is_capped_at_char_boundary() {
        let params = json!({ "content": "é".repeat(2000) }).to_string();
        let (args, truncated) = summarize(&params);
        assert!(truncated);
        assert!(args.len() <= ARGS_LIMIT);
        assert!(args.is_char_boundary(args.len()));
    }

    #[test]
    fn audit_writes_one_json_line_per_call() {
        let dir = std::env::temp_dir().join(format!("zephyr-policy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("audit.jsonl");
        let _ = std::fs::remove_file(&log);
        audit(
            Some(&log),
            "bash",
            r#"{"command":"rm -rf /"}"#,
            &Decision::Deny("recursive rm of a system or home root"),
        );
        audit(Some(&log), "bash", r#"{"command":"ls"}"#, &Decision::Allow);
        audit(None, "bash", "{}", &Decision::Allow);
        let text = std::fs::read_to_string(&log).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["tool"], "bash");
        assert_eq!(lines[0]["decision"], "deny");
        assert_eq!(lines[1]["decision"], "allow");
        assert!(lines[0]["ts_ms"].as_u64().unwrap() > 0);
        assert!(lines[0]["args"].as_str().unwrap().contains("rm -rf /"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
