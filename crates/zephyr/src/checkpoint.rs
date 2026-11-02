//! Checkpoints of the projects folder and `/rollback` (DESIGN §4.14), in Hermes's shadow-git
//! layout: one bare store at `<home>/.checkpoints`, and for each top-level folder of the projects
//! folder its own index (`index-<project>`) and ref (`refs/zephyr/<project>`). A snapshot is
//! taken before each run and commits only the projects that changed. Git runs as the agent
//! (`agent::cmd`): the store and the projects are the agent's. A safety net, not a security
//! boundary: an agent that deletes the store only takes away the owner's undo.

use crate::config::Config;
use anyhow::{anyhow, bail, Context};
use futures_util::StreamExt;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// The longest a snapshot may hold up a run; then its git is killed and the run goes ahead.
pub const LIMIT: Duration = Duration::from_secs(10);
/// Checkpoints kept per project by the hourly prune.
pub const KEEP: usize = 50;
/// How many `/rollback` lists.
const SHOWN: usize = 10;
/// How much of `/rollback diff` is sent.
const DIFF_CHARS: usize = 4000;
/// Never snapshotted. `.env*` files are covered with empty mounts (`host::masks`), which a
/// restore could not write over.
const EXCLUDE: &str = "node_modules/\ntarget/\n.venv/\n__pycache__/\ndist/\nbuild/\n*.log\n.env*\n";
/// The commit message line that ties a project's commit to its snapshot: `<millis> <project>`.
const MARK: &str = "zephyr-checkpoint: ";
const USAGE: &str = "[zephyr] usage: /rollback, /rollback diff <n>, /rollback <n> [project]";
const OFF: &str = "[zephyr] /rollback is off: this container was started without \
    ZEPHYR_PROJECTS; recreate it with `zephyr up`";

/// One snapshot at a time per store: the router and background runs take them side by side.
type Lock = std::sync::Arc<tokio::sync::Mutex<()>>;
static LOCKS: std::sync::Mutex<Vec<(PathBuf, Lock)>> = std::sync::Mutex::new(Vec::new());

#[derive(Clone)]
pub struct Checkpoints {
    store: PathBuf,
    projects: PathBuf,
}

/// One snapshot: the projects it found changed, each with its commit.
#[derive(Debug)]
struct Point {
    id: u128,
    time: i64,
    subject: String,
    commits: Vec<(String, String)>,
}

/// Before a run (a main prompt, a task, a job): a snapshot that never fails the run and never
/// holds it longer than `LIMIT`.
pub async fn before(cfg: &Config, text: &str) {
    let Some(cp) = Checkpoints::of(cfg) else {
        return;
    };
    let why = format!("before: {}", crate::router::preview(text));
    if let Err(e) = cp.snapshot(&why, LIMIT).await {
        eprintln!("zephyr: no checkpoint before this run: {e:#}");
    }
}

/// The owner's `/rollback [diff] [N [PROJECT]]`. `busy` names what is still running, which a
/// restore waits for.
pub async fn command(cfg: &Config, arg: &str, busy: Option<&str>) -> String {
    let Some(cp) = Checkpoints::of(cfg) else {
        return OFF.into();
    };
    let words: Vec<&str> = arg.split_whitespace().collect();
    let num = |w: &str| w.parse::<usize>().ok();
    let done = match words[..] {
        [] => cp.locked(cp.list()).await,
        ["diff", n] if num(n).is_some() => cp.locked(cp.diff(num(n).unwrap_or(0))).await,
        [n] | [n, _] if num(n).is_some() => {
            if let Some(busy) = busy {
                return format!("[zephyr] not rolled back: {busy}; /stop them first");
            }
            cp.restore(num(n).unwrap_or(0), words.get(1).copied()).await
        }
        _ => return USAGE.into(),
    };
    done.unwrap_or_else(|e| format!("[zephyr] /rollback failed: {e:#}"))
}

