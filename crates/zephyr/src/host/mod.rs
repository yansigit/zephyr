//! Host-side commands: set up `~/.zephyr` and drive the `zephyr` container through the docker CLI.
//! Inside the container the same binary runs the daemon commands instead (see `in_container`).

pub mod conf;
mod setup;

use anyhow::{bail, Context};
use conf::Data;
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const USAGE: &str = "usage: zephyr <command>
  init [--non-interactive --projects DIR --provider ID.. --account NAME=PROVIDER/MODEL.. --model P/M --no-start]
  up [--build] | down | restart | logs [-f] | status | upgrade
  login <provider> [--account NAME] [--model P/M] [--no-verify]
  accounts [list | order NAME.. | remove NAME]
  model [--account NAME] [PROVIDER/MODEL]
  channel add discord|imessage
  config get|set KEY [VALUE]
  chat | attach | pause | resume | reload
  doctor [--fix]
  uninstall [--keep-data] [--yes]
env: ZEPHYR_CONTAINER (default zephyr), ZEPHYR_IMAGE, ZEPHYR_SRC (for --build); data in ~/.zephyr";

const COMMANDS: [&str; 19] = [
    "init",
    "up",
    "down",
    "restart",
    "logs",
    "status",
    "upgrade",
    "login",
    "accounts",
    "model",
    "channel",
    "config",
    "chat",
    "attach",
    "doctor",
    "uninstall",
    "pause",
    "resume",
    "reload",
];

/// The image sets ZEPHYR_IN_CONTAINER. ZEPHYR_CONFIG (set in the image and by tests) also points
/// at a daemon config, so it selects the daemon commands too.
pub fn in_container() -> bool {
    std::env::var_os("ZEPHYR_IN_CONTAINER").is_some()
        || std::env::var_os("ZEPHYR_CONFIG").is_some()
        || std::path::Path::new("/.dockerenv").exists()
}

pub fn is_command(cmd: &str) -> bool {
    COMMANDS.contains(&cmd)
}

pub struct Host {
    pub container: String,
    pub image: String,
    pub data: Data,
    pub interactive: bool,
}

impl Host {
    fn from_env() -> Self {
        let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        Host {
            container: env("ZEPHYR_CONTAINER").unwrap_or_else(|| "zephyr".into()),
            image: env("ZEPHYR_IMAGE").unwrap_or_else(|| {
                format!("ghcr.io/yansigit/zephyr:{}", env!("CARGO_PKG_VERSION"))
            }),
            data: Data {
                dir: home().join(".zephyr"),
            },
            interactive: std::io::stdin().is_terminal(),
        }
    }

    /// `docker exec [flags] [-e K=V].. <container> argv..`
    fn exec(&self, flags: &[&str], env: &[(&str, &str)], argv: &[&str]) -> Command {
        let mut cmd = Command::new("docker");
        cmd.arg("exec").args(flags);
        for (k, v) in env {
            cmd.arg("-e").arg(format!("{k}={v}"));
        }
        cmd.arg(&self.container).args(argv);
        cmd
    }

