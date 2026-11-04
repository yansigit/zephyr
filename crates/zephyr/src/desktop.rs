//! The agent's X desktop (DESIGN §4.11): Xvfb plus a window manager on `$ZEPHYR_DESKTOP`, run as
//! the agent. The image sets the variable; without it (tests, the host) there is no desktop.
//!
//! The server requires an MIT-MAGIC-COOKIE (`-auth`). With `--network host` the container shares
//! the host's abstract socket namespace, so Xvfb's `@/tmp/.X11-unix/X<n>` is reachable by any
//! process on the host, and abstract sockets have no file permissions: without the cookie, anyone
//! there could watch or drive the agent's signed-in browser. The server keeps holding that name,
//! so nobody else can claim it and have the agent's clients (which try it first) draw on theirs.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;

static RUNNING: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// The cookie file, in the agent's home: the agent's own clients read it through `XAUTHORITY`.
pub(crate) const XAUTHORITY: &str = ".zephyr-xauthority";
const COOKIE_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// The display children should use, after starting (or restarting) what serves it. A server left
/// over from before a re-exec or a crash keeps the display; the new one then exits at once, which
/// is harmless, and the cookie file is reused, so the old server still lets the agent in.
pub fn ensure() -> Option<String> {
    let display = std::env::var("ZEPHYR_DESKTOP").ok()?;
    let auth = xauthority()?;
    if let Err(e) = cookie(&auth, &display) {
        eprintln!("zephyr: the desktop's cookie in {}: {e}", auth.display());
        return None; // never a server anyone on the host could reach
    }
    let mut running = RUNNING.lock().unwrap();
    running.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
    if running.len() < 2 {
        for c in running.iter_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
        running.clear();
        let auth = auth.to_string_lossy().into_owned();
        let xvfb = [
            "-auth",
            &auth,
            "-screen",
            "0",
            "1920x1080x24",
            "-nolisten",
            "tcp",
        ];
        running.extend(start("Xvfb", &[&display], &xvfb));
        // The window manager needs the server up first.
        std::thread::sleep(std::time::Duration::from_millis(300));
        running.extend(start("openbox", &[], &[]));
    }
    Some(display)
}

/// Where the desktop's cookie is, when there is a desktop: set it as `XAUTHORITY` beside `DISPLAY`.
pub fn xauthority() -> Option<PathBuf> {
    std::env::var("ZEPHYR_DESKTOP").ok()?;
    Some(PathBuf::from(std::env::var_os("HOME")?).join(XAUTHORITY))
}

/// Makes sure `path` holds a cookie for `display`, as the agent and mode 0600. An existing one is
/// kept, so a server still running from a previous daemon keeps accepting the agent's clients.
fn cookie(path: &std::path::Path, display: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let number = display.trim_start_matches(':');
    let _fs = crate::agent::fs();
    if std::fs::read(path).is_ok_and(|b| parse(&b).is_some_and(|(n, _)| n == number)) {
        return Ok(());
    }
    let mut secret = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut secret)?;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&entry(number, &secret))?;
    f.sync_all()?;
    std::fs::rename(&tmp, path)
}

/// One Xauthority entry: any host (`FamilyWild`), display `number`, an MIT-MAGIC-COOKIE.
fn entry(number: &str, secret: &[u8; 16]) -> Vec<u8> {
    let mut out = 0xffffu16.to_be_bytes().to_vec();
    for field in [&b""[..], number.as_bytes(), COOKIE_NAME, secret] {
        out.extend((field.len() as u16).to_be_bytes());
        out.extend(field);
    }
    out
}

/// `(display number, secret)` of a file that is exactly one `entry`, else `None`.
fn parse(bytes: &[u8]) -> Option<(String, Vec<u8>)> {
    let (family, mut rest) = bytes.split_at_checked(2)?;
    if family != [0xff, 0xff] {
        return None;
    }
    let mut fields = Vec::new();
    for _ in 0..4 {
        let (len, tail) = rest.split_at_checked(2)?;
        let len = u16::from_be_bytes([len[0], len[1]]) as usize;
        let (field, tail) = tail.split_at_checked(len)?;
        fields.push(field);
        rest = tail;
    }
    let ok = rest.is_empty() && fields[0].is_empty() && fields[2] == COOKIE_NAME;
    let ok = ok && fields[3].len() == 16;
    ok.then(|| {
        (
            String::from_utf8_lossy(fields[1]).into_owned(),
            fields[3].to_vec(),
        )
    })
}

fn start(program: &str, first: &[&str], rest: &[&str]) -> Option<Child> {
    let display = std::env::var("ZEPHYR_DESKTOP").ok()?;
    crate::agent::cmd(&mut Command::new(program))
        .args(first)
        .args(rest)
        .env("DISPLAY", display)
        .env("XAUTHORITY", xauthority()?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| eprintln!("zephyr: starting {program} for the desktop: {e}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cookie_file_is_one_private_entry_and_is_kept() {
        let dir = crate::test_dir("desktop-cookie");
        let path = dir.join(XAUTHORITY);
        cookie(&path, ":99").unwrap();
        let first = std::fs::read(&path).unwrap();
        let (number, secret) = parse(&first).expect("one MIT-MAGIC-COOKIE entry");
        assert_eq!((number.as_str(), secret.len()), ("99", 16));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // Kept: a server from a previous daemon still accepts the agent's clients.
        cookie(&path, ":99").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), first);
        // Another display, or a damaged file, gets a fresh one.
        cookie(&path, ":98").unwrap();
        assert_eq!(parse(&std::fs::read(&path).unwrap()).unwrap().0, "98");
        std::fs::write(&path, b"junk").unwrap();
        cookie(&path, ":98").unwrap();
        assert!(parse(&std::fs::read(&path).unwrap()).is_some());
        assert!(
            parse(&entry("1", &[7; 16])[..10]).is_none(),
            "a cut file is refused"
        );
    }
}
