//! What the agent's processes may see (DESIGN §4.4). Every process zephyr starts for the agent
//! (rpi children, `rpi auth refresh`, `rpi --list-models`, the git it runs on memory) goes
//! through `cmd`.

use std::process::Command;
use std::sync::OnceLock;

/// The secrets.env variables the agent's processes must not inherit.
static HIDDEN: OnceLock<Vec<String>> = OnceLock::new();

/// Once, at start: `set` are the variables secrets.env set (`dotenv::load`), and `pass` the ones
/// `[agent] env` lets through. Changing `[agent] env` re-execs the daemon, which calls this again.
pub fn init(set: &[String], pass: &[String]) {
    let hidden = set.iter().filter(|k| !pass.contains(k)).cloned().collect();
    let _ = HIDDEN.set(hidden);
}

/// Prepares `cmd` to run for the agent: without the hidden variables.
pub fn cmd(cmd: &mut Command) -> &mut Command {
    for key in HIDDEN.get().into_iter().flatten() {
        cmd.env_remove(key);
    }
    cmd
}

/// Makes this process unreadable to others of its uid: `/proc/<pid>/environ` and `mem` then
/// belong to root, so an agent running as the same uid cannot read the secrets in our environment.
pub fn undumpable() {
    // SAFETY: PR_SET_DUMPABLE takes plain integer arguments and touches no memory of ours.
    unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
}
