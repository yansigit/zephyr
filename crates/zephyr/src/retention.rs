//! Deletes what background work leaves behind once it is 30 days old (DESIGN §4.1): job and task
//! sessions, ended processes' logs and exit files, and ended tasks' overflow reports. Never a
//! `main` session, which `/switch` goes back to, and never a file of work still open. Run hourly
//! from the scheduler's tick, which also makes the day's backup of state.db (`backup`).

use crate::config::Config;
use crate::store::Store;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How old a leftover must be before it goes.
pub const AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// How old a settled row of state.db must be before it goes.
const DB_AGE: Duration = Duration::from_secs(90 * 24 * 3600);

/// One sweep, as the agent (whose files these are). Returns how many files went.
pub fn sweep(cfg: &Config, store: &Store, age: Duration) -> rusqlite::Result<usize> {
    let open = store.tasks(0)?; // queued or running, a resume included
    let tasks: HashSet<i64> = open.iter().map(|t| t.id).collect();
    let sessions: HashSet<String> = open.iter().filter_map(|t| t.session.clone()).collect();
    let procs: HashSet<i64> = store.running_procs()?.iter().map(|r| r.0.id).collect();
    // Settled messages, replies and audit lines in state.db (`Store::prune`), as root.
    match store.prune(crate::now() - DB_AGE.as_secs() as i64) {
        Ok(0) => {}
        Ok(n) => eprintln!("zephyr: pruned {n} state.db rows older than 90 days"),
        Err(e) => eprintln!("zephyr: pruning state.db failed: {e}"),
    }
    // As root, before the agent's identity below: the backups are root's.
    match backup(cfg, store) {
        Ok(false) => {}
        Ok(true) => eprintln!("zephyr: backed up state.db and the config files"),
        Err(e) => eprintln!("zephyr: backing up state.db failed: {e:#}"),
    }
    let _fs = crate::agent::fs();
    let dir = cfg.session_dir();
    let mut gone = 0;
    // rpi files sessions one level down, per cwd (`sessions/--zephyr--/<ts>_<id>.jsonl`).
    for d in std::iter::once(dir.clone()).chain(subdirs(&dir)) {
        gone += remove_old(&d, age, |name| {
            session_of(name).is_some_and(|s| {
                (s.starts_with("job-") || s.starts_with("task-")) && !sessions.contains(s)
            })
        });
    }
    gone += remove_old(&cfg.home.join(".procs"), age, |name| {
        proc_of(name).is_some_and(|id| !procs.contains(&id))
    });
    gone += remove_old(&cfg.home.join("tasks"), age, |name| {
        let id = name.strip_suffix(".md").and_then(|n| n.parse::<i64>().ok());
        id.is_some_and(|id| !tasks.contains(&id))
    });
    gone += remove_old(&cfg.home.join("inbox"), age, inbox_file);
    // The projects' checkpoints: the newest 50 of each (DESIGN §4.14).
    if let Some(cp) = crate::checkpoint::Checkpoints::of(cfg) {
        match cp.prune(crate::checkpoint::KEEP) {
            Ok(0) => {}
            Ok(n) => eprintln!("zephyr: pruned {n} old checkpoints of the projects"),
            Err(e) => eprintln!("zephyr: pruning checkpoints failed: {e:#}"),
        }
    }
    Ok(gone)
}

/// How many backups of each file `backup` keeps.
const BACKUPS_KEPT: usize = 7;

/// The daemon's files `backup` copies beside state.db, when present.
const BACKED_UP: [&str; 4] = ["zephyr.toml", "vault.toml", "host.toml", "secrets.env"];

fn backups_dir(cfg: &Config) -> PathBuf {
    cfg.state_dir().join("backups")
}

/// Once a UTC day: a consistent copy of state.db to `<state>/backups/state-YYYYMMDD.db` and copies
/// of the `BACKED_UP` files as `<name>.YYYYMMDD`, all root's (dir 0700, files 0600), keeping the
/// newest `BACKUPS_KEPT` of each. Returns false when today's backup was already there.
pub fn backup(cfg: &Config, store: &Store) -> anyhow::Result<bool> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    let dir = backups_dir(cfg);
    let day = store.today()?.replace('-', "");
    let db = dir.join(format!("state-{day}.db"));
    if db.exists() {
        return Ok(false);
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let private =
        |path: &Path| std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    for name in BACKED_UP {
        let to = dir.join(format!("{name}.{day}"));
        match std::fs::copy(cfg.state_dir().join(name), &to) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            r => r?,
        };
        private(&to)?;
    }
    // Last, under a temp name: its presence marks the day's backup as whole.
    let tmp = dir.join(format!(".state-{day}.db.tmp"));
    let _ = std::fs::remove_file(&tmp);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    store.copy_into(&tmp)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, &db)?;
    std::fs::File::open(&dir)?.sync_all()?;
    prune_backups(&dir);
    Ok(true)
}