impl Checkpoints {
    pub fn new(home: &Path, projects: &Path) -> Self {
        Checkpoints {
            store: home.join(".checkpoints"),
            projects: projects.to_path_buf(),
        }
    }

    /// The store's lock.
    fn lock(&self) -> Lock {
        let mut locks = LOCKS.lock().unwrap();
        if let Some((_, l)) = locks.iter().find(|(p, _)| *p == self.store) {
            return l.clone();
        }
        let l = Lock::default();
        locks.push((self.store.clone(), l.clone()));
        l
    }

    pub fn of(cfg: &Config) -> Option<Self> {
        cfg.projects.as_deref().map(|p| Self::new(&cfg.home, p))
    }

    /// Commits every project that changed since its last checkpoint, waiting at most `limit`. A
    /// snapshot still going then (the first one over a big folder) finishes in the background
    /// rather than being killed, so the next one only has to look at what changed since.
    /// Returns the projects committed.
    pub async fn snapshot(&self, why: &str, limit: Duration) -> anyhow::Result<Vec<String>> {
        let Ok(one) =
            tokio::time::timeout(limit.min(Duration::from_secs(1)), self.lock().lock_owned()).await
        else {
            bail!("the previous snapshot is still running");
        };
        let (cp, why) = (self.clone(), why.to_string());
        let work = tokio::spawn(async move {
            let _one = one;
            cp.snap(&why).await
        });
        match tokio::time::timeout(limit, work).await {
            Ok(done) => done?,
            Err(_) => bail!(
                "still running after {} s; it finishes in the background and may hold this \
                 run's first changes",
                limit.as_secs()
            ),
        }
    }

    /// Runs `work` holding the store, for at most `LIMIT`.
    async fn locked(
        &self,
        work: impl std::future::Future<Output = anyhow::Result<String>>,
    ) -> anyhow::Result<String> {
        let work = async {
            let _one = self.lock().lock_owned().await;
            work.await
        };
        tokio::time::timeout(LIMIT, work)
            .await
            .map_err(|_| anyhow!("timed out after {} s", LIMIT.as_secs()))?
    }

    async fn snap(&self, why: &str) -> anyhow::Result<Vec<String>> {
        self.init()?;
        let id = crate::unix_millis();
        let mut done: Vec<(String, anyhow::Result<bool>)> =
            futures_util::stream::iter(self.projects())
                .map(|p| async move {
                    let r = self.snap_one(&p, why, id).await;
                    (p, r)
                })
                .buffer_unordered(4)
                .collect()
                .await;
        done.sort_by(|a, b| a.0.cmp(&b.0));
        let mut changed = vec![];
        let mut failed = vec![];
        for (p, r) in done {
            match r {
                Ok(true) => changed.push(p),
                Ok(false) => {}
                Err(e) => failed.push(format!("{p}: {e:#}")),
            }
        }
        if !failed.is_empty() {
            bail!("{}", failed.join("; "));
        }
        Ok(changed)
    }

    /// Commits `project` when its tree differs from its ref. Returns whether it did.
    async fn snap_one(&self, project: &str, why: &str, id: u128) -> anyhow::Result<bool> {
        let tree = match self.tree(project).await {
            Ok(tree) => tree,
            Err(_) => {
                // The index names an object a prune removed (it held state no commit kept):
                // start the index again, which re-reads every file once.
                self.forget_index(project);
                self.tree(project).await?
            }
        };
        let r = format!("refs/zephyr/{project}");
        let head = self
            .run(
                None,
                &["for-each-ref", "--format=%(objectname) %(tree)", &r],
            )
            .await?;
        let parent = match head.split_once(' ') {
            Some((_, old)) if old == tree => return Ok(false),
            Some((commit, _)) => Some(commit.to_string()),
            None => None,
        };
        let msg = format!("{why}\n\n{MARK}{id} {project}");
        let mut args = vec!["commit-tree", "--no-gpg-sign", &tree, "-m", &msg];
        if let Some(p) = &parent {
            args.extend(["-p", p]);
        }
        let commit = self.run(None, &args).await?;
        let old = parent.as_deref().unwrap_or("");
        self.run(None, &["update-ref", &r, &commit, old]).await?;
        Ok(true)
    }

