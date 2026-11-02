//! Deletes what background work leaves behind once it is 30 days old (DESIGN §4.1): job and task
//! sessions, ended processes' logs and exit files, and ended tasks' overflow reports. Never a
//! `main` session, which `/switch` goes back to, and never a file of work still open. Run hourly
//! from the scheduler's tick.

use crate::config::Config;
use crate::store::Store;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How old a leftover must be before it goes.
pub const AGE: Duration = Duration::from_secs(30 * 24 * 3600);

/// One sweep, as the agent (whose files these are). Returns how many files went.
pub fn sweep(cfg: &Config, store: &Store, age: Duration) -> rusqlite::Result<usize> {
    let open = store.tasks(0)?; // queued or running, a resume included
    let tasks: HashSet<i64> = open.iter().map(|t| t.id).collect();
    let sessions: HashSet<String> = open.iter().filter_map(|t| t.session.clone()).collect();
    let procs: HashSet<i64> = store.running_procs()?.iter().map(|r| r.0.id).collect();
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
        assert_eq!(store.add_task("g", "chat", "o").unwrap(), Some(1));
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
