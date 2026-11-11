//! The agent's X desktop (DESIGN §4.11): Xvfb plus a window manager on `$ZEPHYR_DESKTOP`, run as
//! the agent. The image sets the variable; without it (tests, the host) there is no desktop.
//!
//! The server requires an MIT-MAGIC-COOKIE (`-auth`). With `--network host` the container shares
//! the host's abstract socket namespace, so Xvfb's `@/tmp/.X11-unix/X<n>` is reachable by any
//! process on the host, and abstract sockets have no file permissions: without the cookie, anyone
//! there could watch or drive the agent's signed-in browser. The server keeps holding that name,
//! so nobody else can claim it and have the agent's clients (which try it first) draw on theirs.

use crate::config::Config;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

static RUNNING: Mutex<Vec<Child>> = Mutex::new(Vec::new());

/// The desktop's programs, by full path: run as the agent, but never ones it left on its PATH,
/// which would be the daemon's children and spared by the stray check (DESIGN §4.6).
const XVFB: &str = "/usr/bin/Xvfb";
const OPENBOX: &str = "/usr/bin/openbox";
/// Root's config (`deploy/openbox/rc.xml`), never the agent's `~/.config/openbox/rc.xml`: it
/// binds no key, shows no menu and starts nothing.
const OPENBOX_RC: &str = "/opt/zephyr/openbox/rc.xml";
const OPENBOX_ARGS: [&str; 2] = ["--config-file", OPENBOX_RC];

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
    // Without root's openbox config there is no window manager, and the server alone is whole.
    if running.len() < wanted(std::path::Path::new(OPENBOX_RC).is_file()) {
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
        running.extend(start(XVFB, &[&display], &xvfb));
        // The window manager needs the server up first.
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Without root's config openbox would fall back to the agent's: then no window manager.
        match std::path::Path::new(OPENBOX_RC).is_file() {
            true => running.extend(start(OPENBOX, &OPENBOX_ARGS, &[])),
            false => eprintln!("zephyr: {OPENBOX_RC} is missing; the desktop runs without openbox"),
        }
    }
    Some(display)
}

/// How many programs a whole desktop runs: the server, and openbox when its config is there.
fn wanted(rc: bool) -> usize {
    1 + usize::from(rc)
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
    std::fs::rename(&tmp, path)?;
    crate::host::conf::sync_parent(path)
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

/// The file that says the owner holds the desktop (DESIGN §4.11), in `drafts::public_dir`:
/// root's, 0644, holding the start time. Never in the agent's home, where the agent could remove
/// it. The `agent-browser` and `xdotool` wrappers look for it before anything else.
pub const HELD_FILE: &str = "desktop-held";

/// The lock that keeps the agent's commands off the page while the owner has it (DESIGN §4.11),
/// beside [`HELD_FILE`]: root's, 0644. Each `agent-browser` and `xdotool` wrapper holds a shared
/// `flock` on it while the real binary runs; a take holds the exclusive one.
pub const LOCK_FILE: &str = "desktop.lock";
/// How long the processes a take killed have to go.
const KILL_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// state.db's `meta` row that holds a takeover's start while it lasts: the daemon's own record,
/// which a start trusts where it does not trust the flag file.
pub const TAKE_META: &str = "desktop_take";

pub const OTHER_DEVICE: &str = "another device has the desktop";
pub const FILLING: &str = "the vault is signing in; try again in a moment";
const NO_FLAG: &str = "the desktop could not be held; zephyr doctor says why";

/// The owner's hold on the desktop: since when (unix seconds), and the web session that took it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub since: i64,
    pub session: [u8; 32],
}

/// The one lock a takeover and the vault's fill share, so they never overlap. Made once by the
/// daemon and handed to the web hub, the gate server and the vault.
pub struct Control {
    cfg: Config,
    /// Fills running now. Every change to `held` is made holding this lock.
    fills: Mutex<usize>,
    held: watch::Sender<Option<Held>>,
    /// Every take and hand back, in order, for the router's notes: a `watch` would fold a take
    /// and a quick hand back into no change at all.
    events: mpsc::UnboundedSender<Event>,
    taken: Mutex<Option<mpsc::UnboundedReceiver<Event>>>,
    /// The exclusive lock on [`LOCK_FILE`], kept from `drain` to the hand back. Taken and
    /// dropped holding `fills`.
    lock: Mutex<Option<std::fs::File>>,
}

