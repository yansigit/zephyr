//! `zephyr-policy`: rpi Rust-native plugin. Vetoes hardline tool calls on the
//! `ToolCall` event and appends one audit line per call (DESIGN.md §4.4 items 2, 6).
//!
//! The host turns any non-zero handler return into a block, so a panic or a
//! malformed event fails closed. The host shows only a generic block message
//! to the model; the reason string reaches the audit log only.

// `export_plugin!` expands to a safe `extern "C"` fn taking the host's `api` pointer;
// the SDK's `register_entrypoint` checks it before any dereference.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

pub mod policy;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rpi_plugin_sdk::{
    event_handler, export_plugin, EventTag, EVENT_HANDLER_ABORT, EVENT_HANDLER_CONTINUE,
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
        let decision = match serde_json::from_str::<Value>(&call.params) {
            Ok(args) => check(&call.tool_name, &args),
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

export_plugin!(|api| {
    let Some(register) = api.register_event_handler else {
        return -1;
    };
    register(EventTag::ToolCall, on_tool_call, std::ptr::null_mut())
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

/// Redacts `NAME=value` words and the word after a `--secret-flag`, in command text.
fn redact_words(text: &str) -> String {
    let mut after_flag = false;
    text.split(' ')
        .map(|word| {
            let redact_this = std::mem::take(&mut after_flag);
            if let Some((name, _)) = word.split_once('=') {
                if is_secret_name(name) {
                    return format!("{name}={}", REDACTED);
                }
            }
            if word.starts_with('-') && is_secret_name(word) {
                after_flag = true;
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
    use rpi_plugin_sdk::{StablePluginEvent, StbString, StbStringRef};
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
