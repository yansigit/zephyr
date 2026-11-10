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

/// The daemon's proxy, the agent's only way out under the network floor (`egress::PORT`).
const PROXY: &str = "http://127.0.0.1:7792";

/// What points the agent's programs at the proxy (DESIGN §4.19). No `ALL_PROXY`: rpi 0.18
/// refuses a proxy URL that is not HTTP. Node's `fetch` ignores the rest without the last one.
pub const PROXY_ENV: &[(&str, &str)] = &[
    ("HTTP_PROXY", PROXY),
    ("HTTPS_PROXY", PROXY),
    ("http_proxy", PROXY),
    ("https_proxy", PROXY),
    ("NO_PROXY", "localhost,127.0.0.1,::1"),
    ("no_proxy", "localhost,127.0.0.1,::1"),
    ("NODE_USE_ENV_PROXY", "1"),
];

/// `AGENT_BROWSER_ARGS` with the proxy flag after `current`, so Chromium does not depend on
/// finding the variables itself.
pub fn browser_args(current: Option<String>) -> String {
    let flag = format!("--proxy-server={PROXY}");
    match current.filter(|c| !c.is_empty()) {
        Some(current) => format!("{current},{flag}"),
        None => flag,
    }
}

/// Everything that points a program at the proxy: [`PROXY_ENV`], and `AGENT_BROWSER_ARGS` with
/// the proxy flag after `browser`, its value so far. The one list for the daemon's spawns and
/// the host's `docker exec` as the agent.
pub fn proxy_env(browser: Option<String>) -> Vec<(&'static str, String)> {
    let vars = PROXY_ENV.iter().map(|(k, v)| (*k, v.to_string()));
    vars.chain([("AGENT_BROWSER_ARGS", browser_args(browser))])
        .collect()
}

/// Points `cmd` at the proxy ([`proxy_env`]).
pub fn proxy(cmd: &mut Command) -> &mut Command {
    cmd.envs(proxy_env(std::env::var("AGENT_BROWSER_ARGS").ok()))
}