/// What happened to the desktop, for whoever tells main and the owner (`Control::events`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Took(Held),
    /// Handed back, by the owner or by itself: held from `since` to `until`. `restart`: the
    /// daemon started and found the flag of a takeover that a restart or a crash ended.
    Gave {
        since: i64,
        until: i64,
        restart: bool,
    },
}

/// A fill in progress; the desktop cannot be taken until it is dropped.
pub struct Fill(Arc<Control>);

impl Drop for Fill {
    fn drop(&mut self) {
        *self.0.fills.lock().unwrap_or_else(|e| e.into_inner()) -= 1;
    }
}

impl Control {
    /// Nobody holds the desktop at a start: a flag left behind is removed. When the daemon's
    /// own record ([`TAKE_META`]) shows a takeover that a crash, a restart or a re-exec ended,
    /// the first event says so, with its real start; a flag alone says nothing (a hand back
    /// that could not remove it), and what it holds is not read. The wrappers that waited on
    /// the old daemon's socket have already exited 1.
    pub fn new(cfg: &Config) -> Arc<Control> {
        let (events, taken) = mpsc::unbounded_channel();
        let control = Control {
            cfg: cfg.clone(),
            fills: Mutex::new(0),
            held: watch::channel(None).0,
            events,
            taken: Mutex::new(Some(taken)),
            lock: Mutex::new(None),
        };
        if let Err(e) = control.make_lock() {
            eprintln!("zephyr: {}: {e:#}", control.lock_path().display());
        }
        let left = control.record(|store| {
            let left = store.meta(TAKE_META)?;
            store.set_meta(TAKE_META, None)?;
            Ok(left)
        });
        if let Some(since) = left.flatten().and_then(|since| since.parse().ok()) {
            let _ = control.events.send(Event::Gave {
                since,
                until: crate::now(),
                restart: true,
            });
        }
        let _ = std::fs::remove_file(control.flag());
        Arc::new(control)
    }

    /// Reads or writes the takeover's record in state.db; a failure is logged, never fatal.
    fn record<T>(&self, f: impl FnOnce(&crate::store::Store) -> rusqlite::Result<T>) -> Option<T> {
        crate::store::Store::open(&self.cfg.db_path())
            .and_then(|store| f(&store))
            .map_err(|e| eprintln!("zephyr: the takeover's record in state.db: {e}"))
            .ok()
    }

