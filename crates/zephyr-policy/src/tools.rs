//! Tools whose work the daemon does (DESIGN §4.11): `send_file`, `schedule` and `gateway`. The
//! first two only check their arguments here and answer at once; the daemon reads each call from
//! rpi's event stream, which the agent cannot write, and acts on it when the turn settles.
//! `gateway` waits instead: the daemon answers it from the same stream mid-turn, through a file.

use rpi_plugin_sdk::handles::ToolHandles;
use rpi_plugin_sdk::{
    FreeStringFn, StableToolSchema, StbString, StbStringRef, StepHandle, StepResult, ToolExecuteFn,
    ToolPartialCb,
};
use serde_json::{json, Value};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

/// Discord's upload limit for a bot in a server without boosts, and so the cap for every channel.
pub const MAX_FILE: u64 = 10 * 1024 * 1024;

/// The read-only slash commands the agent may run through `gateway`. The owner's approvals
/// (`/memory ok`, `/job ok`, `/rule`, `/login`, `/resume`, `/ok`) stay out, so injected text
/// cannot approve itself (DESIGN §4.5, §4.6). The daemon checks [`gateway_kind`] again.
pub const GATEWAY_COMMANDS: &[&str] = &[
    "/status",
    "/model",
    "/switch",
    "/rules",
    "/memory diff",
    "/memory log",
    "/job list",
];

/// What a `gateway` command is to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gateway {
    /// Runs at once and returns its output.
    Read,
    /// An action the daemon only stores, until the owner's `/ok N` runs it.
    Propose,
}

/// `Some` for a command the agent may send through `gateway`, in these exact shapes only:
/// [`GATEWAY_COMMANDS`] are read; `/model PROVIDER/MODEL`, `/new`, `/switch ID`, `/pause` and
/// `/reload` are proposed. Approvals, and `/stop`, are neither. The argument is one printable
/// ASCII word, so the owner's approval notice shows exactly what would run.
pub fn gateway_kind(command: &str) -> Option<Gateway> {
    if GATEWAY_COMMANDS.contains(&command) {
        return Some(Gateway::Read);
    }
    let (cmd, arg) = command.split_once(' ').unwrap_or((command, ""));
    let word = !arg.is_empty() && arg.bytes().all(|b| b.is_ascii_graphic());
    let ok = match cmd {
        "/new" | "/pause" | "/reload" => command == cmd,
        "/switch" => word,
        "/model" => {
            word && arg
                .split_once('/')
                .is_some_and(|(p, m)| !p.is_empty() && !m.is_empty())
        }
        _ => false,
    };
    ok.then_some(Gateway::Propose)
}

/// Where the daemon drops each `gateway` answer, as `<dir>/<tool call id>`.
pub const GATEWAY_DIR_ENV: &str = "ZEPHYR_GATEWAY_DIR";

/// How long a `gateway` call waits for the daemon.
const GATEWAY_WAIT: Duration = Duration::from_secs(10);

/// A tool call's state between `execute` and its last `poll`.
enum Step {
    /// Made in `execute`, handed over by the first `poll`.
    Answer(Result<String, String>),
    /// A `gateway` call waiting for the daemon's file.
    Waiting {
        path: PathBuf,
        deadline: Instant,
    },
    Finished,
}

static HANDLES: ToolHandles<Step> = ToolHandles::new();

pub const TOOLS: [(&str, &str, &str, ToolExecuteFn); 3] = [
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
    (
        "gateway",
        "Run one of Zephyr's read-only slash commands and get its output: /status (account, \
         model, channels, spend), /model (the models you can switch to), /switch (saved \
         conversations), /rules (the owner's standing rules), /memory diff (a memory change \
         waiting for the owner), /memory log (recent memory commits), /job list (scheduled jobs). \
         Or propose one of these actions for the owner: /model PROVIDER/MODEL, /new, /switch ID, \
         /pause, /reload. A proposal runs only after the owner approves it with /ok; approving \
         anything is always the owner's to type.",
        r#"{"type":"object","properties":{"command":{"type":"string","description":"The command, exactly as listed, e.g. /status"}},"required":["command"]}"#,
        gateway_execute,
    ),
];

