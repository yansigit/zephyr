//! Tools whose work the daemon does (DESIGN §4.11): `send_file` and `schedule`. Here they only
//! check their arguments and answer at once. The daemon reads each call from rpi's event stream,
//! which the agent cannot write, and acts on it when the turn settles.

use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{
    FreeStringFn, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolExecuteFn,
    ToolPartialCb,
};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::path::Path;

/// Discord's upload limit for a bot in a server without boosts, and so the cap for every channel.
pub const MAX_FILE: u64 = 10 * 1024 * 1024;

/// Each call's answer, made in `execute` and handed over by the first `poll`.
static HANDLES: ToolHandles<Option<Result<String, String>>> = ToolHandles::new();

pub const TOOLS: [(&str, &str, &str, ToolExecuteFn); 2] = [
    (
        "send_file",
        "Send a file of up to 10 MB (a screenshot, a PDF) to the owner, on the channel you are \
         answering. It is delivered with your reply.",
        r#"{"type":"object","properties":{"path":{"type":"string","description":"The file, absolute or relative to your working directory"},"caption":{"type":"string"}},"required":["path"]}"#,
        send_file_execute,
    ),
    (
        "schedule",
        "Propose a scheduled job. `schedule` is a five-field cron expression (in the owner's time \
         zone) or an RFC 3339 time; `prompt` is what a fresh session is asked each run, and its \
         answer goes to the channel you are answering. The job runs only after the owner approves \
         it with /job ok.",
        r#"{"type":"object","properties":{"name":{"type":"string"},"schedule":{"type":"string"},"prompt":{"type":"string"}},"required":["name","schedule","prompt"]}"#,
        schedule_execute,
    ),
];

pub fn schema(name: &str, description: &str, parameters: &str) -> StableToolSchema {
    StableToolSchema {
        name: StbString::from_string(name.into()),
        description: StbString::from_string(description.into()),
        parameters: StbString::from_string(parameters.into()),
    }
}

/// `send_file`'s check, in the child (the agent's uid and cwd). The daemon checks again when it
/// reads the file.
pub fn send_file(params: &Value) -> Result<String, String> {
    let path = text(params, "path")?;
    let full = match params["__rpi"]["cwd"].as_str() {
        Some(cwd) if Path::new(path).is_relative() => Path::new(cwd).join(path),
        _ => Path::new(path).to_path_buf(),
    };
    let meta = std::fs::metadata(&full).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_file() {
        return Err(format!("{path} is not a file"));
    }
    if meta.len() > MAX_FILE {
        return Err(format!(
            "{path} is {} bytes; the limit is {MAX_FILE}",
            meta.len()
        ));
    }
    Ok(format!("{path} will be sent to the owner with your reply"))
}

/// `schedule`'s check. The daemon parses the schedule; a bad one is reported to the owner.
pub fn schedule(params: &Value) -> Result<String, String> {
    for key in ["name", "schedule", "prompt"] {
        if text(params, key)?.trim().is_empty() {
            return Err(format!("`{key}` is empty"));
        }
    }
    Ok("proposed; it runs only after the owner approves it with /job ok".into())
}

fn text<'a>(params: &'a Value, key: &str) -> Result<&'a str, String> {
    params[key]
        .as_str()
        .ok_or_else(|| format!("missing string parameter `{key}`"))
}

extern "C" fn send_file_execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    execute(params, free, send_file)
}

extern "C" fn schedule_execute(
    _: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    execute(params, free, schedule)
}

fn execute(
    params: StbString,
    free: Option<FreeStringFn>,
    check: fn(&Value) -> Result<String, String>,
) -> StepHandle {
    // SAFETY: the host hands over a live JSON string it owns until we free it with `free`, once.
    let raw = unsafe { params.to_string_lossy() };
    unsafe { params.free_with(free) };
    let answer = std::panic::catch_unwind(|| {
        serde_json::from_str::<Value>(&raw)
            .map_err(|e| format!("invalid parameters: {e}"))
            .and_then(|p| check(&p))
            .map(|t| json!({ "content": [{ "type": "text", "text": t }] }).to_string())
    })
    .unwrap_or_else(|_| Err("the tool panicked".into()));
    HANDLES.insert(Some(answer))
}

pub extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    let answer = HANDLES.get(handle).and_then(|entry| entry.lock().take());
    match answer {
        Some(Ok(result)) => StepResult::done(StbString::from_string(result)),
        Some(Err(message)) => StepResult::err(StbString::from_string(message)),
        None => StepResult::err(StbString::from_string(
            "invalid or finished tool handle".into(),
        )),
    }
}

pub extern "C" fn cancel(handle: StepHandle) {
    HANDLES.cancel(handle);
}

pub extern "C" fn destroy(handle: StepHandle) {
    HANDLES.remove(handle);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn send_file_needs_a_small_existing_file() {
        let dir = std::env::temp_dir().join(format!("zephyr-tools-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.png"), b"png").unwrap();
        let cwd = json!({ "cwd": dir });
        assert!(send_file(&json!({ "path": "a.png", "__rpi": cwd })).is_ok());
        assert!(send_file(&json!({ "path": dir.join("a.png") })).is_ok());
        assert!(send_file(&json!({ "path": "missing.png", "__rpi": cwd })).is_err());
        assert!(send_file(&json!({ "path": dir }))
            .unwrap_err()
            .contains("not a file"));
        assert!(send_file(&json!({})).is_err());
        let big = std::fs::File::create(dir.join("big")).unwrap();
        big.set_len(MAX_FILE + 1).unwrap();
        assert!(send_file(&json!({ "path": "big", "__rpi": cwd }))
            .unwrap_err()
            .contains("limit"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn schedule_needs_every_field() {
        let ok = json!({ "name": "n", "schedule": "0 9 * * *", "prompt": "p" });
        assert!(schedule(&ok).unwrap().contains("/job ok"));
        assert!(schedule(&json!({ "name": "n", "schedule": " ", "prompt": "p" })).is_err());
        assert!(schedule(&json!({ "name": "n", "prompt": "p" })).is_err());
    }

    #[test]
    fn a_handle_answers_once() {
        let handle = execute(StbString::from_string("{\"x\":1}".into()), None, |_| {
            Ok("ok".into())
        });
        let first = poll(handle, None, std::ptr::null_mut());
        assert!(matches!(first.tag, rpi_plugin_sdk::StepResultTag::Done));
        let again = poll(handle, None, std::ptr::null_mut());
        assert!(matches!(again.tag, rpi_plugin_sdk::StepResultTag::Err));
        destroy(handle);
        destroy(handle);
    }
}