    /// The takes and hand backs from now on, each once and in order. The router's, taken once.
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let taken = self.taken.lock().unwrap_or_else(|e| e.into_inner()).take();
        taken.unwrap_or_else(|| mpsc::unbounded_channel().1)
    }

    fn flag(&self) -> PathBuf {
        crate::drafts::public_dir(&self.cfg).join(HELD_FILE)
    }

    fn lock_path(&self) -> PathBuf {
        crate::drafts::public_dir(&self.cfg).join(LOCK_FILE)
    }

    /// The lock file, for the agent to open for reading and lock, never to change or replace.
    fn make_lock(&self) -> anyhow::Result<()> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        crate::drafts::ensure_public_dir(&self.cfg)?;
        let mut open = std::fs::OpenOptions::new();
        let file = open
            .create(true)
            .append(true)
            .mode(0o644)
            .open(self.lock_path())?;
        Ok(file.set_permissions(PermissionsExt::from_mode(0o644))?)
    }

    /// After `take` and before the owner gets the desktop: waits for the agent's browser and
    /// desktop commands that were already running to end (each holds a shared lock on
    /// [`LOCK_FILE`]), `Config::desktop_drain` at most, then kills what still holds it. The
    /// exclusive lock is then kept until the hand back, so no wrapper takes a place meanwhile.
    pub async fn drain(self: &Arc<Self>) {
        let this = self.clone();
        let _ = tokio::task::spawn_blocking(move || this.drain_blocking()).await;
    }

    fn drain_blocking(&self) {
        use std::os::fd::AsRawFd;
        let Some(held) = self.held() else {
            return;
        };
        if self
            .lock
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            return; // this takeover has it already
        }
        let path = self.lock_path();
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(e) => return eprintln!("zephyr: {}: {e}", path.display()),
        };
        let alone = |wait: std::time::Duration| {
            let until = std::time::Instant::now() + wait;
            loop {
                // SAFETY: a plain syscall on a descriptor this function owns.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    return true;
                }
                if std::time::Instant::now() >= until {
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let mut got = alone(self.cfg.desktop_drain);
        if !got {
            let killed = kill_holders(&path);
            eprintln!("zephyr: ended {killed} of the agent's commands still on the desktop");
            got = alone(KILL_WAIT);
        }
        if !got {
            return eprintln!("zephyr: {} is still locked; handing over", path.display());
        }
        // Only for the takeover it was drained for: a hand back meanwhile drops it here.
        let _fills = self.fills.lock().unwrap_or_else(|e| e.into_inner());
        if self.held().as_ref() == Some(&held) {
            *self.lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
        }
    }

    /// The owner takes the desktop. The session that holds it may take it again (a new ticket
    /// after its socket dropped).
    pub fn take(&self, session: [u8; 32], now: i64) -> Result<(), &'static str> {
        use std::os::unix::fs::PermissionsExt;
        self.take_writing(session, now, |flag| {
            std::fs::write(flag, format!("{now}\n"))?;
            std::fs::set_permissions(flag, PermissionsExt::from_mode(0o644))
        })
    }

    /// `take`, the flag written by `write`. The hold is published first: a wrapper that sees the
    /// flag asks the gate, which must already see the hold, or it would answer "go on". The
    /// flag is there whenever the hold is, never the other way round.
    fn take_writing(
        &self,
        session: [u8; 32],
        now: i64,
        write: impl FnOnce(&std::path::Path) -> std::io::Result<()>,
    ) -> Result<(), &'static str> {
        let fills = self.fills.lock().unwrap_or_else(|e| e.into_inner());
        match self.held() {
            Some(held) if held.session == session => return Ok(()),
            Some(_) => return Err(OTHER_DEVICE),
            None if *fills > 0 => return Err(FILLING),
            None => {}
        }
        let held = Held {
            since: now,
            session,
        };
        self.held.send_replace(Some(held.clone()));
        // Without the flag the wrappers would not wait: then there is no takeover.
        let flag = self.flag();
        let written = crate::drafts::ensure_public_dir(&self.cfg).and_then(|_| Ok(write(&flag)?));
        if let Err(e) = written {
            eprintln!("zephyr: writing {}: {e:#}", flag.display());
            let _ = std::fs::remove_file(&flag);
            self.held.send_replace(None);
            return Err(NO_FLAG);
        }
        self.record(|store| store.set_meta(TAKE_META, Some(&now.to_string())));
        let _ = self.events.send(Event::Took(held));
        Ok(())
    }

    /// The desktop goes back to the agent: `(since, until)` when it was held.
    pub fn give(&self, now: i64) -> Option<(i64, i64)> {
        let _fills = self.fills.lock().unwrap_or_else(|e| e.into_inner());
        // The hold is the truth and ends here, whatever becomes of the flag: one that stays
        // only makes the wrappers ask the gate, which lets them run at once.
        let held = self.held.send_replace(None)?;
        // The agent's commands may take their places again.
        drop(self.lock.lock().unwrap_or_else(|e| e.into_inner()).take());
        self.record(|store| store.set_meta(TAKE_META, None));
        let removed = (0..3).find_map(|_| match std::fs::remove_file(self.flag()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Some(e),
            _ => None,
        });
        if let Some(e) = removed {
            eprintln!("zephyr: removing {}: {e}", self.flag().display());
        }
        let _ = self.events.send(Event::Gave {
            since: held.since,
            until: now,
            restart: false,
        });
        Some((held.since, now))
    }

    pub fn held(&self) -> Option<Held> {
        self.held.borrow().clone()
    }

    /// The vault is about to type into the page; `None` while the owner holds the desktop.
    pub fn fill(self: &Arc<Self>) -> Option<Fill> {
        let mut fills = self.fills.lock().unwrap_or_else(|e| e.into_inner());
        if self.held().is_some() {
            return None;
        }
        *fills += 1;
        Some(Fill(self.clone()))
    }

    pub fn changes(&self) -> watch::Receiver<Option<Held>> {
        self.held.subscribe()
    }
}