/// `gateway`'s check: the command must have a [`gateway_kind`].
pub fn gateway(params: &Value) -> Result<String, String> {
    let command = text(params, "command")?.trim();
    if gateway_kind(command).is_none() {
        return Err(format!(
            "{command} is not available to the agent; it can run {}, and propose \
             /model PROVIDER/MODEL, /new, /switch ID, /pause or /reload",
            GATEWAY_COMMANDS.join(", ")
        ));
    }
    Ok(command.into())
}

/// The answer file for a call, or `None` when the id could not be a plain file name.
pub fn gateway_file(dir: &Path, call_id: &str) -> Option<PathBuf> {
    let plain = !call_id.is_empty()
        && call_id.len() <= 128
        && call_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    plain.then(|| dir.join(call_id))
}

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

extern "C" fn gateway_execute(
    call_id: StbStringRef,
    params: StbString,
    free: Option<FreeStringFn>,
) -> StepHandle {
    // SAFETY: the host lends the id for this call only; it is copied out before returning.
    let id = std::panic::catch_unwind(|| unsafe { call_id.as_str() }.to_string());
    // SAFETY: as in `execute`.
    let raw = unsafe { params.to_string_lossy() };
    unsafe { params.free_with(free) };
    let dir = std::env::var_os(GATEWAY_DIR_ENV).filter(|d| !d.is_empty());
    HANDLES.insert(gateway_step(
        &raw,
        id.ok().as_deref(),
        dir.map(PathBuf::from),
    ))
}

/// What a `gateway` call does: wait for the daemon's answer, or fail at once.
fn gateway_step(raw: &str, call_id: Option<&str>, dir: Option<PathBuf>) -> Step {
    let checked = std::panic::catch_unwind(|| {
        serde_json::from_str::<Value>(raw)
            .map_err(|e| format!("invalid parameters: {e}"))
            .and_then(|p| gateway(&p))
    })
    .unwrap_or_else(|_| Err("the tool panicked".into()));
    if let Err(e) = checked {
        return Step::Answer(Err(e));
    }
    let Some(dir) = dir else {
        return Step::Answer(Err("gateway is only available inside Zephyr".into()));
    };
    match call_id.and_then(|id| gateway_file(&dir, id)) {
        Some(path) => Step::Waiting {
            path,
            deadline: Instant::now() + GATEWAY_WAIT,
        },
        None => Step::Answer(Err("unusable tool call id".into())),
    }
}

fn result(text: &str) -> String {
    json!({ "content": [{ "type": "text", "text": text }] }).to_string()
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
            .map(|t| result(&t))
    })
    .unwrap_or_else(|_| Err("the tool panicked".into()));
    HANDLES.insert(Step::Answer(answer))
}

