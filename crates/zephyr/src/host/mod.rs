//! Host-side commands: set up `~/.zephyr` and drive the `zephyr` container through the docker CLI.
//! Inside the container the same binary runs the daemon commands instead (see `in_container`).

mod alert;
pub mod conf;
mod setup;

use anyhow::{bail, Context};
use conf::{Data, DAEMON_FILES, DAEMON_MOUNT};
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const USAGE: &str = "usage: zephyr <command>   (zephyr --version: this CLI's and the container's)
  init [--non-interactive --projects DIR --provider ID.. --account NAME=PROVIDER/MODEL.. --model P/M --no-start]
  up [--build] | down | restart | logs [-f] | status | upgrade [VERSION]
  login <provider> [--account NAME] [--model P/M] [--no-verify]
  accounts [list | order NAME.. | remove NAME]
  model [--account NAME] [PROVIDER/MODEL]
  channel add|remove discord|imessage | list
  skill add DIR | list | rm NAME
  mcp add NAME (--url URL [--header K=V].. | -- CMD ARGS..) [--secret NAME].. | list | rm NAME
  packages add NAME.. | rm NAME.. | list
  subagent list | add NAME [--model P/M] [--prompt TEXT] | edit NAME [--model P/M|default] [--prompt TEXT] | rm NAME
  vault list | add login NAME --site URL [--user U] [--totp] | add token NAME | edit NAME | rm NAME | revoke NAME [PROGRAM]
  config get KEY | set KEY VALUE   (a known setting goes through the daemon while it runs)
  chat | attach [--stock] | shell [bash|zsh] | pause | resume | reload
  doctor [--fix]
  alert setup TOPIC [--server URL] | check | test | off
  uninstall [--keep-data] [--yes]
env: ZEPHYR_CONTAINER (default zephyr), ZEPHYR_IMAGE, ZEPHYR_SRC (for --build); data in ~/.zephyr";

/// The published image, tagged with the version (and `latest`, the newest release).
const REPO: &str = "ghcr.io/yansigit/zephyr:";

/// The image built from it with host.toml's apt `packages` (`zephyr packages`), never pushed.
const LOCAL: &str = "zephyr-local:";

/// This CLI's version, and the image it runs by default.
const ME: &str = env!("CARGO_PKG_VERSION");

/// The daemon's config files a rollback restores with state.db: an older daemon refuses keys it
/// does not know.
const ROLLED_BACK: [&str; 3] = ["zephyr.toml", "vault.toml", "host.toml"];

const COMMANDS: [&str; 26] = [
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
    "shell",
    "doctor",
    "uninstall",
    "pause",
    "resume",
    "reload",
    "skill",
    "mcp",
    "packages",
    "vault",
    "subagent",
    "alert",
];

/// The image sets ZEPHYR_IN_CONTAINER. ZEPHYR_CONFIG (set in the image and by tests) also points
/// at a daemon config, so it selects the daemon commands too.
pub fn in_container() -> bool {
    std::env::var_os("ZEPHYR_IN_CONTAINER").is_some()
        || std::env::var_os("ZEPHYR_CONFIG").is_some()
        || std::path::Path::new("/.dockerenv").exists()
}

/// After `zephyr --version` on the host: the container's own version, which differs from this CLI's
/// after a `cargo install` without `zephyr upgrade`.
pub fn container_version() {
    let h = Host::from_env();
    if !h.running() {
        return;
    }
    let theirs = running_version(&h).unwrap_or_else(|| "unknown".into());
    let hint = if theirs == env!("CARGO_PKG_VERSION") {
        ""
    } else {
        " (not this CLI's; `zephyr upgrade` brings both to the newest release)"
    };
    println!("container {}: {theirs}{hint}", h.container);
}

