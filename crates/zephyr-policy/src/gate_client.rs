//! Asking the daemon's gate socket about a call (approvals spec §4.2 path 2): one JSON request
//! line on a connection of its own, one JSON answer line back. Std only, no runtime: the plugin's
//! `ToolCall` handler is synchronous.

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

/// The longest answer read: the daemon's are a verdict, an id and one line of text.
const MAX_ANSWER: u64 = 64 * 1024;

/// Sends `req` to the gate at `socket` and returns its answer, within `timeout` in all. `Err` when
/// there is no gate, it does not answer in time, or the answer is not JSON: the caller refuses
/// the call then, as for any answer but `allow`.
///
/// The exchange runs on a thread of its own, so even a `connect` that waits for room in a gate's
/// full listen queue never holds the call past `timeout`; the thread itself ends within
/// `timeout` of its last read or write, or when the daemon goes.
pub fn ask(socket: &Path, req: &Value, timeout: Duration) -> Result<Value, String> {
    let deadline = Instant::now() + timeout;
    let (tx, rx) = std::sync::mpsc::channel();
    let (socket, line) = (socket.to_path_buf(), format!("{req}\n"));
    std::thread::Builder::new()
        .name("zephyr-gate".into())
        .spawn(move || {
            let _ = tx.send(exchange(&socket, &line, deadline));
        })
        .map_err(|e| format!("no thread to ask the gate on: {e}"))?;
    match rx.recv_timeout(timeout) {
        Ok(answer) => answer,
        Err(_) => Err("the gate did not answer in time".into()),
    }
}

fn exchange(socket: &Path, line: &str, deadline: Instant) -> Result<Value, String> {
    let left = || {
        let left = deadline.saturating_duration_since(Instant::now());
        // A zero timeout means none at all to std: never pass one.
        (!left.is_zero()).then_some(left).ok_or("out of time")
    };
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("no gate: {e}"))?;
    stream
        .set_write_timeout(Some(left()?))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(line.as_bytes())
        .map_err(|e| format!("the gate took no request: {e}"))?;
    let mut answer = Vec::new();
    let mut reader = BufReader::new(&stream).take(MAX_ANSWER);
    loop {
        stream
            .set_read_timeout(Some(left()?))
            .map_err(|e| e.to_string())?;
        let buf = reader.fill_buf().map_err(|e| format!("no answer: {e}"))?;
        if buf.is_empty() {
            break; // the gate hung up
        }
        let (got, done) = match buf.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (buf.len(), false),
        };
        answer.extend_from_slice(&buf[..got]);
        reader.consume(got);
        if done {
            break;
        }
    }
    serde_json::from_slice(&answer).map_err(|_| "the gate's answer is not JSON".into())
}