    /// `project`'s folder as it is now, written to its index: the tree's id.
    async fn tree(&self, project: &str) -> anyhow::Result<String> {
        self.clear_locks(project);
        self.run(Some(project), &["add", "-A"]).await?;
        self.run(Some(project), &["write-tree"]).await
    }

    fn started(&self) -> bool {
        let _fs = crate::agent::fs();
        self.store.join("HEAD").exists()
    }

    /// Creates the store on first use.
    fn init(&self) -> anyhow::Result<()> {
        if self.started() {
            return Ok(());
        }
        let _fs = crate::agent::fs();
        check(
            &["init"],
            self.git(None, &["init", "-q", "--bare"]).output(),
        )?;
        std::fs::create_dir_all(self.store.join("info"))?;
        std::fs::write(self.store.join("info/exclude"), EXCLUDE)?;
        Ok(())
    }

    /// The top-level folders snapshotted: not hidden, not symlinks, named as a ref can be.
    fn projects(&self) -> Vec<String> {
        let _fs = crate::agent::fs();
        let entries = std::fs::read_dir(&self.projects).into_iter().flatten();
        let mut names: Vec<String> = entries
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| named(n))
            // Only repos: a folder without .git has no .gitignore either, and is often data (a
            // game world, model weights) far too big to copy at every run.
            .filter(|n| self.projects.join(n).join(".git").exists())
            .collect();
        names.sort();
        names
    }

    /// Locks a killed git left behind. Only called holding the store's lock, so none is live.
    fn clear_locks(&self, project: &str) {
        let _fs = crate::agent::fs();
        let index = self.store.join(format!("index-{project}.lock"));
        let reference = self.store.join(format!("refs/zephyr/{project}.lock"));
        for lock in [index, reference] {
            let _ = std::fs::remove_file(lock);
        }
    }

    fn forget_index(&self, project: &str) {
        let _fs = crate::agent::fs();
        let _ = std::fs::remove_file(self.store.join(format!("index-{project}")));
    }

    fn exists(&self, project: &str) -> bool {
        let _fs = crate::agent::fs();
        self.projects.join(project).is_dir()
    }

    /// git as the agent on the store, with `project` as its work tree and index when given.
    fn git(&self, project: Option<&str>, args: &[&str]) -> Command {
        let mut cmd = Command::new("git");
        crate::agent::cmd(&mut cmd)
            .args(args)
            .env("GIT_DIR", &self.store)
            // The same behaviour whatever the agent's own git config says.
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Zephyr")
            .env("GIT_AUTHOR_EMAIL", "zephyr@localhost")
            .env("GIT_COMMITTER_NAME", "Zephyr")
            .env("GIT_COMMITTER_EMAIL", "zephyr@localhost")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdin(Stdio::null());
        match project {
            Some(p) => cmd
                .env("GIT_WORK_TREE", self.projects.join(p))
                .env("GIT_INDEX_FILE", self.store.join(format!("index-{p}")))
                .current_dir(self.projects.join(p)),
            None => cmd.current_dir("/"),
        };
        cmd
    }

    async fn run(&self, project: Option<&str>, args: &[&str]) -> anyhow::Result<String> {
        let out = tokio::process::Command::from(self.git(project, args))
            .kill_on_drop(true)
            .output()
            .await;
        check(args, out)
    }

    fn run_sync(&self, args: &[&str]) -> anyhow::Result<String> {
        check(args, self.git(None, args).output())
    }

    /// Every snapshot, newest first.
    async fn history(&self) -> anyhow::Result<Vec<Point>> {
        if !self.started() {
            return Ok(vec![]);
        }
        let refs = self
            .run(
                None,
                &["for-each-ref", "--format=%(refname)", "refs/zephyr/"],
            )
            .await?;
        if refs.is_empty() {
            return Ok(vec![]);
        }
        let mut args = vec!["log", "--format=%H%x1f%ct%x1f%s%x1f%b%x1e"];
        args.extend(refs.lines());
        let log = self.run(None, &args).await?;
        let mut points: BTreeMap<u128, Point> = BTreeMap::new();
        for entry in log.split('\x1e').map(str::trim).filter(|e| !e.is_empty()) {
            let mut f = entry.splitn(4, '\x1f');
            let (Some(commit), Some(time), Some(subject), Some(body)) =
                (f.next(), f.next(), f.next(), f.next())
            else {
                continue;
            };
            let mark = body.lines().find_map(|l| l.strip_prefix(MARK));
            let Some((id, project)) = mark.and_then(|m| m.split_once(' ')) else {
                continue;
            };
            let Ok(id) = id.parse() else { continue };
            let point = points.entry(id).or_insert_with(|| Point {
                id,
                time: time.parse().unwrap_or(0),
                subject: subject.to_string(),
                commits: vec![],
            });
            point
                .commits
                .push((project.to_string(), commit.to_string()));
        }
        Ok(points.into_values().rev().collect())
    }

    /// `/rollback`: the newest checkpoints, and how much changed since each.
    async fn list(&self) -> anyhow::Result<String> {
        let points = self.history().await?;
        if points.is_empty() {
            return Ok(
                "[zephyr] no checkpoints yet: one is taken before each run that finds \
                the projects changed"
                    .into(),
            );
        }
        self.refresh(&points).await?;
        let mut memo = HashMap::new();
        let mut lines = vec![];
        for (n, point) in points.iter().take(SHOWN).enumerate() {
            let mut changed = 0;
            for (project, commit) in state_at(&points, point.id, None) {
                if !memo.contains_key(&commit) {
                    let count = self.changed(&project, &commit).await?.lines().count();
                    memo.insert(commit.clone(), count);
                }
                changed += memo[&commit];
            }
            let names: Vec<&str> = point.commits.iter().map(|c| c.0.as_str()).collect();
            lines.push(format!(
                "#{}, {}, {}, {} ({changed} files changed since)",
                n + 1,
                ago(point.time),
                label(&point.subject),
                names.join(", "),
            ));
        }
        Ok(format!(
            "[zephyr] checkpoints, newest first (/rollback diff N, /rollback N [project]):\n{}",
            lines.join("\n")
        ))
    }

    /// `/rollback diff N`: per project, what changed since checkpoint N.
    async fn diff(&self, n: usize) -> anyhow::Result<String> {
        let points = self.history().await?;
        let point = pick(&points, n)?;
        self.refresh(&points).await?;
        let mut out = String::new();
        for (project, commit) in state_at(&points, point.id, None) {
            let stat = match self.exists(&project) {
                true => {
                    let args = ["diff", "--cached", "--stat=72", &commit];
                    self.run(Some(&project), &args).await?
                }
                false => "the folder is gone".into(),
            };
            if !stat.is_empty() {
                out.push_str(&format!("{project}:\n{stat}\n"));
            }
        }
        if out.is_empty() {
            return Ok(format!("[zephyr] nothing changed since #{n}"));
        }
        if let Some((i, _)) = out.char_indices().nth(DIFF_CHARS) {
            out.truncate(i);
            out.push_str("\n…");
        }
        Ok(format!(
            "[zephyr] changes since #{n} ({}, {}); /rollback {n} undoes them:\n{}",
            label(&point.subject),
            ago(point.time),
            out.trim_end()
        ))
    }

    /// `/rollback N [PROJECT]`: puts the projects (or one) back as they were at checkpoint N,
    /// after a checkpoint of how they are now, so `/rollback 1` undoes it. Files ignored by the
    /// store or a project's `.gitignore` are left alone.
    async fn restore(&self, n: usize, only: Option<&str>) -> anyhow::Result<String> {
        let _one = tokio::time::timeout(LIMIT, self.lock().lock_owned())
            .await
            .map_err(|_| anyhow!("a checkpoint is being taken; try again"))?;
        // N names a checkpoint as listed before the one taken below.
        let points = self.history().await?;
        let point = pick(&points, n)?;
        let to = state_at(&points, point.id, only);
        if to.is_empty() {
            bail!(
                "{} has no checkpoint as old as #{n}",
                only.unwrap_or("nothing")
            );
        }
        tokio::time::timeout(LIMIT, self.snap("before rollback"))
            .await
            .map_err(|_| anyhow!("timed out after {} s", LIMIT.as_secs()))
            .and_then(|r| r)
            .context("could not save the projects as they are first, so nothing changed")?;
        for (project, commit) in &to {
            {
                let _fs = crate::agent::fs();
                std::fs::create_dir_all(self.projects.join(project))?;
            }
            self.clear_locks(project);
            // Not timed: a restore cut halfway would be worse than a slow one.
            self.run(Some(project), &["read-tree", "-u", "--reset", commit])
                .await
                .with_context(|| format!("restoring {project}"))?;
        }
        let names: Vec<&str> = to.keys().map(String::as_str).collect();
        Ok(format!(
            "[zephyr] rolled back {} to #{n} ({}, {}); /rollback 1 undoes it",
            names.join(", "),
            label(&point.subject),
            ago(point.time),
        ))
    }

    /// Brings the indexes of the projects in `points` up to date with their folders.
    async fn refresh(&self, points: &[Point]) -> anyhow::Result<()> {
        let projects: std::collections::BTreeSet<&str> = points
            .iter()
            .flat_map(|p| p.commits.iter().map(|c| c.0.as_str()))
            .collect();
        for project in projects.into_iter().filter(|p| self.exists(p)) {
            if self.tree(project).await.is_err() {
                self.forget_index(project);
                self.tree(project).await?;
            }
        }
        Ok(())
    }

    /// The files of `project` that differ from `commit` (its index refreshed first).
    async fn changed(&self, project: &str, commit: &str) -> anyhow::Result<String> {
        match self.exists(project) {
            true => {
                let args = ["diff", "--cached", "--name-only", "--no-renames", commit];
                self.run(Some(project), &args).await
            }
            false => {
                let args = ["ls-tree", "-r", "--name-only", commit];
                self.run(None, &args).await
            }
        }
    }

    /// Keeps the newest `keep` checkpoints of each project: its chain is rebuilt from their trees
    /// (no file is stored again), then git drops the rest. Skipped while a snapshot runs.
    /// Returns how many checkpoints went.
    pub fn prune(&self, keep: usize) -> anyhow::Result<usize> {
        let Ok(_one) = self.lock().try_lock_owned() else {
            return Ok(0);
        };
        if !self.started() {
            return Ok(0);
        }
        let mut gone = 0;
        let refs = self.run_sync(&["for-each-ref", "--format=%(refname)", "refs/zephyr/"])?;
        for r in refs.lines() {
            let count: usize = self.run_sync(&["rev-list", "--count", r])?.parse()?;
            if count <= keep {
                continue;
            }
            let old = self.run_sync(&["rev-parse", r])?;
            let n = keep.to_string();
            let log = self.run_sync(&["log", "-n", &n, "--format=%T%x1f%ct%x1f%B%x1e", r])?;
            let kept: Vec<&str> = log
                .split('\x1e')
                .map(str::trim)
                .filter(|e| !e.is_empty())
                .collect();
            let mut parent: Option<String> = None;
            for entry in kept.into_iter().rev() {
                let mut f = entry.splitn(3, '\x1f');
                let (Some(tree), Some(time), Some(msg)) = (f.next(), f.next(), f.next()) else {
                    bail!("unreadable checkpoint in {r}");
                };
                let mut args = vec!["commit-tree", "--no-gpg-sign", tree, "-m", msg];
                if let Some(p) = &parent {
                    args.extend(["-p", p]);
                }
                let date = format!("{time} +0000");
                let mut cmd = self.git(None, &args);
                cmd.env("GIT_AUTHOR_DATE", &date)
                    .env("GIT_COMMITTER_DATE", &date);
                parent = Some(check(&args, cmd.output())?);
            }
            let new = parent.context("no checkpoint to keep")?;
            self.run_sync(&["update-ref", r, &new, &old])?;
            gone += count - keep;
        }
        if gone > 0 {
            self.run_sync(&["gc", "--prune=now", "--quiet"])?;
        }
        Ok(gone)
    }
}