/// SIGKILLs every process of the agent's (never this one) that holds a lock on `lock` through
/// an open descriptor of its own, found in `/proc`: the wrappers that outlasted a take's wait,
/// and anything of the agent's that took the lock itself to hold a take up. Returns how many.
fn kill_holders(lock: &std::path::Path) -> usize {
    use std::os::unix::fs::MetadataExt;
    let lock = std::fs::canonicalize(lock).unwrap_or_else(|_| lock.to_path_buf());
    // SAFETY: geteuid takes no arguments and cannot fail.
    let uid = crate::agent::ids().map_or_else(|| unsafe { libc::geteuid() }, |(uid, _)| uid);
    let me = std::process::id() as i32;
    let holds = |pid: i32| {
        let fds = std::fs::read_dir(format!("/proc/{pid}/fd"))
            .into_iter()
            .flatten();
        fds.flatten().any(|fd| {
            let info = format!("/proc/{pid}/fdinfo/{}", fd.file_name().to_string_lossy());
            std::fs::read_link(fd.path()).is_ok_and(|to| to == lock)
                && std::fs::read_to_string(info).is_ok_and(|info| info.contains("\nlock:"))
        })
    };
    let mut killed = 0;
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        let theirs = entry.metadata().is_ok_and(|m| m.uid() == uid);
        if pid != me && theirs && holds(pid) {
            // SAFETY: a plain syscall; a pid that ended meanwhile is an error, ignored.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            killed += 1;
        }
    }
    killed
}

/// What is wrong with the takeover as the image lays it out (doctor's `desktop` line, DESIGN
/// §4.3.3); empty when it is ready. `up` is whether the daemon runs: it removes a flag a crash
/// left when it starts, so one found beside a running daemon is a takeover in progress.
pub fn check(cfg: &Config, up: bool) -> Vec<String> {
    let flag = crate::drafts::public_dir(cfg).join(HELD_FILE);
    let link = std::path::Path::new(crate::browser_gate::GATE_BIN).join("xdotool");
    let me = std::fs::canonicalize("/proc/self/exe").unwrap_or_default();
    problems(&cfg.x11vnc, &flag, up, &link, &me, 0)
}

fn problems(
    x11vnc: &std::path::Path,
    flag: &std::path::Path,
    up: bool,
    link: &std::path::Path,
    me: &std::path::Path,
    owner: u32,
) -> Vec<String> {
    use std::os::unix::fs::MetadataExt;
    let mut found = Vec::new();
    if !x11vnc.is_file() {
        found.push(format!("{} is missing", x11vnc.display()));
    }
    if !up && flag.symlink_metadata().is_ok() {
        let flag = flag.display();
        found.push(format!(
            "{flag} is left from a crash (the next start removes it)"
        ));
    }
    // The link itself is root's, in root's folder (doctor checks the folder), and names zephyr.
    let roots = link.symlink_metadata().is_ok_and(|m| m.uid() == owner);
    if !roots || std::fs::canonicalize(link).ok().as_deref() != Some(me) {
        let link = link.display();
        found.push(format!(
            "{link} is not root's link to zephyr, so xdotool does not wait"
        ));
    }
    found
}

/// `at` as `HH:MM` in the owner's time zone.
pub fn clock(cfg: &Config, at: i64) -> String {
    let tz = crate::scheduler::zone(cfg).unwrap_or(jiff::tz::TimeZone::UTC);
    crate::settings::clock(at, &tz)
}

/// What a held command reads when the owner still has the desktop after the gate's wait.
pub fn still(cfg: &Config, since: i64) -> String {
    format!(
        "zephyr: the owner has the desktop (since {}); this command did not run. Do other work, \
         or try again later.",
        clock(cfg, since)
    )
}