/// Prepares `cmd` to run for the agent: without the hidden variables, on [`AGENT_PATH`], through
/// the proxy in bridge mode, and as the agent's uid.
pub fn cmd(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    for key in HIDDEN.get().into_iter().flatten() {
        cmd.env_remove(key);
    }
    cmd.env("PATH", AGENT_PATH);
    if crate::egress::bridge() {
        proxy(cmd);
    }
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

/// Why the agent (`uid`, `gid`) could replace or rewrite the program at `path` (B5), or `None`
/// when it cannot: the file (a link followed) must be `owner`'s (root's in the image) and
/// writable by no one else, and no folder above it, or above the link, the agent's or one it may
/// write; in a sticky folder (`/tmp`) only what the agent owns is its to replace. Doctor checks
/// rpi, zephyr, the policy plugin and the real agent-browser this way: a pinned tarball's
/// `./rpi` unpacked with its own owner (uid 1001) would be the agent's when the host user is.
pub fn replaceable(path: &std::path::Path, owner: u32, uid: u32, gid: u32) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let real = match std::fs::canonicalize(path) {
        Ok(real) => real,
        Err(e) => return Some(format!("{} cannot be read: {e}", path.display())),
    };
    let m = match std::fs::metadata(&real) {
        Ok(m) => m,
        Err(e) => return Some(format!("{} cannot be read: {e}", real.display())),
    };
    if m.uid() != owner {
        return Some(format!("{} is uid {}'s", real.display(), m.uid()));
    }
    if m.mode() & 0o022 != 0 {
        return Some(format!("{} may be written by others", real.display()));
    }
    let writes = |m: &std::fs::Metadata| {
        m.uid() == uid || m.mode() & 0o002 != 0 || (m.gid() == gid && m.mode() & 0o020 != 0)
    };
    for start in [path, real.as_path()] {
        // What the entry each folder holds belongs to, for the sticky bit.
        let mut child = std::fs::symlink_metadata(start).map_or(owner, |m| m.uid());
        for dir in start
            .ancestors()
            .skip(1)
            .filter(|d| !d.as_os_str().is_empty())
        {
            let Ok(d) = std::fs::metadata(dir) else {
                return Some(format!("{} cannot be read", dir.display()));
            };
            let sticky = d.mode() & 0o1000 != 0;
            if d.uid() == uid || (writes(&d) && (!sticky || child == uid)) {
                return Some(format!("{} is the agent's to write", dir.display()));
            }
            child = d.uid();
        }
    }
    None
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

    fn envs(cmd: &Command) -> Vec<(String, String)> {
        let text = |s: &std::ffi::OsStr| s.to_str().unwrap().to_string();
        cmd.get_envs()
            .map(|(k, v)| (text(k), v.map(text).unwrap_or_default()))
            .collect()
    }

    #[test]
    fn bridge_mode_gives_the_agent_the_proxy_and_the_browser_flag() {
        let at = format!("http://127.0.0.1:{}", crate::egress::PORT);
        assert_eq!(PROXY_ENV[0], ("HTTP_PROXY", at.as_str()));
        let mut c = Command::new("true");
        let envs = envs(proxy(&mut c));
        for (k, v) in PROXY_ENV {
            assert!(envs.contains(&(k.to_string(), v.to_string())), "{k}");
        }
        for k in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
            assert!(envs.contains(&(k.to_string(), at.clone())), "{k}");
        }
        // rpi 0.18 refuses a proxy URL that is not HTTP.
        assert!(!envs
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("ALL_PROXY")));
        let flag = format!("--proxy-server={at}");
        assert!(envs.contains(&("AGENT_BROWSER_ARGS".into(), browser_args(None))));
        assert_eq!(browser_args(None), flag);
        assert_eq!(
            browser_args(Some("--no-sandbox,--x".into())),
            format!("--no-sandbox,--x,{flag}")
        );
        assert_eq!(browser_args(Some(String::new())), flag);
    }

    #[test]
    fn outside_bridge_mode_the_agent_gets_no_proxy() {
        let mut c = Command::new("true");
        let envs = envs(cmd(&mut c));
        let keys: Vec<&str> = envs.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["PATH"]);
    }

    /// B5: a program root starts or trusts (rpi, zephyr, the real agent-browser) is root's and
    /// writable by no one else, and no folder above it lets the agent replace it.
    #[test]
    fn replaceable_names_why_the_agent_could_swap_a_program() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let dir = crate::test_dir("agent-replaceable");
        let bin = dir.join("bin");
        std::fs::create_dir(&bin).unwrap();
        mode(&dir, 0o755);
        mode(&bin, 0o755);
        let rpi = bin.join("rpi");
        std::fs::write(&rpi, "#!/bin/sh\n").unwrap();
        mode(&rpi, 0o755);
        // SAFETY: plain syscalls with no arguments.
        let (me, group) = unsafe { (libc::geteuid(), libc::getegid()) };
        let (agent, agent_group) = (me + 1, group + 1);
        // Mine (root's in the image) and the folders above it no one else's: /tmp is sticky.
        assert_eq!(replaceable(&rpi, me, agent, agent_group), None);
        // Someone else's file: the tarball's uid 1001, say.
        let theirs = replaceable(&rpi, me + 7, agent, agent_group);
        assert!(theirs.is_some_and(|w| w.contains(&format!("uid {me}"))));
        mode(&rpi, 0o775);
        assert!(replaceable(&rpi, me, agent, agent_group).is_some());
        mode(&rpi, 0o757);
        assert!(replaceable(&rpi, me, agent, agent_group).is_some());
        mode(&rpi, 0o755);
        // A folder above it the agent may write, without the sticky bit.
        mode(&bin, 0o777);
        let why = replaceable(&rpi, me, agent, agent_group).unwrap();
        assert!(why.contains(&bin.display().to_string()), "{why}");
        mode(&bin, 0o1777);
        assert_eq!(replaceable(&rpi, me, agent, agent_group), None);
        mode(&bin, 0o775);
        assert_eq!(replaceable(&rpi, me, agent, agent_group), None);
        assert!(
            replaceable(&rpi, me, agent, group).is_some(),
            "the agent's group may write it"
        );
        mode(&bin, 0o755);
        // Reached through a link: the link's folder counts too.
        let open = dir.join("open");
        std::fs::create_dir(&open).unwrap();
        mode(&open, 0o777);
        let link = open.join("rpi");
        std::os::unix::fs::symlink(&rpi, &link).unwrap();
        assert!(replaceable(&link, me, agent, agent_group).is_some());
        assert!(replaceable(&dir.join("none"), me, agent, agent_group).is_some());
        std::fs::remove_dir_all(dir).unwrap();
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