pub extern "C" fn poll(handle: StepHandle, _: Option<ToolPartialCb>, _: *mut c_void) -> StepResult {
    let err = |m: &str| StepResult::err(StbString::from_string(m.into()));
    let Some(entry) = HANDLES.get(handle) else {
        return err("invalid or finished tool handle");
    };
    let mut state = entry.lock();
    match std::mem::replace(&mut *state, Step::Finished) {
        Step::Answer(Ok(result)) => StepResult::done(StbString::from_string(result)),
        Step::Answer(Err(message)) => StepResult::err(StbString::from_string(message)),
        Step::Finished => err("invalid or finished tool handle"),
        Step::Waiting { .. } if entry.cancelled.load(Ordering::SeqCst) => err("cancelled"),
        Step::Waiting { path, deadline } => match std::fs::read_to_string(&path) {
            Ok(text) => {
                let _ = std::fs::remove_file(&path);
                StepResult::done(StbString::from_string(result(&text)))
            }
            Err(_) if Instant::now() >= deadline => err("the gateway did not answer"),
            Err(_) => {
                *state = Step::Waiting { path, deadline };
                drop(state);
                // shortcut: the host's driver calls poll in a loop, so a short sleep keeps it from
                // spinning; use a wake-up if the SDK grows one.
                std::thread::sleep(Duration::from_millis(20));
                StepResult::pending(StbString::from_string(String::new()))
            }
        },
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

    fn tag(handle: StepHandle) -> rpi_plugin_sdk::StepResultTag {
        poll(handle, None, std::ptr::null_mut()).tag
    }

    #[test]
    fn gateway_takes_only_the_listed_read_only_commands() {
        assert_eq!(
            gateway(&json!({ "command": " /status " })).unwrap(),
            "/status"
        );
        for bad in [
            "/memory ok",
            "/job ok 1",
            "/rule x",
            "/login",
            "/resume",
            "/status x",
        ] {
            assert!(gateway(&json!({ "command": bad })).is_err(), "{bad}");
        }
        assert!(gateway(&json!({})).is_err());
        let dir = Path::new("/d");
        assert_eq!(
            gateway_file(dir, "toolu_01-a"),
            Some(dir.join("toolu_01-a"))
        );
        for bad in ["", "../x", "a/b", "a.b"] {
            assert_eq!(gateway_file(dir, bad), None, "{bad:?}");
        }
    }

    #[test]
    fn gateway_reads_proposes_or_refuses() {
        for read in GATEWAY_COMMANDS {
            assert_eq!(gateway_kind(read), Some(Gateway::Read), "{read}");
        }
        for action in [
            "/model fake/m2",
            "/new",
            "/switch 1a2b",
            "/pause",
            "/reload",
        ] {
            assert_eq!(gateway_kind(action), Some(Gateway::Propose), "{action}");
        }
        for never in [
            "/memory ok",
            "/memory undo",
            "/job ok 1",
            "/job rm 1",
            "/job add a | b | c",
            "/job run 1",
            "/rule x",
            "/rule rm 1",
            "/login",
            "/login anthropic",
            "/resume",
            "/ok 1",
            "/no 1",
            "/stop",
            "/new x",
            "/new ",
            "/pause now",
            "/model fake",
            "/model /m2",
            "/model fake/ m2",
            "/model fake/m2 x",
            "/model fake/m\u{202e}2",
            "/switch ",
            "/switch a b",
            "hello",
        ] {
            assert_eq!(gateway_kind(never), None, "{never}");
        }
        assert_eq!(
            gateway(&json!({ "command": "/model fake/m2" })).unwrap(),
            "/model fake/m2"
        );
    }

    #[test]
    fn gateway_waits_for_the_daemons_file() {
        use rpi_plugin_sdk::StepResultTag::{Done, Err, Pending};
        let dir = std::env::temp_dir().join(format!("zephyr-gw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cmd = r#"{"command":"/status"}"#;

        let refused = HANDLES.insert(gateway_step(
            r#"{"command":"/memory ok"}"#,
            Some("c0"),
            Some(dir.clone()),
        ));
        assert!(matches!(tag(refused), Err));
        let outside = HANDLES.insert(gateway_step(cmd, Some("c0"), None));
        assert!(matches!(tag(outside), Err));

        let h = HANDLES.insert(gateway_step(cmd, Some("c1"), Some(dir.clone())));
        assert!(matches!(tag(h), Pending));
        std::fs::write(dir.join("c1"), "agent: on m").unwrap();
        let done = poll(h, None, std::ptr::null_mut());
        assert!(matches!(done.tag, Done));
        // SAFETY: tag is Done.
        let text = unsafe { done.payload.done.result.to_string_lossy() };
        assert!(text.contains("agent: on m"), "{text}");
        assert!(!dir.join("c1").exists(), "the answer file is consumed");
        assert!(matches!(tag(h), Err), "answers once");

        let cancelled = HANDLES.insert(gateway_step(cmd, Some("c2"), Some(dir.clone())));
        cancel(cancelled);
        assert!(matches!(tag(cancelled), Err));

        let late = HANDLES.insert(Step::Waiting {
            path: dir.join("c3"),
            deadline: Instant::now(),
        });
        assert!(matches!(tag(late), Err));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
