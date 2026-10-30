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

/// Prepares `cmd` to run for the agent: without the hidden variables, and as the agent's uid.
pub fn cmd(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    for key in HIDDEN.get().into_iter().flatten() {
        cmd.env_remove(key);
    }
    if let Some((uid, gid)) = ids() {
        // std drops supplementary groups and changes directory after these.
        cmd.uid(uid).gid(gid);
    }
    cmd
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