/// The kind of a backup file: `state.db`, or the `BACKED_UP` name it copies.
fn backup_kind(name: &str) -> Option<&str> {
    let dated = |d: &str| d.len() == 8 && d.bytes().all(|b| b.is_ascii_digit());
    if let Some(d) = name
        .strip_prefix("state-")
        .and_then(|n| n.strip_suffix(".db"))
    {
        return dated(d).then_some("state.db");
    }
    let (base, d) = name.rsplit_once('.')?;
    (dated(d) && BACKED_UP.contains(&base)).then_some(base)
}

/// Removes all but the newest `BACKUPS_KEPT` of each kind; the date in the name orders them.
fn prune_backups(dir: &Path) {
    let mut kinds: std::collections::HashMap<String, Vec<String>> = Default::default();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(name) = e.file_name().into_string() else {
            continue;
        };
        if let Some(kind) = backup_kind(&name) {
            kinds.entry(kind.to_string()).or_default().push(name);
        }
    }
    for names in kinds.values_mut() {
        names.sort_unstable_by(|a, b| b.cmp(a));
        for old in names.iter().skip(BACKUPS_KEPT) {
            let _ = std::fs::remove_file(dir.join(old));
        }
    }
}

/// `zephyr doctor`'s warning: the newest backup of state.db is over 2 days old, or there is none
/// though state.db was made over a day ago. `now` is the clock, for tests.
pub fn backup_warning(cfg: &Config, now: std::time::SystemTime) -> Option<String> {
    let age =
        |t: std::io::Result<std::time::SystemTime>| t.ok().and_then(|t| now.duration_since(t).ok());
    let newest = std::fs::read_dir(backups_dir(cfg))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_str().and_then(backup_kind) == Some("state.db"))
        .filter_map(|e| age(e.metadata().and_then(|m| m.modified())))
        .min();
    const DAY: u64 = 24 * 3600;
    match newest {
        Some(a) if a.as_secs() > 2 * DAY => Some(format!(
            "the newest backup of state.db is {} days old ({})",
            a.as_secs() / DAY,
            backups_dir(cfg).display()
        )),
        Some(_) => None,
        // When state.db was made: roughly since when the daemon has run.
        None => age(std::fs::metadata(cfg.db_path()).and_then(|m| m.created()))
            .filter(|a| a.as_secs() > DAY)
            .map(|_| {
                format!(
                    "no backup of state.db in {} yet",
                    backups_dir(cfg).display()
                )
            }),
    }
}

/// The session id in rpi's `<timestamp>_<session id>.jsonl` (or its `.lock`).
fn session_of(name: &str) -> Option<&str> {
    let name = name.strip_suffix(".lock").unwrap_or(name);
    Some(name.strip_suffix(".jsonl")?.split_once('_')?.1)
}

/// The process id in `<id>.log`, `<id>.exit` or `<id>.exit.tmp` (`procs.rs`).
fn proc_of(name: &str) -> Option<i64> {
    let (id, ext) = name.split_once('.')?;
    matches!(ext, "log" | "exit" | "exit.tmp")
        .then(|| id.parse().ok())
        .flatten()
}

/// A file the owner sent (`inbox.rs`): `<channel>-<id>-<n>.<ext>`, or a `.<stem>.part` left by a
/// crash mid-write.
fn inbox_file(name: &str) -> bool {
    let stem = match name.strip_prefix('.') {
        Some(n) => n.strip_suffix(".part"),
        None => name.rsplit_once('.').map(|(stem, _)| stem),
    };
    let rest = stem.and_then(|s| s.strip_prefix("discord-").or(s.strip_prefix("imessage-")));
    rest.and_then(|r| r.rsplit_once('-'))
        .is_some_and(|(id, n)| !id.is_empty() && n.parse::<u8>().is_ok())
}

fn subdirs(dir: &Path) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(dir).into_iter().flatten().flatten();
    entries
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir())) // never follows a symlink
        .map(|e| e.path())
        .collect()
}