/// The container's version: its `zephyr --version` when running, else its image's tag (a stopped
/// container's too); None without a container.
fn running_version(h: &Host) -> Option<String> {
    if h.running() {
        match h.exec(&[], &[], &["zephyr", "--version"]).output() {
            Ok(o) if o.status.success() && !o.stdout.is_empty() => {
                let text = String::from_utf8_lossy(&o.stdout);
                return Some(text.trim().trim_start_matches("zephyr ").to_string());
            }
            _ => {}
        }
    }
    // Images before 0.3.1 have no --version: the published image's tag, else the image's name.
    let out = docker(&["inspect", "-f", "{{.Config.Image}}", &h.container])
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let image = String::from_utf8_lossy(&out.stdout);
    let image = image.trim();
    let local = || image.strip_prefix(LOCAL)?.split('-').next();
    Some(
        image
            .strip_prefix(REPO)
            .or_else(local)
            .unwrap_or(image)
            .to_string(),
    )
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
            image: env("ZEPHYR_IMAGE")
                .unwrap_or_else(|| format!("{REPO}{}", env!("CARGO_PKG_VERSION"))),
            data: Data {
                dir: home().join(".zephyr"),
                daemon: home().join(".zephyr-daemon"),
                container: env("ZEPHYR_CONTAINER").unwrap_or_else(|| "zephyr".into()),
                image: env("ZEPHYR_IMAGE")
                    .unwrap_or_else(|| format!("{REPO}{}", env!("CARGO_PKG_VERSION"))),
            },
            interactive: std::io::stdin().is_terminal(),
        }
    }

    /// `docker exec` as the agent (this user's uid, in its home): rpi sign-ins and model lists,
    /// whose files must stay the agent's. Plain `exec` runs as root, the daemon's side.
    pub(crate) fn exec_agent(
        &self,
        flags: &[&str],
        env: &[(&str, &str)],
        argv: &[&str],
    ) -> Command {
        let ids = agent_ids();
        let user = format!("{}:{}", ids.0, ids.1);
        // The image's `ENV PATH` is root's; the agent's adds its own bin folder.
        let path = format!("PATH={}", crate::agent::AGENT_PATH);
        let flags = [&["-u", &user, "-w", "/zephyr", "-e", &path], flags].concat();
        self.exec(&flags, env, argv)
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

    /// A login shell in the container as the agent, on the desktop when it runs.
    fn shell(&self, shell: &str) -> anyhow::Result<()> {
        if !self.interactive || !std::io::stdout().is_terminal() {
            bail!("zephyr shell needs a terminal");
        }
        self.need_running()?;
        let env = [
            ("DISPLAY", ":99"),
            ("XAUTHORITY", "/zephyr/.zephyr-xauthority"),
        ];
        run(&mut self.exec_agent(&["-it", "-e", "TERM"], &env, &[shell, "-l"]))
    }

    fn state(&self) -> Option<String> {
        state(&self.container)
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
    /// Then builds the local image with host.toml's packages on top, when there are any.
    fn ensure_image(&self, build: bool, refresh: bool) -> anyhow::Result<()> {
        self.ensure_base(build, refresh)?;
        self.derive(&self.data.packages()?, build || refresh)
    }

    fn ensure_base(&self, build: bool, refresh: bool) -> anyhow::Result<()> {
        if build {
            return self.build();
        }
        if !refresh && quiet(docker(&["image", "inspect", &self.image])) {
            return Ok(());
        }
        if pull(&self.image) {
            return Ok(());
        }
        println!("could not pull {}; building it locally", self.image);
        self.build()
    }

    /// The image the container runs: `self.image`, or with apt `packages` the local image built
    /// from it (`local_image`).
    fn run_image(&self) -> anyhow::Result<String> {
        Ok(local_image(&self.image, &self.data.packages()?).unwrap_or_else(|| self.image.clone()))
    }

    /// Builds the local image of `packages` on `self.image` when it is missing, or always with
    /// `force` (docker's layer cache makes that quick when neither changed).
    /// The images run as root until the daemon drops to the agent, so apt-get needs no sudo.
    fn derive(&self, packages: &[String], force: bool) -> anyhow::Result<()> {
        let Some(tag) = local_image(&self.image, packages) else {
            return Ok(());
        };
        if !force && quiet(docker(&["image", "inspect", &tag])) {
            return Ok(());
        }
        println!("building {tag} with {}", packages.join(" "));
        let dockerfile = format!(
            "FROM {}\nRUN apt-get update && apt-get install -y --no-install-recommends {} \
             && rm -rf /var/lib/apt/lists/*\n",
            self.image,
            packages.join(" ")
        );
        let mut child = docker(&["build", "-t", &tag, "-"])
            .stdin(Stdio::piped())
            .spawn()
            .context("running docker build")?;
        child
            .stdin
            .take()
            .context("stdin")?
            .write_all(dockerfile.as_bytes())?;
        if !child.wait()?.success() {
            bail!(
                "building {tag} failed: is each of {} an Ubuntu 24.04 apt package?",
                packages.join(", ")
            );
        }
        Ok(())
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
    /// The daemon runs as root holding only SETUID, SETGID and KILL; the agent runs as this
    /// user, and cannot reach the daemon's files in `~/.zephyr-daemon` (DESIGN §4.4).
    fn up(&self) -> anyhow::Result<()> {
        // Before the first split the old container holds the files the migration moves, so it
        // goes first; otherwise it runs until the new one's arguments are all known.
        if self.data.direct() {
            self.remove();
        }
        self.migrate()?;
        let projects = self
            .data
            .projects()?
            .context("no projects dir: run `zephyr init`")?;
        let image = self.run_image()?;
        let ids = agent_ids();
        let data = self.data.dir.display();
        let daemon = self.data.daemon.display();
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
        // No file overrides, no setuid escalation, and no fork bomb.
        .args([
            "--cap-drop",
            "ALL",
            "--cap-add",
            "SETUID",
            "--cap-add",
            "SETGID",
        ])
        .args(["--cap-add", "KILL", "--security-opt", "no-new-privileges"])
        .args(["--pids-limit", "4096", "--user", "0:0"])
        .args([
            "-e",
            "HOME=/zephyr",
            "-e",
            &format!("ZEPHYR_CONFIG={DAEMON_MOUNT}/zephyr.toml"),
        ])
        .args(["-e", "RPI_CODING_AGENT_DIR=/zephyr/.rpi/agent"])
        .args(["-e", &format!("ZEPHYR_AGENT_UID={}", ids.0)])
        .args(["-e", &format!("ZEPHYR_AGENT_GID={}", ids.1)])
        // Turns on `/rollback`'s checkpoints of the projects folder (DESIGN §4.14).
        .args(["-e", &format!("ZEPHYR_PROJECTS={projects}")])
        .args(["--mount", &format!("type=bind,src={data},dst=/zephyr")])
        .args([
            "--mount",
            &format!("type=bind,src={daemon},dst={DAEMON_MOUNT}"),
        ])
        .args([
            "--mount",
            &format!("type=bind,src={projects},dst={projects}"),
        ])
        .args(masks(&projects.to_string())?)
        // Docker's json-file logs grow without end by default.
        .args(["--log-opt", "max-size=20m", "--log-opt", "max-file=5"])
        .args(["--ulimit", "nofile=65536:65536"])
        .args(
            (self.data.host_get("memory")?)
                .or_else(default_memory)
                .map(|m| ["--memory".to_string(), m])
                .into_iter()
                .flatten(),
        )
        // The agent's browser renders WebGL on the GPU; without one it is SwiftShader (§4.11).
        .args(if nvidia_runtime() {
            &[
                "--gpus",
                "all",
                "-e",
                "NVIDIA_DRIVER_CAPABILITIES=graphics,utility",
            ][..]
        } else {
            &[]
        })
        // Root cannot enter the agent's 0700 home; the daemon starts each child there as the agent.
        .args(["-w", "/", &image])
        .stdout(Stdio::null());

        // The old container steps aside (stopped, so its daemon drains) and comes back if the new
        // one cannot be created.
        let old = format!("{}-old", self.container);
        let was_running = self.running();
        let aside = self.state().is_some();
        if aside {
            // Left by an `up` that died between the rename and the removal.
            let _ = quiet(docker(&["rm", "-f", &old]));
            let _ = quiet(docker(&["stop", &self.container]));
            run(&mut docker(&["rename", &self.container, &old]))?;
        }
        // As `init` does: the extensions the image ships since this config was written. Only
        // with the old daemon stopped, which would load them; a convenience, so it only warns.
        match self
            .data
            .load()
            .and_then(|mut doc| self.data.add_image_extensions(&mut doc))
        {
            Ok(true) => println!("added the image's new extensions to zephyr.toml"),
            Ok(false) => {}
            Err(e) => eprintln!(
                "warning: could not add the image's new extensions: {e:#}; run `zephyr init` later"
            ),
        }
        if let Err(e) = run(&mut cmd) {
            if aside {
                let _ = quiet(docker(&["rm", "-f", &self.container]));
                let _ = quiet(docker(&["rename", &old, &self.container]));
                if was_running {
                    let _ = quiet(docker(&["start", &self.container]));
                }
            }
            return Err(e);
        }
        if aside {
            let _ = quiet(docker(&["rm", "-f", &old]));
        }
        self.wait_ready()
    }

    /// `up`, backed up first as an upgrade is when the container runs another version.
    fn up_checked(&self) -> anyhow::Result<()> {
        let running = running_version(self);
        if running
            .as_deref()
            .is_some_and(|v| v != ME && semver(v).is_some())
        {
            self.settle(running.as_deref())?;
        }
        self.up()
    }

    /// Stops the container, so its daemon drains (tasks, long execs), then removes it.
    fn remove(&self) {
        let _ = quiet(docker(&["stop", &self.container]));
        let _ = quiet(docker(&["rm", "-f", &self.container]));
    }

    /// Waits until the daemon answers on its socket (`zephyr status`), so "ready" means a message
    /// sent now is taken. Starting it (rpi, the providers, the desktop) takes a few seconds.
    fn wait_ready(&self) -> anyhow::Result<()> {
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
        const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let tty = std::io::stdout().is_terminal();
        let start = std::time::Instant::now();
        if !tty {
            println!("started {}; waiting for its daemon", self.container);
        }
        while start.elapsed() < LIMIT {
            let status = self
                .exec(&[], &[], &["zephyr", "status"])
                .stderr(Stdio::null())
                .output();
            if status.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("daemon: running"))
            {
                if tty {
                    print!("\r\x1b[2K");
                }
                let secs = start.elapsed().as_secs();
                println!(
                    "{} is ready ({secs}s). Logs: zephyr logs -f",
                    self.container
                );
                return Ok(());
            }
            if tty {
                let frame = FRAMES[(start.elapsed().as_millis() / 100 % 10) as usize];
                let secs = start.elapsed().as_secs();
                print!("\r{frame} starting {} ({secs}s)", self.container);
                let _ = std::io::stdout().flush();
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        if tty {
            println!();
        }
        eprintln!(
            "{}'s daemon did not answer within {}s; its last log lines:",
            self.container,
            LIMIT.as_secs()
        );
        let _ = docker(&["logs", "--tail", "20", &self.container]).status();
        bail!(
            "{} started, but its daemon is not answering",
            self.container
        )
    }

    /// Once, before the first start with a split: the daemon's files leave the agent's home for
    /// `~/.zephyr-daemon`, and that dir becomes root's. A no-op once it is root's.
    fn migrate(&self) -> anyhow::Result<()> {
        if !self.data.direct() {
            return Ok(());
        }
        let (dir, daemon) = (&self.data.dir, &self.data.daemon);
        std::fs::create_dir_all(daemon)?;
        let names = DAEMON_FILES
            .iter()
            .chain(&["state.db", "state.db-wal", "state.db-shm"]);
        for name in names {
            let (old, new) = (dir.join(name), daemon.join(name));
            if old.exists() && !new.exists() {
                std::fs::rename(&old, &new)
                    .with_context(|| format!("moving {} to {}", old.display(), new.display()))?;
            }
        }
        for stale in ["zephyr.lock", "zephyr.sock"] {
            let _ = std::fs::remove_file(dir.join(stale));
        }
        let mut doc = self.data.load()?;
        if conf::get(&doc, "state").is_none() {
            conf::set(&mut doc, "state", &format!("{DAEMON_MOUNT:?}"))?;
        }
        if conf::get(&doc, "attach.token_file").as_deref() == Some("/zephyr/tui.token") {
            conf::set(
                &mut doc,
                "attach.token_file",
                &format!("\"{DAEMON_MOUNT}/tui.token\""),
            )?;
        }
        // The host's `zephyr chat` now runs as root; this uid is the agent's.
        let uid = agent_ids().0.to_string();
        if let Some(chat) = doc["owners"]["chat"].as_array_mut() {
            chat.retain(|v| v.as_str() != Some(uid.as_str()));
            if !chat.iter().any(|v| v.as_str() == Some("0")) {
                chat.push("0");
            }
        }
        self.data.save(&doc)?;
        // The only step that needs root: chown in a throwaway container.
        let mount = format!("type=bind,src={},dst=/d", daemon.display());
        run(&mut docker(&[
            "run",
            "--rm",
            "--user",
            "0",
            "--network",
            "none",
            "--entrypoint",
            "sh",
            "--mount",
            &mount,
            &self.image,
            "-c",
            "chown -R 0:0 /d && chmod 700 /d",
        ]))
        .context("giving ~/.zephyr-daemon to root")?;
        println!(
            "moved zephyr's own files to {} (root's now)",
            daemon.display()
        );
        Ok(())
    }

    /// `zephyr tui` in the container, or with `stock` the stock `rpi --connect` client.
    fn attach(&self, stock: bool) -> anyhow::Result<()> {
        self.need_running()?;
        let token = self.data.token()?;
        // Root's own files for the client: in the daemon's dir, which the agent cannot enter (it
        // could make a dir in /tmp first). Root cannot enter the agent's home either.
        let home = format!("{DAEMON_MOUNT}/tui");
        run(&mut self.exec(&[], &[], &["mkdir", "-p", "-m", "0700", &home]))?;
        // `-e NAME` without a value: docker copies it from this process's environment, when set.
        let mut flags = vec!["-it", "-e", "RPI_SERVER_TOKEN"];
        let mut cmd = if stock {
            let doc = self.data.load()?;
            let port = conf::get(&doc, "attach.port").context("no [attach] port in zephyr.toml")?;
            // As root, so the agent's uid cannot read the token from the client's environment.
            let agent = format!("{home}/agent");
            let env = [("HOME", home.as_str()), ("RPI_CODING_AGENT_DIR", &agent)];
            let mut cmd = self.exec(&flags, &env, &["rpi", "--connect"]);
            cmd.arg(format!("127.0.0.1:{port}"));
            cmd
        } else {
            // What the TUI reads to pick its colours and glyphs.
            for var in [
                "TERM",
                "COLORTERM",
                "COLORFGBG",
                "NO_COLOR",
                "LANG",
                "LC_ALL",
                "LC_CTYPE",
            ] {
                flags.extend(["-e", var]);
            }
            // The container runs on UTC; the TUI shows times in this machine's zone.
            let tz = std::env::var("TZ")
                .ok()
                .filter(|t| !t.is_empty())
                .or_else(|| jiff::tz::TimeZone::system().iana_name().map(String::from))
                .unwrap_or_else(|| "UTC".into());
            self.exec(&flags, &[("HOME", &home), ("TZ", &tz)], &["zephyr", "tui"])
        };
        let ran = run(cmd.env("RPI_SERVER_TOKEN", &token));
        if ran.is_err() && !stock && std::io::stdout().is_terminal() {
            // A `zephyr tui` killed with its container cannot undo its terminal modes: the
            // alternate screen, mouse, bracketed paste, synchronized output, kitty keys, no wrap.
            print!("\x1b[?1000l\x1b[?1002l\x1b[?1006l\x1b[?2004l\x1b[?2026l\x1b[<u\x1b[?7h\x1b[?25h\x1b[?1049l");
            let _ = std::io::stdout().flush();
        }
        if stock {
            return ran;
        }
        ran.context("zephyr tui stopped; if the container restarted, `zephyr attach` again")
    }

    fn doctor(&self, fix: bool) -> anyhow::Result<()> {
        let mut ok = check("docker answers", quiet(docker(&["version"])));
        let dir = &self.data.dir;
        let daemon = &self.data.daemon;
        if fix {
            self.data.ensure_layout()?;
            if self.data.direct() {
                // Rewriting a file makes it 0600 and its dir 0700.
                for name in DAEMON_FILES {
                    if let Ok(text) = self.data.read(name) {
                        self.data.write(name, &text)?;
                    }
                }
            }
        }
        ok &= check(
            &format!("{} exists and is private", dir.display()),
            private(dir),
        );
        ok &= if self.data.direct() {
            // Before the first `zephyr up` with a split: the files are still this user's.
            ["secrets.env", "tui.token"].iter().fold(
                check(
                    &format!("{} is private (root's after `zephyr up`)", daemon.display()),
                    private(daemon),
                ),
                |ok, name| {
                    let path = daemon.join(name);
                    check(&format!("{} is private", path.display()), private(&path)) && ok
                },
            )
        } else {
            check(
                &format!("{} is root's and private", daemon.display()),
                std::fs::metadata(daemon).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o077 == 0),
            )
        };
        ok &= check("zephyr.toml parses", self.data.load().is_ok());
        if fix && !self.running() && self.data.projects()?.is_some() {
            self.ensure_image(false, false)?;
            self.up_checked()?;
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

    /// `zephyr upgrade [VERSION]`: the newest release, or VERSION. This CLI is replaced first, so
    /// the CLI that starts the container always matches its image.
    fn upgrade(mut self, version: Option<&str>) -> anyhow::Result<()> {
        if std::env::var("ZEPHYR_IMAGE").is_ok_and(|v| !v.is_empty()) {
            // A named image (a local build, a test install): no versions to compare.
            self.ensure_image(false, true)?;
            self.settle(running_version(&self).as_deref())?;
            return self.up();
        }
        let target = match version {
            Some(v) if semver(v).is_some() => v.to_string(),
            Some(v) => bail!("{v} is not a version (x.y.z)"),
            None => latest()?,
        };
        let running = running_version(&self);
        if target == ME && self.running() && running.as_deref() == Some(target.as_str()) {
            println!("already on {target}");
            return Ok(());
        }
        self.image = format!("{REPO}{target}");
        self.data.image = self.image.clone();
        // A release is its published image, never a local build of this tree under its tag.
        if !quiet(docker(&["image", "inspect", &self.image])) && !pull(&self.image) {
            bail!("could not pull {}", self.image);
        }
        // While the old container still runs: the owner's packages on the new image.
        self.derive(&self.data.packages()?, false)?;
        // Without a container (after `zephyr down`), state.db is most likely this CLI's version's.
        let back = matches!(
            (semver(running.as_deref().unwrap_or(ME)), semver(&target)),
            (Some(r), Some(t)) if t < r
        );
        let bak = format!("state.db.{target}.bak");
        // Touched as well, so `settle`'s pruning (oldest first) keeps the backup this restores.
        let exists = format!(
            "import os, sys; p = '{DAEMON_MOUNT}/{bak}'; os.path.exists(p) or sys.exit(1); os.utime(p)"
        );
        if back && !quiet(self.daemon_py(&exists)) {
            bail!("no backup from {target} ({bak}); its schema may not read this state.db");
        }
        // Fetched and checked before anything changes.
        let cli = (target != ME)
            .then(|| fetch_cli(&self.image, &target))
            .transpose()?;
        // A newer CLI waits and backs up itself; an older one may not know how to go back.
        if cli.is_none() || back {
            self.settle(running.as_deref())?;
        }
        if back {
            // Removed, not only stopped: an older CLI run next must not name the restored
            // state.db's backup after the newer container.
            self.remove();
            // The WAL belongs to the newer state.db, which is backed up now. The config goes back
            // too: the older daemon refuses keys it does not know.
            let restore = format!(
                "import os, shutil; \
                 d = '{DAEMON_MOUNT}/'; \
                 [os.remove(d + f) for f in ('state.db-wal', 'state.db-shm') if os.path.exists(d + f)]; \
                 shutil.copy(d + '{bak}', d + 'state.db'); \
                 [shutil.copy(d + f + '.{target}.bak', d + f) for f in {ROLLED_BACK:?} \
                 if os.path.exists(d + f + '.{target}.bak')]"
            );
            run(&mut self.daemon_py(&restore)).context("restoring state.db")?;
            eprintln!(
                "warning: state.db and the config are back to {target}'s backups; \
                 changes since then are lost"
            );
        }
        let Some((new, exe)) = cli else {
            return self.up();
        };
        std::fs::rename(&new, &exe).with_context(|| format!("replacing {}", exe.display()))?;
        println!("zephyr CLI: {ME} -> {target}");
        let err = Command::new(&exe).args(["upgrade", &target]).exec();
        Err(err).with_context(|| format!("running {}", exe.display()))
    }

    /// Waits until no message, job, task or process is in flight (30 minutes at most), then
    /// backs up state.db as `state.db.<running>.bak` (migrations only go forward), and the
    /// `ROLLED_BACK` config files beside it as `<file>.<running>.bak`. The newest 5 versions'
    /// backups are kept. A failed backup stops the upgrade; only a first install, without
    /// state.db, goes on without one.
    fn settle(&self, running: Option<&str>) -> anyhow::Result<()> {
        const LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
        let start = std::time::Instant::now();
        while self.running() {
            let status = self
                .exec(&[], &[], &["zephyr", "status"])
                .stderr(Stdio::null())
                .output();
            if !status.is_ok_and(|o| busy(&String::from_utf8_lossy(&o.stdout))) {
                break;
            }
            if start.elapsed() >= LIMIT {
                bail!(
                    "the agent has been busy for 30 minutes (`zephyr status`); \
                     send it /stop, then run this again"
                );
            }
            println!("the agent is busy; waiting...");
            std::thread::sleep(std::time::Duration::from_secs(10));
        }
        if !self.data.daemon.exists() {
            return Ok(());
        }
        // Without a container to name it (after `zephyr down`), a name going back never picks.
        let suffix = match running.filter(|v| semver(v).is_some()) {
            Some(v) => format!(".{v}.bak"),
            None => ".bak".into(),
        };
        let name = format!("state.db{suffix}");
        // `b[len(d) + 8:]` is a backup's `.<version>.bak`, after `state.db`.
        let script = format!(
            "import glob, os, shutil, sqlite3, sys; \
             d = '{DAEMON_MOUNT}/'; \
             os.path.exists(d + 'state.db') or sys.exit(3); \
             bak = sqlite3.connect('{DAEMON_MOUNT}/{name}'); \
             sqlite3.connect(d + 'state.db').backup(bak); \
             bak.close(); \
             [shutil.copy(d + f, d + f + '{suffix}') for f in {ROLLED_BACK:?} if os.path.exists(d + f)]; \
             old = sorted(glob.glob(d + 'state.db.*.bak'), key=os.path.getmtime)[:-5]; \
             [os.remove(p) for b in old for p in [d + f + b[len(d) + 8:] for f in ('state.db', *{ROLLED_BACK:?})] \
             if os.path.exists(p)]"
        );
        match self.daemon_py(&script).status()?.code() {
            Some(0) => println!(
                "state.db backed up to {}",
                self.data.daemon.join(&name).display()
            ),
            Some(3) => {}
            _ => bail!("backing up state.db failed; not upgrading"),
        }
        Ok(())
    }

    /// python3 from the image, as root, on the daemon's dir, which this user cannot read.
    fn daemon_py(&self, script: &str) -> Command {
        let daemon = self.data.daemon.display();
        let mount = format!("type=bind,src={daemon},dst={DAEMON_MOUNT}");
        docker(&[
            "run",
            "--rm",
            "--user",
            "0",
            "--network",
            "none",
            "--entrypoint",
            "python3",
            "--mount",
            &mount,
            &self.image,
            "-c",
            script,
        ])
    }
}

/// The newest release's version, as `latest` itself reports it.
fn latest() -> anyhow::Result<String> {
    let image = format!("{REPO}latest");
    if !pull(&image) {
        bail!("could not pull {image}: `docker login ghcr.io`, or name the version (zephyr upgrade X.Y.Z)");
    }
    let out = output(&mut docker(&[
        "run",
        "--rm",
        "--entrypoint",
        "zephyr",
        &image,
        "--version",
    ]))?;
    let version = out.trim().trim_start_matches("zephyr ");
    semver(version).with_context(|| format!("{image} reports {out:?}, not a version"))?;
    Ok(version.to_string())
}

/// `x.y.z` as numbers, so 0.10.0 comes after 0.9.0.
fn semver(v: &str) -> Option<(u64, u64, u64)> {
    let n = v
        .split('.')
        .map(|p| {
            p.parse()
                .ok()
                .filter(|_| p.bytes().all(|b| b.is_ascii_digit()))
        })
        .collect::<Option<Vec<u64>>>()?;
    match n[..] {
        [a, b, c] => Some((a, b, c)),
        _ => None,
    }
}

/// `zephyr status` shows work in flight: a message the agent is on, a job run, a task or a
/// process. Rows that wait on someone else never clear by themselves, so they do not count: a
/// notice for a TUI nobody attaches, a reply for a removed channel, messages while paused.
fn busy(status: &str) -> bool {
    status.lines().any(|l| {
        let mut w = l.split_whitespace();
        matches!(
            (w.next(), w.next()),
            (Some("inbox"), Some("dispatched"))
                | (Some("runs"), Some("claimed" | "running"))
                | (Some("tasks" | "procs"), Some("running"))
        )
    })
}

/// The CLI in `image`, written beside this one and checked to be `target`: (new file, this CLI).
fn fetch_cli(image: &str, target: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    let exe = std::env::current_exe()?;
    let new = exe.with_file_name(format!(".zephyr-{target}.new"));
    let file = std::fs::File::create(&new).with_context(|| {
        format!(
            "cannot write beside {0}; install the CLI by hand: \
             docker run --rm --entrypoint cat {image} /usr/local/bin/zephyr > {0} && chmod 755 {0}",
            exe.display()
        )
    })?;
    let checked = (|| {
        run(docker(&[
            "run",
            "--rm",
            "--entrypoint",
            "cat",
            image,
            "/usr/local/bin/zephyr",
        ])
        .stdout(file))?;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755))?;
        let out = output(Command::new(&new).arg("--version"))?;
        let said = out.lines().next().unwrap_or_default();
        if said != format!("zephyr {target}") {
            bail!("the CLI in {image} says {said:?}, not zephyr {target}");
        }
        Ok(())
    })();
    if let Err(e) = checked {
        let _ = std::fs::remove_file(&new);
        return Err(e);
    }
    Ok((new, exe))
}

/// The container's state (`running`, `exited`, ...), or None when it does not exist.
pub(crate) fn state(container: &str) -> Option<String> {
    let out = docker(&["inspect", "-f", "{{.State.Status}}", container])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// This user's uid and gid: the agent's in the container.
fn agent_ids() -> (u32, u32) {
    // SAFETY: getuid and getgid have no arguments and cannot fail.
    unsafe { (libc::getuid(), libc::getgid()) }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn docker(args: &[&str]) -> Command {
    let mut cmd = Command::new("docker");
    cmd.args(args);
    cmd
}

/// Whether Docker has the NVIDIA runtime (nvidia-container-toolkit), so `--gpus` works.
fn nvidia_runtime() -> bool {
    docker(&[
        "info",
        "--format",
        "{{range $k, $v := .Runtimes}}{{$k}} {{end}}",
    ])
    .stderr(Stdio::null())
    .output()
    .is_ok_and(|o| {
        String::from_utf8_lossy(&o.stdout)
            .split_whitespace()
            .any(|r| r == "nvidia")
    })
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

/// `docker pull`, logging in to the image's registry with `gh` and trying again when it fails.
fn pull(image: &str) -> bool {
    // Docker draws its own progress; the image is several GB the first time.
    println!("pulling {image}");
    if shown(docker(&["pull", image])) {
        return true;
    }
    if let (Some(token), Some(user)) = (
        gh(&["auth", "token"]),
        gh(&["api", "user", "--jq", ".login"]),
    ) {
        let registry = image.split('/').next().unwrap_or("ghcr.io");
        println!("logging in to {registry} as {user} (gh) and pulling again");
        let mut login = docker(&["login", registry, "-u", &user, "--password-stdin"]);
        return feed(&mut login, &token).is_ok() && shown(docker(&["pull", image]));
    }
    false
}

/// True when the command succeeds; its output goes to the terminal (docker's progress bars).
fn shown(mut cmd: Command) -> bool {
    cmd.status().is_ok_and(|s| s.success())
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
    // find exits 1 on any dir it cannot read (a database's 0700 data dir); what it found counts.
    let found = Command::new("find")
        .args([projects, "-name", "node_modules", "-prune", "-o"])
        .args(["-name", ".git", "-prune", "-o", "-name", ".env*", "-print0"])
        .stderr(Stdio::null())
        .output()
        .context("running find")?;
    paths.extend(
        String::from_utf8_lossy(&found.stdout)
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(String::from),
    );
    let mut args = vec![];
    for p in paths {
        if p.contains(',') {
            eprintln!(
                "warning: {p} stays visible to the agent: --mount cannot name a path with a comma"
            );
            continue;
        }
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

fn private(path: &Path) -> bool {
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
            h.up_checked()
        }
        "upgrade" => match &a.rest()?[..] {
            [] => h.upgrade(None),
            [v] => h.upgrade(Some(v)),
            _ => bail!("{USAGE}"),
        },
        "down" => {
            a.rest()?;
            run(&mut docker(&["stop", &h.container]))?;
            run(&mut docker(&["rm", &h.container]))
        }
        "restart" => {
            a.rest()?;
            run(docker(&["restart", &h.container]).stdout(Stdio::null()))?;
            h.wait_ready()
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
        // On a terminal, `chat` is the TUI; pipes, scripts and a config without the attach port
        // keep the line client.
        "chat"
            if h.interactive
                && h.data
                    .load()
                    .is_ok_and(|d| conf::get(&d, "attach.port").is_some()) =>
        {
            a.rest()?;
            h.attach(false)
        }
        "chat" | "pause" | "resume" => {
            a.rest()?;
            h.need_running()?;
            let flags: &[&str] = match (cmd, h.interactive) {
                ("chat", true) => &["-it"],
                ("chat", false) => &["-i"],
                _ => &[],
            };
            run(&mut h.exec(flags, &[], &["zephyr", cmd]))
        }
        "attach" => {
            let stock = a.flag("--stock");
            a.rest()?;
            h.attach(stock)
        }
        "shell" => match &a.rest()?[..] {
            [] => h.shell("bash"),
            [s] if s == "bash" || s == "zsh" => h.shell(s),
            _ => bail!("{USAGE}"),
        },
        "config" => {
            let rest = a.rest()?;
            let args = rest.iter().map(String::as_str).collect::<Vec<_>>();
            // A registered setting goes through the daemon's checks (validation, the change log,
            // time limits) while it runs; its refusal is this command's failure.
            if let ["set", key, value] = args[..] {
                if crate::settings::find(key).is_some() && h.running() {
                    return run(&mut h.exec(&[], &[], &["zephyr", "set", key, value]));
                }
            }
            let mut doc = h.data.load()?;
            match args[..] {
                ["get", key] => {
                    println!(
                        "{}",
                        conf::get(&doc, key).with_context(|| format!("{key} is not set"))?
                    );
                }
                ["set", key, value] => {
                    if crate::settings::find(key).is_none() {
                        eprintln!("warn: {key} is not a known setting; saved without checking");
                    }
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
            let dir = format!("{} and {}", h.data.dir.display(), h.data.daemon.display());
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
            if !keep && !h.data.direct() {
                // The daemon's files are root's: give them back while the image is still here.
                let ids = agent_ids();
                let mount = format!("type=bind,src={},dst=/d", h.data.daemon.display());
                let chown = format!("chown -R {}:{} /d", ids.0, ids.1);
                run(&mut docker(&[
                    "run",
                    "--rm",
                    "--user",
                    "0",
                    "--network",
                    "none",
                    "--entrypoint",
                    "sh",
                    "--mount",
                    &mount,
                    &h.image,
                    "-c",
                    &chown,
                ]))
                .with_context(|| {
                    format!(
                        "taking {} back from root failed; remove it with sudo rm -rf",
                        h.data.daemon.display()
                    )
                })?;
            }
            // Local package images first: each is built on a release image.
            let locals = docker(&["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"])
                .arg(LOCAL.trim_end_matches(':'))
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            for image in locals.lines().chain([h.image.as_str()]) {
                let _ = quiet(docker(&["rmi", image]));
            }
            if !keep {
                for d in [&h.data.dir, &h.data.daemon] {
                    std::fs::remove_dir_all(d).or_else(|e| {
                        if e.kind() == std::io::ErrorKind::NotFound {
                            Ok(())
                        } else {
                            Err(e)
                        }
                    })?;
                }
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
        "skill" => setup::skill(&h, a),
        "mcp" => setup::mcp(&h, a),
        "packages" => setup::packages(&h, a),
        "vault" => setup::vault(&h, a),
        "subagent" => setup::subagent(&h, a),
        "alert" => alert::main(&h, a),
        _ => bail!("{USAGE}"),
    }
}

/// The container's memory cap when host.toml sets none: 75% of the host's RAM, so a runaway
/// agent cannot push the host into swap or its OOM killer. None when `/proc/meminfo` is unreadable.
fn default_memory() -> Option<String> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = info
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))?
        .trim()
        .strip_suffix("kB")?
        .trim()
        .parse()
        .ok()?;
    Some(format!("{}k", kb / 4 * 3))
}

/// `zephyr-local:<version>-<hash>` for apt `packages` on `base`, None without packages. The hash
/// covers the base too, so two named images (`ZEPHYR_IMAGE`) never share a local image.
fn local_image(base: &str, packages: &[String]) -> Option<String> {
    if packages.is_empty() {
        return None;
    }
    let mut sorted = packages.to_vec();
    sorted.sort();
    let text = format!("{base}\n{}", sorted.join("\n"));
    let digest = ring::digest::digest(&ring::digest::SHA256, text.as_bytes());
    let hash: String = digest.as_ref()[..4]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let version = base.strip_prefix(REPO).unwrap_or("custom");
    Some(format!("{LOCAL}{version}-{hash}"))
}

/// A Debian package name (`^[a-z0-9][a-z0-9+.-]+$`), checked before it reaches a shell. A
/// trailing `-` is refused too: apt-get install reads `name-` as "remove name".
fn package_name(name: &str) -> bool {
    name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && !name.ends_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"+.-".contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_reads_zephyr_status() {
        let idle = "daemon: running\ninbox   done        12\noutbox  sent        3\nruns    ok          2\n";
        assert!(!busy(idle));
        for line in [
            "inbox   dispatched  1",
            "runs    claimed     1",
            "runs    running     1",
            "tasks   running     1",
            "procs   running     2",
        ] {
            assert!(busy(&format!("{idle}{line}\n")), "{line}");
        }
        // Rows that wait on someone else would hold an upgrade forever.
        for line in [
            "inbox   new         1",
            "outbox  pending     2",
            "tasks   queued      1",
            "procs   exited      3",
        ] {
            assert!(!busy(&format!("{idle}{line}\n")), "{line}");
        }
    }

    #[test]
    fn package_names_are_checked_and_name_a_stable_local_image() {
        for good in ["ffmpeg", "g++", "libssl3t64", "python3.12-venv", "0ad"] {
            assert!(package_name(good), "{good}");
        }
        for bad in [
            "x", "Ffmpeg", "-y", "git-", "a b", "a;rm", "$(id)", "pkg=1.0", "",
        ] {
            assert!(!package_name(bad), "{bad}");
        }
        let list = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let base = format!("{REPO}0.4.0");
        assert_eq!(local_image(&base, &[]), None);
        let tag = local_image(&base, &list(&["jq", "ffmpeg"])).unwrap();
        // Pinned: sha256("ghcr.io/yansigit/zephyr:0.4.0\nffmpeg\njq"), so it holds across builds.
        assert_eq!(tag, "zephyr-local:0.4.0-90531f12");
        assert_eq!(
            local_image(&base, &list(&["ffmpeg", "jq"])),
            Some(tag.clone())
        );
        let other = local_image("zephyr:dev", &list(&["ffmpeg", "jq"])).unwrap();
        assert!(other.starts_with("zephyr-local:custom-") && other != tag);
    }

    #[test]
    fn versions_compare_as_numbers() {
        assert!(semver("0.10.0") > semver("0.9.3"));
        for bad in ["v1.2.3", "1.2", "1.2.3.4", "1.+2.3", "latest", ""] {
            assert_eq!(semver(bad), None, "{bad}");
        }
    }
}