    /// The container's state (`running`, `exited`, ...), or None when it does not exist.
    fn state(&self) -> Option<String> {
        let out = docker(&["inspect", "-f", "{{.State.Status}}", &self.container])
            .stderr(Stdio::null())
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn running(&self) -> bool {
        self.state().as_deref() == Some("running")
    }

    fn need_running(&self) -> anyhow::Result<()> {
        if !self.running() {
            bail!("container {} is not running: `zephyr up`", self.container);
        }
        Ok(())
    }

    /// Pulls the image, logging in to ghcr.io with `gh` when needed; builds it when that fails.
    fn ensure_image(&self, build: bool, refresh: bool) -> anyhow::Result<()> {
        if build {
            return self.build();
        }
        if !refresh && quiet(docker(&["image", "inspect", &self.image])) {
            return Ok(());
        }
        if quiet(docker(&["pull", &self.image])) {
            return Ok(());
        }
        if let (Some(token), Some(user)) = (
            gh(&["auth", "token"]),
            gh(&["api", "user", "--jq", ".login"]),
        ) {
            let registry = self.image.split('/').next().unwrap_or("ghcr.io");
            let mut login = docker(&["login", registry, "-u", &user, "--password-stdin"]);
            if feed(&mut login, &token).is_ok() && quiet(docker(&["pull", &self.image])) {
                return Ok(());
            }
        }
        println!("could not pull {}; building it locally", self.image);
        self.build()
    }

    /// `docker build` from ZEPHYR_SRC (default: this source tree). The gh token goes in as a
    /// BuildKit secret through the environment, never as an argument or a layer.
    fn build(&self) -> anyhow::Result<()> {
        let src = std::env::var_os("ZEPHYR_SRC")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../..")));
        let dockerfile = src.join("deploy/Dockerfile");
        if !dockerfile.exists() {
            bail!(
                "{} not found: set ZEPHYR_SRC to a zephyr checkout",
                dockerfile.display()
            );
        }
        let mut cmd = docker(&["build", "-t", &self.image, "-f"]);
        cmd.arg(&dockerfile);
        if let Some(token) = gh(&["auth", "token"]) {
            cmd.args(["--secret", "id=gh_token,env=GH_TOKEN"])
                .env("GH_TOKEN", token);
        }
        run(cmd.arg(&src))
    }

    /// Has the running daemon reload its settings, and prints what it did with them.
    pub(crate) fn apply(&self) -> anyhow::Result<()> {
        if !self.running() {
            println!("saved; applies when zephyr starts (`zephyr up`)");
            return Ok(());
        }
        run(&mut self.exec(&[], &[], &["zephyr", "reload"]))
    }

    /// Recreates the container from the image. Secrets come from secrets.env, read by the daemon.
    fn up(&self) -> anyhow::Result<()> {
        let projects = self
            .data
            .projects()?
            .context("no projects dir: run `zephyr init`")?;
        let _ = quiet(docker(&["rm", "-f", &self.container]));
        let ids = unsafe { (libc::getuid(), libc::getgid()) };
        let data = self.data.dir.display();
        let projects = projects.display();
        let mut cmd = docker(&["run", "-d", "--name", &self.container, "--init"]);
        cmd.args([
            "--restart",
            "unless-stopped",
            "--stop-timeout",
            "30",
            "--network",
            "host",
        ])
        // The agent runs here: no capabilities, no setuid escalation, and no fork bomb (DESIGN §4.4).
        .args(["--cap-drop", "ALL", "--security-opt", "no-new-privileges"])
        .args(["--pids-limit", "4096"])
        .args(["--user", &format!("{}:{}", ids.0, ids.1)])
        .args([
            "-e",
            "HOME=/zephyr",
            "-e",
            "ZEPHYR_CONFIG=/zephyr/zephyr.toml",
        ])
        .args(["-e", "RPI_CODING_AGENT_DIR=/zephyr/.rpi/agent"])
        .args(["--mount", &format!("type=bind,src={data},dst=/zephyr")])
        .args([
            "--mount",
            &format!("type=bind,src={projects},dst={projects}"),
        ])
        .args(masks(&projects.to_string())?)
        .args(
            self.data
                .host_get("memory")?
                .map(|m| ["--memory".to_string(), m])
                .into_iter()
                .flatten(),
        )
        .args(["-w", "/zephyr", &self.image])
        .stdout(Stdio::null());
        run(&mut cmd)?;
        println!("started {}. Logs: zephyr logs -f", self.container);
        Ok(())
    }

    fn doctor(&self, fix: bool) -> anyhow::Result<()> {
        let mut ok = check("docker answers", quiet(docker(&["version"])));
        let dir = &self.data.dir;
        if fix {
            self.data.ensure_layout()?;
            for p in [self.data.secrets(), self.data.token()] {
                conf::write_private(&p, &std::fs::read_to_string(&p)?)?;
            }
        }
        ok &= check(
            &format!("{} exists and is private", dir.display()),
            private(dir),
        );
        for p in [self.data.secrets(), self.data.token()] {
            ok &= check(&format!("{} is private (0600)", p.display()), private(&p));
        }
        ok &= check(
            &format!("{} parses", self.data.config().display()),
            self.data.load().is_ok(),
        );
        if fix && !self.running() && self.data.projects()?.is_some() {
            self.ensure_image(false, false)?;
            self.up()?;
        }
        let running = self.running();
        ok &= check(&format!("container {} is running", self.container), running);
        if running {
            ok &= self
                .exec(&[], &[], &["zephyr", "doctor"])
                .status()?
                .success();
        }
        if !ok {
            bail!(
                "doctor found problems{}",
                if fix { "" } else { " (try --fix)" }
            );
        }
        Ok(())
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn docker(args: &[&str]) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(args);
    cmd
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    let status = cmd
        .status()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !status.success() {
        let args: Vec<_> = cmd
            .get_args()
            .take(2)
            .map(|a| a.to_string_lossy())
            .collect();
        bail!(
            "{} {} failed ({status})",
            cmd.get_program().to_string_lossy(),
            args.join(" ")
        );
    }
    Ok(())
}

/// True when the command succeeds; its output is dropped.
fn quiet(mut cmd: Command) -> bool {
    cmd.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Stdout of a successful command.
fn output(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.stderr(Stdio::inherit()).output()?;
    if !out.status.success() {
        bail!("{:?} failed ({})", cmd.get_program(), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Runs `cmd` with `secret` on stdin, output dropped: the only way secrets reach a process.
fn feed(cmd: &mut Command, secret: &str) -> anyhow::Result<()> {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .context("stdin")?
        .write_all(format!("{secret}\n").as_bytes())?;
    let status = child.wait()?;
    if !status.success() {
        bail!("{:?} failed ({status})", cmd.get_program());
    }
    Ok(())
}

/// `gh <args>` trimmed stdout, or None when gh is missing or fails.
fn gh(args: &[&str]) -> Option<String> {
    let out = Command::new("gh")
        .args(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (out.status.success() && !text.is_empty()).then_some(text)
}

/// Empty mounts over secrets inside the projects dir (DESIGN §4.4).
/// shortcut: computed at each `up`, so a secret created later stays visible until the next `up`.
fn masks(projects: &str) -> anyhow::Result<Vec<String>> {
    let mut paths = vec![
        format!("{projects}/.parity-env.sh"),
        format!("{projects}/.parity-lanes/archive"),
    ];
    let found = output(Command::new("find").args([
        projects,
        "-name",
        "node_modules",
        "-prune",
        "-o",
        "-name",
        ".env*",
        "-print0",
    ]))?;
    paths.extend(
        found
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(String::from),
    );
    let mut args = vec![];
    for p in paths {
        let path = std::path::Path::new(&p);
        let spec = if path.is_dir() {
            format!("type=tmpfs,dst={p}")
        } else if path.exists() {
            format!("type=bind,src=/dev/null,dst={p},readonly")
        } else {
            continue;
        };
        args.extend(["--mount".to_string(), spec]);
    }
    Ok(args)
}

fn private(path: &std::path::Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| std::os::unix::fs::MetadataExt::mode(&m) & 0o077 == 0)
}

fn check(label: &str, ok: bool) -> bool {
    println!("{} {label}", if ok { "ok  " } else { "FAIL" });
    ok
}

/// Removes `--name` flags from the arguments as they are asked for; `rest` refuses leftovers.
pub struct Args(Vec<String>);

impl Args {
    fn flag(&mut self, name: &str) -> bool {
        let before = self.0.len();
        self.0.retain(|a| a != name);
        before != self.0.len()
    }

    fn values(&mut self, name: &str) -> anyhow::Result<Vec<String>> {
        let mut found = vec![];
        while let Some(i) = self.0.iter().position(|a| a == name) {
            if i + 1 >= self.0.len() {
                bail!("{name} needs a value");
            }
            found.push(self.0.remove(i + 1));
            self.0.remove(i);
        }
        Ok(found)
    }

    fn value(&mut self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.values(name)?.pop())
    }

    fn rest(self) -> anyhow::Result<Vec<String>> {
        if let Some(a) = self.0.iter().find(|a| a.starts_with("--")) {
            bail!("unknown option {a}\n{USAGE}");
        }
        Ok(self.0)
    }
}

pub fn main(cmd: &str, args: Vec<String>) -> anyhow::Result<()> {
    let h = Host::from_env();
    let mut a = Args(args);
    match cmd {
        "init" => setup::init(&h, a),
        "up" => {
            let build = a.flag("--build");
            a.rest()?;
            h.ensure_image(build, false)?;
            h.up()
        }
        "upgrade" => {
            a.rest()?;
            h.ensure_image(false, true)?;
            h.up()
        }
        "down" => {
            a.rest()?;
            run(&mut docker(&["stop", &h.container]))?;
            run(&mut docker(&["rm", &h.container]))
        }
        "restart" => {
            a.rest()?;
            run(&mut docker(&["restart", &h.container]))
        }
        "logs" => {
            let follow = a.flag("-f") | a.flag("--follow");
            a.rest()?;
            let mut cmd = docker(&["logs"]);
            if follow {
                cmd.arg("-f");
            }
            run(cmd.arg(&h.container))
        }
        "status" => {
            a.rest()?;
            println!(
                "container {}: {}",
                h.container,
                h.state().as_deref().unwrap_or("not created")
            );
            if h.running() {
                run(&mut h.exec(&[], &[], &["zephyr", "status"]))?;
            }
            Ok(())
        }
        "doctor" => {
            let fix = a.flag("--fix");
            a.rest()?;
            h.doctor(fix)
        }
        "reload" => {
            a.rest()?;
            h.need_running()?;
            h.apply()
        }
        "chat" | "pause" | "resume" => {
            a.rest()?;
            h.need_running()?;
            let flags: &[&str] = if cmd == "chat" { &["-it"] } else { &[] };
            run(&mut h.exec(flags, &[], &["zephyr", cmd]))
        }
        "attach" => {
            a.rest()?;
            h.need_running()?;
            let doc = h.data.load()?;
            let port = conf::get(&doc, "attach.port").context("no [attach] port in zephyr.toml")?;
            let token = std::fs::read_to_string(h.data.token())?;
            // `-e NAME` without a value: docker copies it from this process's environment.
            let mut cmd = h.exec(
                &["-it", "-e", "RPI_SERVER_TOKEN"],
                &[],
                &["rpi", "--connect"],
            );
            cmd.arg(format!("127.0.0.1:{port}"))
                .env("RPI_SERVER_TOKEN", token.trim());
            run(&mut cmd)
        }
        "config" => {
            let rest = a.rest()?;
            let mut doc = h.data.load()?;
            match rest.iter().map(String::as_str).collect::<Vec<_>>()[..] {
                ["get", key] => {
                    println!(
                        "{}",
                        conf::get(&doc, key).with_context(|| format!("{key} is not set"))?
                    );
                }
                ["set", key, value] => {
                    conf::set(&mut doc, key, value)?;
                    // Refuse what the daemon would refuse (unknown keys, a bad time zone).
                    toml::from_str::<crate::config::Config>(&doc.to_string())
                        .map_err(anyhow::Error::from)
                        .and_then(|c| c.validate())
                        .with_context(|| {
                            format!("{key} = {value} would make the config invalid")
                        })?;
                    h.data.save(&doc)?;
                    println!("set {key}");
                    h.apply()?;
                }
                _ => bail!("usage: zephyr config get KEY | config set KEY VALUE"),
            }
            Ok(())
        }
        "uninstall" => {
            let keep = a.flag("--keep-data");
            let yes = a.flag("--yes");
            a.rest()?;
            let dir = h.data.dir.display().to_string();
            let confirmed = keep
                || yes
                || (h.interactive
                    && dialoguer::Confirm::new()
                        .with_prompt(format!("Delete {dir} (config, memory, credentials)?"))
                        .default(false)
                        .interact()?);
            if !confirmed {
                bail!("not deleting {dir}: pass --keep-data or --yes");
            }
            let _ = quiet(docker(&["rm", "-f", &h.container]));
            let _ = quiet(docker(&["rmi", &h.image]));
            if !keep {
                std::fs::remove_dir_all(&h.data.dir).or_else(|e| {
                    if e.kind() == std::io::ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e)
                    }
                })?;
            }
            println!(
                "removed {} and its image{}",
                h.container,
                if keep { "" } else { " and data" }
            );
            Ok(())
        }
        "login" => setup::login_cmd(&h, a),
        "accounts" => setup::accounts(&h, a),
        "model" => setup::model(&h, a),
        "channel" => setup::channel(&h, a),
        _ => bail!("{USAGE}"),
    }
}