/// What a held command reads at the hand back. It never runs: a click planned on the page as it
/// was is not made on the page as the owner left it.
pub fn used(cfg: &Config, since: i64, until: i64) -> String {
    format!(
        "zephyr: the owner used the desktop from {} to {}; this command did not run. The page \
         may have changed: take a fresh snapshot before acting.",
        clock(cfg, since),
        clock(cfg, until)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(name: &str) -> (Config, Arc<Control>) {
        let dir = crate::test_dir(name);
        let cfg: Config =
            toml::from_str(&format!("rpi = '/r'\nhome = {dir:?}\ntimezone = 'UTC'\n")).unwrap();
        let control = Control::new(&cfg);
        (cfg, control)
    }

    fn flag(cfg: &Config) -> PathBuf {
        crate::drafts::public_dir(cfg).join(HELD_FILE)
    }

    #[test]
    fn take_writes_the_flag_and_give_removes_it() {
        use std::os::unix::fs::PermissionsExt;
        let (cfg, control) = control("desktop-take");
        let mut changes = control.changes();
        assert!(control.held().is_none() && !flag(&cfg).exists());
        control.take([1; 32], 100).unwrap();
        // In root's public folder, never the agent's home, which the agent could clear.
        assert!(!flag(&cfg).starts_with(&cfg.home), "{:?}", flag(&cfg));
        assert_eq!(std::fs::read_to_string(flag(&cfg)).unwrap(), "100\n");
        let mode = std::fs::metadata(flag(&cfg)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
        let held = Held {
            since: 100,
            session: [1; 32],
        };
        assert_eq!(control.held(), Some(held.clone()));
        assert_eq!(*changes.borrow_and_update(), Some(held));
        assert_eq!(control.give(160), Some((100, 160)));
        assert!(control.held().is_none() && !flag(&cfg).exists());
        assert!(changes.has_changed().unwrap());
        assert_eq!(control.give(170), None, "nothing was held");
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    #[test]
    fn a_second_take_and_a_take_during_a_fill_are_refused() {
        let (cfg, control) = control("desktop-second");
        let fill = control.fill().expect("nobody holds the desktop");
        assert_eq!(control.take([1; 32], 100), Err(FILLING));
        assert!(!flag(&cfg).exists());
        drop(fill);
        control.take([1; 32], 100).unwrap();
        assert_eq!(control.take([2; 32], 101), Err(OTHER_DEVICE));
        // Its own session again: still held since the first take.
        control.take([1; 32], 105).unwrap();
        assert_eq!(control.held().unwrap().since, 100);
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    #[test]
    fn a_fill_during_a_takeover_is_refused() {
        let (cfg, control) = control("desktop-fill");
        control.take([1; 32], 100).unwrap();
        assert!(control.fill().is_none());
        control.give(101);
        assert!(control.fill().is_some());
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    /// A wrapper that sees the flag asks the gate at once: the gate must see the hold by then.
    #[test]
    fn the_hold_is_published_before_the_flag_is_written() {
        let (cfg, control) = control("desktop-order");
        let mut changes = control.changes();
        let seen = control.take_writing([1; 32], 100, |flag| {
            assert_eq!(control.held().map(|h| h.since), Some(100));
            std::fs::write(flag, "100\n")
        });
        assert_eq!(seen, Ok(()));
        // A flag that cannot be written is no takeover, and nobody is told of one.
        control.give(101);
        let mut events = control.events();
        while events.try_recv().is_ok() {}
        let failed = control.take_writing([1; 32], 102, |_| Err(std::io::Error::other("full")));
        assert_eq!(failed, Err(NO_FLAG));
        assert!(control.held().is_none() && !flag(&cfg).exists());
        assert!(events.try_recv().is_err());
        assert!(changes.borrow_and_update().is_none());
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    /// The router must hear of both, however fast the second follows the first.
    #[test]
    fn a_take_and_a_give_back_to_back_are_two_events_in_order() {
        let (cfg, control) = control("desktop-events");
        let mut events = control.events();
        control.take([1; 32], 100).unwrap();
        control.give(160);
        control.take([2; 32], 170).unwrap();
        // Its own session again is no new takeover.
        control.take([2; 32], 175).unwrap();
        control.give(180);
        assert_eq!(control.give(181), None);
        let held = |since, session| Held { since, session };
        let got: Vec<Event> = std::iter::from_fn(|| events.try_recv().ok()).collect();
        assert_eq!(
            got,
            [
                Event::Took(held(100, [1; 32])),
                Event::Gave {
                    since: 100,
                    until: 160,
                    restart: false
                },
                Event::Took(held(170, [2; 32])),
                Event::Gave {
                    since: 170,
                    until: 180,
                    restart: false
                },
            ]
        );
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    #[test]
    fn a_start_removes_a_stale_flag() {
        let (cfg, control) = control("desktop-stale");
        // A flag with no take on record (a hand back that could not remove it): removed, and
        // nobody is told of a takeover that was not one. What the file says is not read.
        crate::drafts::ensure_public_dir(&cfg).unwrap();
        std::fs::write(flag(&cfg), "100\n").unwrap();
        let clean = Control::new(&cfg);
        assert!(!flag(&cfg).exists());
        assert!(clean.events().try_recv().is_err());
        control.take([1; 32], 100).unwrap();
        assert!(flag(&cfg).exists());
        std::fs::write(flag(&cfg), "7\n").unwrap();
        // The daemon crashed and starts again.
        let again = Control::new(&cfg);
        assert!(again.held().is_none() && !flag(&cfg).exists());
        // That takeover is over, and whoever listens is told since when it ran, from the
        // daemon's own record.
        let told = again.events().try_recv();
        assert!(
            matches!(
                told,
                Ok(Event::Gave {
                    since: 100,
                    restart: true,
                    ..
                })
            ),
            "{told:?}"
        );
        // A clean start says nothing.
        assert!(Control::new(&cfg).events().try_recv().is_err());
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    #[test]
    fn doctor_names_what_the_takeover_lacks() {
        use std::os::unix::fs::MetadataExt;
        let dir = crate::test_dir("desktop-doctor");
        let (x11vnc, flag, link, me) = (
            dir.join("x11vnc"),
            dir.join(HELD_FILE),
            dir.join("xdotool"),
            dir.join("zephyr"),
        );
        let mine = std::fs::metadata(&dir).unwrap().uid();
        let found = |up: bool| problems(&x11vnc, &flag, up, &link, &me, mine);
        assert_eq!(found(true).len(), 2, "{:?}", found(true));
        std::fs::write(&x11vnc, "").unwrap();
        std::fs::write(&me, "").unwrap();
        std::fs::write(dir.join("other"), "").unwrap();
        std::os::unix::fs::symlink(dir.join("other"), &link).unwrap();
        assert_eq!(
            found(true),
            [format!(
                "{} is not root's link to zephyr, so xdotool does not wait",
                link.display()
            )]
        );
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&me, &link).unwrap();
        assert_eq!(found(true), [""; 0]);
        // Another owner's link, even to zephyr, is not root's.
        assert_eq!(
            problems(&x11vnc, &flag, true, &link, &me, mine + 1).len(),
            1
        );
        // A flag beside a running daemon is a takeover; beside none, a crash left it.
        std::fs::write(&flag, "100\n").unwrap();
        assert_eq!(found(true), [""; 0]);
        assert_eq!(
            found(false),
            [format!(
                "{} is left from a crash (the next start removes it)",
                flag.display()
            )]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The hold is the truth, the flag only the hint to ask: a hand back whose flag stays
    /// (here it became a folder) still ends the hold, and leaves no take on record.
    #[test]
    fn a_flag_that_cannot_be_removed_still_ends_the_hold() {
        let (cfg, control) = control("desktop-stuck");
        control.take([1; 32], 100).unwrap();
        std::fs::remove_file(flag(&cfg)).unwrap();
        std::fs::create_dir(flag(&cfg)).unwrap();
        assert_eq!(control.give(160), Some((100, 160)));
        assert!(control.held().is_none());
        assert!(control.fill().is_some(), "the vault still waits");
        assert!(Control::new(&cfg).events().try_recv().is_err());
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    /// A shared lock on the control's lock file, as a wrapper holds one while its command runs.
    fn place(cfg: &Config) -> std::fs::File {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(crate::drafts::public_dir(cfg).join(LOCK_FILE)).unwrap();
        // SAFETY: a plain syscall on a descriptor this function owns.
        let got = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) };
        assert_eq!(got, 0, "no place to be had");
        file
    }

    fn placed(cfg: &Config) -> bool {
        use std::os::fd::AsRawFd;
        let file = std::fs::File::open(crate::drafts::public_dir(cfg).join(LOCK_FILE)).unwrap();
        // SAFETY: as in `place`.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_SH | libc::LOCK_NB) == 0 }
    }

    /// The owner gets the desktop only once the agent's commands already on it have ended,
    /// and none takes a place while they have it.
    #[test]
    fn a_take_waits_for_the_commands_already_running() {
        use std::os::unix::fs::PermissionsExt;
        let (cfg, control) = control("desktop-drain");
        let lock = crate::drafts::public_dir(&cfg).join(LOCK_FILE);
        let mode = std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "the agent may read and lock it, not change it");
        let running = place(&cfg);
        control.take([1; 32], 100).unwrap();
        let (done, drained) = std::sync::mpsc::channel();
        let draining = std::thread::spawn({
            let control = control.clone();
            move || {
                control.drain_blocking();
                done.send(()).unwrap();
            }
        });
        // Still waiting while the command runs (15 s at most, far off).
        assert!(drained
            .recv_timeout(std::time::Duration::from_millis(300))
            .is_err());
        drop(running);
        drained
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("the take goes on once the command ended");
        draining.join().unwrap();
        assert!(
            !placed(&cfg),
            "a command took a place under the owner's hands"
        );
        // The holder's own take again changes nothing.
        control.drain_blocking();
        control.give(160);
        assert!(placed(&cfg), "the hand back left the lock taken");
        // A hand back before the wait ends leaves no lock behind.
        let running = place(&cfg);
        control.take([1; 32], 200).unwrap();
        let draining = std::thread::spawn({
            let control = control.clone();
            move || control.drain_blocking()
        });
        control.give(201);
        drop(running);
        draining.join().unwrap();
        assert!(placed(&cfg));
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    /// A command that outlasts the wait (or anything of the agent's that took the lock to hold
    /// a take up) is killed before the owner gets the desktop.
    #[test]
    fn a_command_that_outlasts_the_wait_is_killed() {
        use std::io::BufRead;
        use std::os::unix::process::ExitStatusExt;
        let (mut cfg, _) = control("desktop-kill");
        cfg.desktop_drain = std::time::Duration::from_millis(200);
        let control = Control::new(&cfg);
        let lock = crate::drafts::public_dir(&cfg).join(LOCK_FILE);
        let script = "import fcntl, sys, time\nf = open(sys.argv[1])\n\
                      fcntl.flock(f, fcntl.LOCK_SH)\nprint('placed', flush=True)\ntime.sleep(600)\n";
        let mut stuck = Command::new("python3")
            .args(["-I", "-c", script])
            .arg(&lock)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stuck.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "placed");
        control.take([1; 32], 100).unwrap();
        control.drain_blocking();
        assert_eq!(stuck.wait().unwrap().signal(), Some(libc::SIGKILL));
        assert!(!placed(&cfg), "the owner has no lock on the desktop");
        control.give(101);
        let _ = std::fs::remove_dir_all(crate::drafts::public_dir(&cfg));
    }

    #[test]
    fn a_held_command_is_told_since_when_and_what_to_do() {
        let (cfg, _) = control("desktop-text");
        assert_eq!(
            still(&cfg, 14 * 3600 + 4 * 60),
            "zephyr: the owner has the desktop (since 14:04); this command did not run. Do \
             other work, or try again later."
        );
        assert_eq!(
            used(&cfg, 14 * 3600 + 4 * 60, 14 * 3600 + 8 * 60),
            "zephyr: the owner used the desktop from 14:04 to 14:08; this command did not run. \
             The page may have changed: take a fresh snapshot before acting."
        );
    }

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

    #[test]
    fn the_desktop_runs_by_full_path() {
        // Not looked up on the agent's PATH, where a program of its own of that name would be
        // the daemon's child, spared by the stray check.
        assert_eq!(XVFB, "/usr/bin/Xvfb");
        assert_eq!(OPENBOX, "/usr/bin/openbox");
    }

    #[test]
    fn openbox_reads_roots_config_that_runs_nothing() {
        // Never `~/.config/openbox/rc.xml`, the agent's to write: its keys, menu and autostart
        // could start a program as the desktop's child, which the stray check spares.
        assert_eq!(
            OPENBOX_ARGS,
            ["--config-file", "/opt/zephyr/openbox/rc.xml"]
        );
        let rc = include_str!("../../../deploy/openbox/rc.xml");
        let menu = include_str!("../../../deploy/openbox/menu.xml");
        for text in [rc, menu] {
            let lower = text.to_ascii_lowercase();
            for word in ["execute", "autostart", "pipemenu", "<command", "showmenu"] {
                assert!(!lower.contains(word), "{word} in {text}");
            }
        }
        assert!(
            rc.contains("<file>/opt/zephyr/openbox/menu.xml</file>"),
            "{rc}"
        );
    }

    #[test]
    fn a_desktop_without_openbox_is_whole_with_its_server() {
        // Else every `ensure` would kill the server and start another.
        assert_eq!((wanted(false), wanted(true)), (1, 2));
    }
}
