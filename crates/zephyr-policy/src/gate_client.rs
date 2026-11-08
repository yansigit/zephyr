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
/// The socket counts as the gate only when the daemon serves it ([`trusted`]): the path comes
/// from the environment, which the agent sets for its own commands (I2), so a listener the agent
/// started is refused before anything is sent to it.
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
    let peer = peer_uid(&stream).map_err(|e| format!("no gate: its uid cannot be read: {e}"))?;
    if !trusted(peer, program_owner()) {
        return Err(format!(
            "no gate: the socket is served by uid {peer}, not by the daemon"
        ));
    }
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

/// Whether a gate socket served by uid `peer` may answer for the daemon: root's, as the daemon in
/// the container is, or the uid that owns the program asking (`owner`, `/proc/self/exe`'s), which
/// in the container is root too (`/usr/local/bin/zephyr`, `/usr/local/bin/rpi`), and elsewhere
/// is the user whose build runs both sides (tests, CI, a developer's machine), where no uid
/// separates the agent from the daemon. An owner that cannot be read trusts root alone.
pub fn trusted(peer: u32, owner: Option<u32>) -> bool {
    peer == 0 || owner == Some(peer)
}

/// The uid of the process that listens on `stream`'s other end (`SO_PEERCRED`, set when it
/// listened; std's `peer_cred` is unstable).
fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let size = std::mem::size_of::<libc::ucred>();
    let mut len = size as libc::socklen_t;
    // SAFETY: `stream` holds an open socket for the whole call, and getsockopt writes at most
    // `len` bytes into `cred`, a `ucred` of exactly that size that this frame owns.
    let r = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if r != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len as usize != size {
        return Err(std::io::Error::other("a short SO_PEERCRED answer"));
    }
    Ok(cred.uid)
}

/// Who owns the program running this: the binary `/proc/self/exe` names.
fn program_owner() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self/exe").ok().map(|m| m.uid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::os::unix::process::CommandExt;

    /// I2: root's daemon answers for the gate; so does the uid that owns the program asking,
    /// where no uid splits the agent from the daemon (tests, a developer's build). The agent's
    /// own listener, under root's program, does not.
    #[test]
    fn only_root_or_the_programs_owner_answers_for_the_gate() {
        assert!(trusted(0, Some(0)), "root's daemon, root's program");
        assert!(
            trusted(0, Some(1000)),
            "root's daemon, whoever owns the program"
        );
        assert!(
            trusted(1000, Some(1000)),
            "the program's owner: no uid split"
        );
        assert!(
            !trusted(1000, Some(0)),
            "the agent's listener, root's program"
        );
        assert!(!trusted(65534, Some(1000)));
        assert!(!trusted(1000, None), "no owner read: root only");
    }

    /// I2: a socket another uid serves (the agent's own listener at a path it named) is no gate,
    /// whatever it answers. Root only: a listener of another uid needs one to start it.
    #[test]
    fn a_listener_of_another_uid_is_no_gate() {
        // SAFETY: geteuid takes no arguments and cannot fail.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skipped: only root can start a listener as another uid");
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("zp-gate-peer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let socket = dir.join("gate.sock");
        let script = "import os, socket, sys\n\
                      s = socket.socket(socket.AF_UNIX)\n\
                      s.bind(sys.argv[1])\n\
                      s.listen(4)\n\
                      print('up', flush=True)\n\
                      while True:\n    \
                          c, _ = s.accept()\n    \
                          c.recv(65536)\n    \
                          c.sendall(b'{\"verdict\": \"allow\"}\\n')\n    \
                          c.close()\n";
        let mut child = std::process::Command::new("python3")
            .args(["-I", "-c", script])
            .arg(&socket)
            .current_dir(&dir)
            .stdout(std::process::Stdio::piped())
            .uid(65534)
            .gid(65534)
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "up");
        let answer = ask(
            &socket,
            &json!({"v": 1, "tool": "browser"}),
            Duration::from_secs(5),
        );
        let _ = child.kill();
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        let err = answer.expect_err("an allow from uid 65534 was taken for the gate's");
        assert!(err.contains("65534"), "{err}");
    }
}