fn check(args: &[&str], out: std::io::Result<Output>) -> anyhow::Result<String> {
    let out = out.context("running git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A folder name a ref can carry. shortcut: other projects (a space in the name) are skipped;
/// encode names if that ever matters.
fn named(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-+".contains(c))
}

fn pick(points: &[Point], n: usize) -> anyhow::Result<&Point> {
    n.checked_sub(1)
        .and_then(|i| points.get(i))
        .ok_or_else(|| anyhow!("no checkpoint #{n}; /rollback lists them"))
}

/// Each project's commit as it stood at snapshot `id`: its newest at or before it.
fn state_at(points: &[Point], id: u128, only: Option<&str>) -> BTreeMap<String, String> {
    let mut at = BTreeMap::new();
    for point in points.iter().filter(|p| p.id <= id) {
        for (project, commit) in &point.commits {
            if only.is_none_or(|o| o == project) {
                at.entry(project.clone()).or_insert_with(|| commit.clone());
            }
        }
    }
    at
}

/// `before "fix the login…"` for a run's checkpoint, the subject itself otherwise.
fn label(subject: &str) -> String {
    match subject.strip_prefix("before: ") {
        Some(text) => format!("before \"{text}\""),
        None => subject.to_string(),
    }
}

fn ago(time: i64) -> String {
    let s = (crate::now() - time).max(0);
    match s {
        0..60 => format!("{s} s ago"),
        60..3600 => format!("{} min ago", s / 60),
        3600..86400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh home and projects folder per call, with one project `app`.
    fn setup() -> (Checkpoints, PathBuf) {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = crate::test_dir(&format!("checkpoint-{n}"));
        let projects = dir.join("projects");
        std::fs::create_dir_all(projects.join("app/.git")).unwrap();
        std::fs::create_dir_all(projects.join(".hidden")).unwrap();
        (Checkpoints::new(&dir, &projects), projects.join("app"))
    }

    async fn say(cp: &Checkpoints, arg: &str) -> String {
        let cfg: Config = toml::from_str(&format!(
            "rpi = '/r'\nhome = {:?}\n",
            cp.store.parent().unwrap()
        ))
        .unwrap();
        let cfg = Config {
            projects: Some(cp.projects.clone()),
            ..cfg
        };
        command(&cfg, arg, None).await
    }

    fn read(path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    #[tokio::test]
    async fn nothing_changed_means_no_new_checkpoint() {
        let (cp, app) = setup();
        std::fs::write(app.join("a.txt"), "one").unwrap();
        assert_eq!(cp.snapshot("before: x", LIMIT).await.unwrap(), ["app"]);
        assert!(cp.snapshot("before: y", LIMIT).await.unwrap().is_empty());
        assert_eq!(cp.history().await.unwrap().len(), 1);
        std::fs::write(app.join("a.txt"), "two").unwrap();
        assert_eq!(cp.snapshot("before: z", LIMIT).await.unwrap(), ["app"]);
        let list = say(&cp, "").await;
        // Not the age: a second may tick over.
        let lines: Vec<&str> = list.lines().collect();
        assert!(
            lines[1].starts_with("#1, ") && lines[2].starts_with("#2, "),
            "{list}"
        );
        assert!(
            lines[1].ends_with(", before \"z\", app (0 files changed since)"),
            "{list}"
        );
        assert!(
            lines[2].ends_with(", before \"x\", app (1 files changed since)"),
            "{list}"
        );
        assert!(!list.contains("hidden"), "{list}");
    }

    #[tokio::test]
    async fn rollback_restores_and_can_itself_be_undone() {
        let (cp, app) = setup();
        std::fs::create_dir_all(app.join("src")).unwrap();
        std::fs::write(app.join("src/changed.rs"), "old").unwrap();
        std::fs::write(app.join("deleted.txt"), "keep me").unwrap();
        std::fs::write(app.join("debug.log"), "log 1").unwrap();
        std::fs::write(app.join(".gitignore"), "secret.txt\n").unwrap();
        std::fs::write(app.join("secret.txt"), "s1").unwrap();
        cp.snapshot("before: fix the login", LIMIT).await.unwrap();

        // The run.
        std::fs::write(app.join("src/changed.rs"), "new").unwrap();
        std::fs::remove_file(app.join("deleted.txt")).unwrap();
        std::fs::create_dir_all(app.join("added")).unwrap();
        std::fs::write(app.join("added/new.txt"), "fresh").unwrap();
        std::fs::write(app.join("debug.log"), "log 2").unwrap();
        std::fs::write(app.join("secret.txt"), "s2").unwrap();

        let diff = say(&cp, "diff 1").await;
        assert!(diff.contains("src/changed.rs"), "{diff}");
        assert!(diff.contains("added/new.txt"), "{diff}");
        let reply = say(&cp, "1").await;
        assert!(
            reply.starts_with("[zephyr] rolled back app to #1"),
            "{reply}"
        );
        assert_eq!(read(&app.join("src/changed.rs")).unwrap(), "old");
        assert_eq!(read(&app.join("deleted.txt")).unwrap(), "keep me");
        assert!(!app.join("added").exists());
        // Ignored files are left as they are.
        assert_eq!(read(&app.join("debug.log")).unwrap(), "log 2");
        assert_eq!(read(&app.join("secret.txt")).unwrap(), "s2");

        // The rollback took a checkpoint first, so it is #1 now and puts the run's work back.
        let list = say(&cp, "").await;
        assert!(
            list.lines()
                .nth(1)
                .unwrap()
                .contains(", before rollback, app ("),
            "{list}"
        );
        let reply = say(&cp, "1").await;
        assert!(
            reply.starts_with("[zephyr] rolled back app to #1"),
            "{reply}"
        );
        assert_eq!(read(&app.join("src/changed.rs")).unwrap(), "new");
        assert!(!app.join("deleted.txt").exists());
        assert_eq!(read(&app.join("added/new.txt")).unwrap(), "fresh");
    }

    #[tokio::test]
    async fn one_project_rolls_back_alone_and_a_deleted_one_comes_back() {
        let (cp, app) = setup();
        let web = app.parent().unwrap().join("web");
        std::fs::create_dir_all(web.join(".git")).unwrap();
        std::fs::write(app.join("a"), "1").unwrap();
        std::fs::write(web.join("w"), "1").unwrap();
        cp.snapshot("before: x", LIMIT).await.unwrap();
        std::fs::write(app.join("a"), "2").unwrap();
        std::fs::write(web.join("w"), "2").unwrap();
        assert!(say(&cp, "1 web")
            .await
            .starts_with("[zephyr] rolled back web to #1"));
        assert_eq!(read(&web.join("w")).unwrap(), "1");
        assert_eq!(read(&app.join("a")).unwrap(), "2");

        std::fs::remove_dir_all(&app).unwrap();
        // #1 is the checkpoint the rollback took, which has app's "2".
        assert!(say(&cp, "1 app").await.contains("rolled back app"));
        assert_eq!(read(&app.join("a")).unwrap(), "2");
        assert!(say(&cp, "1 nope").await.contains("nope has no checkpoint"));
        assert!(say(&cp, "9").await.contains("no checkpoint #9"));
        assert!(say(&cp, "bogus").await.starts_with("[zephyr] usage"));
    }

    #[tokio::test]
    async fn prune_keeps_the_newest_fifty() {
        let (cp, app) = setup();
        for i in 0..55 {
            std::fs::write(app.join("n.txt"), i.to_string()).unwrap();
            cp.snapshot(&format!("before: run {i}"), LIMIT)
                .await
                .unwrap();
        }
        assert_eq!(cp.prune(KEEP).unwrap(), 5);
        let points = cp.history().await.unwrap();
        assert_eq!(points.len(), KEEP);
        assert_eq!(points[0].subject, "before: run 54");
        assert_eq!(points[KEEP - 1].subject, "before: run 5");
        assert_eq!(cp.prune(KEEP).unwrap(), 0);
        // The rebuilt chain still restores, and snapshots go on from it.
        std::fs::write(app.join("n.txt"), "after").unwrap();
        assert!(say(&cp, "50").await.contains("rolled back app to #50"));
        assert_eq!(read(&app.join("n.txt")).unwrap(), "5");
    }

    #[tokio::test]
    async fn a_slow_snapshot_finishes_in_the_background() {
        let (cp, app) = setup();
        std::fs::write(app.join("a"), "1").unwrap();
        let e = cp
            .snapshot("before: x", Duration::from_millis(1))
            .await
            .unwrap_err();
        assert!(e.to_string().contains("background"), "{e:#}");
        while cp.lock().try_lock_owned().is_err() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // It was committed, not killed: the next one finds nothing new.
        assert!(cp.snapshot("before: y", LIMIT).await.unwrap().is_empty());
        assert!(say(&cp, "").await.contains("\"x\""));
        // A git killed by a daemon crash can leave its locks behind.
        std::fs::write(cp.store.join("index-app.lock"), "").unwrap();
        std::fs::write(cp.store.join("refs/zephyr/app.lock"), "").unwrap();
        std::fs::write(app.join("a"), "2").unwrap();
        assert_eq!(cp.snapshot("before: z", LIMIT).await.unwrap(), ["app"]);
    }

    #[tokio::test]
    async fn folders_that_are_not_repos_are_left_out() {
        let (cp, app) = setup();
        let data = app.parent().unwrap().join("world");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("region.dat"), "big").unwrap();
        std::fs::write(app.join("a"), "1").unwrap();
        assert_eq!(cp.snapshot("before: x", LIMIT).await.unwrap(), ["app"]);
    }

    #[tokio::test]
    async fn an_index_naming_a_pruned_object_is_rebuilt() {
        let (cp, app) = setup();
        std::fs::write(app.join("a"), "1").unwrap();
        cp.snapshot("before: x", LIMIT).await.unwrap();
        // Refreshed by a listing but never committed, then dropped by a prune's gc.
        std::fs::write(app.join("a"), "2").unwrap();
        say(&cp, "").await;
        cp.run_sync(&["gc", "--prune=now", "--quiet"]).unwrap();
        assert_eq!(cp.snapshot("before: y", LIMIT).await.unwrap(), ["app"]);
    }

    #[test]
    fn names_and_labels() {
        assert!(named("zephyr") && named("my_app-2.0"));
        assert!(!named(".parity-lanes") && !named("a b") && !named("x.lock") && !named("a..b"));
        assert_eq!(label("before: hi"), "before \"hi\"");
        assert_eq!(label("before rollback"), "before rollback");
    }
}