/// Removes the regular files in `dir` (not below) older than `age` whose name `pick`s.
fn remove_old(dir: &Path, age: Duration, pick: impl Fn(&str) -> bool) -> usize {
    let mut gone = 0;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let Ok(meta) = e.path().symlink_metadata() else {
            continue;
        };
        let old = meta
            .modified()
            .is_ok_and(|t| t.elapsed().is_ok_and(|d| d > age));
        let picked = e.file_name().to_str().is_some_and(&pick);
        if meta.is_file() && old && picked && std::fs::remove_file(e.path()).is_ok() {
            gone += 1;
        }
    }
    gone
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_old_leftovers_of_ended_work_go() {
        let home = crate::test_dir("retention");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        // Task 1 is still queued in session task-1; process 1 is still running.
        assert_eq!(
            store.add_task("g", None, None, "chat", "o").unwrap(),
            Some(1)
        );
        store.set_task_session(1, "task-1").unwrap();
        assert_eq!(store.add_proc("ls", "chat", "o").unwrap().unwrap().id, 1);

        let s = home.join("sessions/--zephyr--");
        for d in [&s, &home.join(".procs"), &home.join("tasks")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let month_ago = std::time::SystemTime::now() - AGE - Duration::from_secs(86400);
        let file = |path: PathBuf, old: bool| {
            std::fs::write(&path, "x").unwrap();
            if old {
                std::fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(month_ago)
                    .unwrap();
            }
            path
        };
        let gone = [
            file(s.join("2026-01-01T00-00-00-000Z_job-1-2.jsonl"), true),
            file(s.join("2026-01-01T00-00-00-000Z_job-1-2.jsonl.lock"), true),
            file(s.join("2026-01-01T00-00-00-000Z_task-9.jsonl"), true),
            file(home.join(".procs/2.log"), true),
            file(home.join(".procs/2.exit"), true),
            file(home.join("tasks/5.md"), true),
        ];
        let kept = [
            file(s.join("2026-01-01T00-00-00-000Z_main.jsonl"), true),
            file(s.join("2026-01-01T00-00-00-000Z_main-17.jsonl"), true),
            file(s.join("2026-01-01T00-00-00-000Z_task-1.jsonl"), true), // its task is open
            file(s.join("2026-10-01T00-00-00-000Z_job-3-1.jsonl"), false), // fresh
            file(home.join(".procs/1.log"), true),                       // still running
            file(home.join(".procs/notes.txt"), true),                   // not ours
            file(home.join("tasks/1.md"), true),                         // its task is open
            file(home.join("tasks/6.md"), false),                        // fresh
        ];
        assert_eq!(sweep(&cfg, &store, AGE).unwrap(), gone.len());
        for p in &gone {
            assert!(!p.exists(), "{} should be gone", p.display());
        }
        for p in &kept {
            assert!(p.exists(), "{} should stay", p.display());
        }
    }

    #[test]
    fn the_sweep_prunes_state_db_past_ninety_days() {
        let home = crate::test_dir("retention-db");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        store.audit("main", "bash", "ls", "started").unwrap();
        store.audit("main", "bash", "pwd", "started").unwrap();
        let old = crate::now() - DB_AGE.as_secs() as i64 - 60;
        rusqlite::Connection::open(cfg.db_path())
            .unwrap()
            .execute("UPDATE audit SET ts = ?1 WHERE args = 'ls'", [old])
            .unwrap();
        sweep(&cfg, &store, AGE).unwrap();
        let rows = store.audit_rows().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].2, "pwd", "a fresh line stays");
    }

    #[test]
    fn a_backup_is_made_once_a_day_and_the_newest_seven_kept() {
        use std::os::unix::fs::PermissionsExt;
        let home = crate::test_dir("retention-backup");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        store.audit("main", "bash", "ls", "started").unwrap();
        std::fs::write(home.join("zephyr.toml"), "rpi = '/r'\n").unwrap();
        std::fs::write(home.join("secrets.env"), "K=v\n").unwrap();
        let dir = home.join("backups");
        // Eight older days of each kind, the oldest of which goes.
        std::fs::create_dir_all(&dir).unwrap();
        for d in 20250101..20250109 {
            for name in [format!("state-{d}.db"), format!("zephyr.toml.{d}")] {
                std::fs::write(dir.join(name), "old").unwrap();
            }
        }
        std::fs::write(dir.join("notes.txt"), "mine").unwrap();
        assert!(backup(&cfg, &store).unwrap());
        let day = store.today().unwrap().replace('-', "");
        let db = dir.join(format!("state-{day}.db"));
        let copy = Store::open(&db).unwrap();
        assert_eq!(copy.audit_rows().unwrap().len(), 1, "a whole copy");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&db), 0o600);
        assert_eq!(mode(&dir.join(format!("secrets.env.{day}"))), 0o600);
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("secrets.env.{day}"))).unwrap(),
            "K=v\n"
        );
        assert!(
            !dir.join(format!("vault.toml.{day}")).exists(),
            "none to copy"
        );
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names
                .iter()
                .filter(|n| backup_kind(n) == Some("state.db"))
                .count(),
            7
        );
        assert_eq!(
            names
                .iter()
                .filter(|n| n.starts_with("zephyr.toml."))
                .count(),
            7
        );
        assert!(!names.contains(&"state-20250101.db".into()), "{names:?}");
        assert!(!names.contains(&"state-20250102.db".into()), "{names:?}");
        assert!(names.contains(&"state-20250103.db".into()), "{names:?}");
        assert!(names.contains(&"notes.txt".into()), "not a backup: kept");
        // Today's is there: a second sweep (a restart) makes no other.
        let before = std::fs::metadata(&db).unwrap().modified().unwrap();
        assert!(!backup(&cfg, &store).unwrap());
        assert_eq!(std::fs::metadata(&db).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn doctor_warns_of_a_stale_or_missing_backup() {
        let home = crate::test_dir("retention-backup-age");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        let now = std::time::SystemTime::now();
        let day = Duration::from_secs(24 * 3600);
        assert_eq!(
            backup_warning(&cfg, now),
            None,
            "a new state.db needs none yet"
        );
        let none = backup_warning(&cfg, now + 2 * day).unwrap();
        assert!(none.starts_with("no backup of state.db"), "{none}");
        backup(&cfg, &store).unwrap();
        assert_eq!(backup_warning(&cfg, now + day), None);
        let stale = backup_warning(&cfg, now + 3 * day + Duration::from_secs(60)).unwrap();
        assert!(
            stale.starts_with("the newest backup of state.db is 3 days old"),
            "{stale}"
        );
    }

    #[test]
    fn names_are_matched_exactly() {
        assert_eq!(
            session_of("2026-01-01T00-00Z_job-1-2.jsonl"),
            Some("job-1-2")
        );
        assert_eq!(
            session_of("2026-01-01T00-00Z_task-3.jsonl.lock"),
            Some("task-3")
        );
        assert_eq!(session_of("job-1-2.jsonl"), None);
        assert_eq!(session_of("x_task-3.json"), None);
        assert_eq!(proc_of("12.exit.tmp"), Some(12));
        assert_eq!(proc_of("12.log"), Some(12));
        assert_eq!(proc_of("12.md"), None);
        assert_eq!(proc_of("x.log"), None);
    }

    #[test]
    fn old_inbox_files_go() {
        let home = crate::test_dir("retention-inbox");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n")).unwrap();
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        let dir = home.join("inbox");
        std::fs::create_dir_all(&dir).unwrap();
        let month_ago = std::time::SystemTime::now() - AGE - Duration::from_secs(86400);
        let file = |name: &str, old: bool| {
            let path = dir.join(name);
            std::fs::write(&path, "x").unwrap();
            if old {
                let f = std::fs::File::options().write(true).open(&path).unwrap();
                f.set_modified(month_ago).unwrap();
            }
            path
        };
        let gone = [
            file("discord-1433712346123456789-1.png", true),
            file("imessage-CCCC-3333-4444-10.jpg", true),
            file(".imessage-CCCC-3333-4444-2.part", true),
        ];
        let kept = [
            file("discord-1-1.png", false), // fresh
            file("notes.txt", true),        // the agent's own
            file("discord-x.png", true),
        ];
        std::os::unix::fs::symlink("/etc/hostname", dir.join("discord-2-1.png")).unwrap();
        assert_eq!(sweep(&cfg, &store, AGE).unwrap(), gone.len());
        assert!(gone.iter().all(|p| !p.exists()));
        assert!(kept.iter().all(|p| p.exists()));
        assert!(dir.join("discord-2-1.png").symlink_metadata().is_ok());
    }
}
