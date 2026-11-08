//! What the agent's processes may see and be (DESIGN §4.4). In the container the daemon runs as
//! root holding only SETUID, SETGID and KILL; the agent runs as the owner's uid, which can read
//! none of the daemon's files. Every process zephyr starts for the agent (rpi children,
//! `rpi auth refresh`, `rpi --list-models`, the git it runs on memory) goes through `cmd`, and
//! every file the daemon touches in the agent's home (`/zephyr`) is touched inside `fs`.
//! Outside the container (tests, a manual `zephyr run`) the uid parts do nothing.

use std::process::Command;
use std::sync::OnceLock;

/// The secrets.env variables the agent's processes must not inherit.
static HIDDEN: OnceLock<Vec<String>> = OnceLock::new();
/// The agent's `(uid, gid)` when zephyr runs as root.
static AGENT: OnceLock<Option<(u32, u32)>> = OnceLock::new();

/// Once, at start: `set` are the variables secrets.env set (`dotenv::load`), and `pass` the ones
/// `[agent] env` lets through. Changing `[agent] env` re-execs the daemon, which calls this again.
pub fn init(set: &[String], pass: &[String]) {
    let hidden = set.iter().filter(|k| !pass.contains(k)).cloned().collect();
    let _ = HIDDEN.set(hidden);
}

/// Once, before anything else. As root, reads the agent's uid and gid from
/// `ZEPHYR_AGENT_UID`/`ZEPHYR_AGENT_GID` (set by `zephyr up`) and drops supplementary groups.
pub fn init_ids() -> anyhow::Result<()> {
    // SAFETY: plain syscalls with no pointers, or a null list of length 0.
    let ids = if unsafe { libc::getuid() } == 0 {
        let id = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u32>().ok());
        match (id("ZEPHYR_AGENT_UID"), id("ZEPHYR_AGENT_GID")) {
            (Some(uid), Some(gid)) if uid != 0 => {
                if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
                    anyhow::bail!("dropping groups: {}", std::io::Error::last_os_error());
                }
                Some((uid, gid))
            }
            _ => None,
        }
    } else {
        None
    };
    let _ = AGENT.set(ids);
    Ok(())
}

/// The agent's `(uid, gid)`, when the daemon runs as root with one set.
pub fn ids() -> Option<(u32, u32)> {
    AGENT.get().copied().flatten()
}

/// The `PATH` of every process run for the agent. Only these get `/zephyr/.local/bin`, where the
/// agent installs its own tools: root's PATH (the image's) leaves it out, or the agent could plant
/// a `git` or `zephyr` there for root to run. `/opt/zephyr/gate/bin` (root's, holding only the
/// browser wrapper, `browser_gate`) comes first, so an `agent-browser` the agent puts in its own
/// bin does not shadow the wrapper (approvals spec §4.4; doctor checks it).
pub const AGENT_PATH: &str = "/opt/zephyr/gate/bin:/zephyr/.local/bin:/opt/zephyr/py/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Prepares `cmd` to run for the agent: without the hidden variables, on [`AGENT_PATH`], and as
/// the agent's uid.
pub fn cmd(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    for key in HIDDEN.get().into_iter().flatten() {
        cmd.env_remove(key);
    }
    cmd.env("PATH", AGENT_PATH);
    if let Some((uid, gid)) = ids() {
        // std drops supplementary groups and changes directory after these.
        cmd.uid(uid).gid(gid);
    }
    cmd
}

/// The directories on `path` (a `PATH` value) that `uid`/`gid` could put a program into: one it
/// owns or may write, or one under such a directory. On root's PATH, any of these lets the agent
/// plant a program root runs (doctor checks it).
pub fn writable_dirs(path: &str, uid: u32, gid: u32) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    let theirs = |m: &std::fs::Metadata| {
        m.uid() == uid || m.mode() & 0o002 != 0 || (m.gid() == gid && m.mode() & 0o020 != 0)
    };
    path.split(':')
        .filter(|d| {
            std::path::Path::new(d)
                .ancestors()
                .filter_map(|a| std::fs::metadata(a).ok())
                .any(|m| theirs(&m))
        })
        .map(String::from)
        .collect()
}

/// While held, this thread's file access is the agent's (setfsuid/setfsgid), so what the
/// daemon reads or writes in the agent's home is exactly what the agent itself could: a
/// symlink the agent plants there leads nowhere it could not go. Never hold it across an
/// `.await`: the task may resume on another thread.
#[must_use]
pub struct Fs(Option<(u32, u32)>);

pub fn fs() -> Fs {
    let Some((uid, gid)) = ids() else {
        return Fs(None);
    };
    // SAFETY: plain syscalls. Each returns the previous id; passing -1 changes nothing.
    let previous = unsafe { (libc::setfsuid(uid), libc::setfsgid(gid)) };
    let now = unsafe { (libc::setfsuid(u32::MAX), libc::setfsgid(u32::MAX)) };
    assert_eq!(
        now,
        (uid as i32, gid as i32),
        "setfsuid/setfsgid did not take"
    );
    Fs(Some((previous.0 as u32, previous.1 as u32)))
}

impl Drop for Fs {
    fn drop(&mut self) {
        if let Some((uid, gid)) = self.0 {
            // SAFETY: as in `fs`.
            unsafe {
                libc::setfsuid(uid);
                libc::setfsgid(gid);
            }
        }
    }
}

/// Makes this process unreadable to others of its uid: `/proc/<pid>/environ` and `mem` then
/// belong to root, so an agent running as the same uid cannot read the secrets in our environment.
pub fn undumpable() {
    // SAFETY: PR_SET_DUMPABLE takes plain integer arguments and touches no memory of ours.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_processes_get_the_agent_path() {
        let mut sh = Command::new("sh");
        sh.args(["-c", "printf %s \"$PATH\""]).env("PATH", "/x");
        let out = cmd(&mut sh).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), AGENT_PATH);
        // The browser wrapper comes first, so no agent-browser of the agent's own shadows it.
        let first = AGENT_PATH.split(':').next();
        assert_eq!(first, Some(crate::browser_gate::GATE_BIN));
        let own = AGENT_PATH
            .split(':')
            .position(|d| d == "/zephyr/.local/bin");
        assert_eq!(own, Some(1));
    }

    #[test]
    fn writable_dirs_finds_the_agents_own_and_what_lies_under_them() {
        let dir = crate::test_dir("agent-path");
        let sub = dir.join("bin");
        std::fs::create_dir(&sub).unwrap();
        // SAFETY: plain syscalls with no arguments.
        let (me, group) = unsafe { (libc::getuid(), libc::getgid()) };
        let path = format!("/usr/bin:{}", sub.display());
        assert_eq!(writable_dirs(&path, me, group), [sub.display().to_string()]);
        // Root's own dirs are no one else's.
        assert!(writable_dirs("/usr/bin:/bin", me + 1, group + 1).is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
