use serde_json::json;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use zephyr::channel::AnyChannel;
use zephyr::chat::{self, ChatChannel};
use zephyr::config::Config;
use zephyr::discord::Discord;
use zephyr::rpc::{Child, USED_COMMANDS};
use zephyr::store::Store;

/// The rpi minor this build speaks (DESIGN §3).
const RPI_PIN: &str = "rpi 0.21.";

const USAGE: &str = "usage: zephyr <run|supervise|status|doctor|chat|tui|pause|resume|reload|skill>  (config: $ZEPHYR_CONFIG or ~/agent/zephyr.toml)";

fn main() -> ExitCode {
    // `agent-browser` on the agent's PATH is this binary (approvals spec §4.4): the wrapper runs
    // before anything else, with no runtime and no config (the agent cannot read the daemon's).
    let mut argv = std::env::args_os();
    let zero = argv.next().unwrap_or_default();
    if Path::new(&zero).file_name() == Some(std::ffi::OsStr::new("agent-browser")) {
        let args = argv.map(|a| a.to_string_lossy().into_owned()).collect();
        return zephyr::browser_gate::main(args);
    }
    zephyr_main()
}

#[tokio::main]
async fn zephyr_main() -> ExitCode {
    let first = std::env::args().nth(1).unwrap_or_default();
    if first == "--version" || first == "-V" {
        println!("zephyr {}", env!("CARGO_PKG_VERSION"));
        if !zephyr::host::in_container() {
            zephyr::host::container_version();
        }
        return ExitCode::SUCCESS;
    }
    if let Err(e) = zephyr::agent::init_ids() {
        eprintln!("zephyr: {e:#}");
        return ExitCode::FAILURE;
    }
    let mut args = std::env::args().skip(1);
    let cmd = args.next().unwrap_or_default();
    let host = !zephyr::host::in_container();
    if host && zephyr::host::is_command(&cmd) {
        return finish(zephyr::host::main(&cmd, args.collect()));
    }
    if host && matches!(cmd.as_str(), "-h" | "--help" | "help") {
        println!("{}", zephyr::host::USAGE);
        return ExitCode::SUCCESS;
    }
    if host && cmd != "run" {
        eprintln!("{}", zephyr::host::USAGE);
        return ExitCode::from(2);
    }
    if cmd == "__file" {
        return file_cmd(&args.collect::<Vec<_>>());
    }
    // Run as the agent, which cannot read the config: what its uid can reach (DESIGN §4.19).
    if cmd == "__netcheck" {
        return zephyr::egress::netcheck(&args.collect::<Vec<_>>());
    }
    // Before the config: it only starts `zephyr run`, which reads it (and secrets.env) itself.
    if cmd == "supervise" {
        return zephyr::supervise::main().await;
    }
    // The daemon and the commands run beside it read secrets.env themselves (no --env-file).
    let mut from_file = Vec::new();
    let cfg = Config::load(&Config::default_path()).and_then(|cfg| {
        from_file = zephyr::dotenv::load(&cfg.secrets_path())?;
        zephyr::agent::init(&from_file, &cfg.agent.env);
        Ok(cfg)
    });
    let result = match cfg {
        Err(e) if cmd != "-h" && cmd != "--help" => Err(e),
        cfg => match (cmd.as_str(), cfg) {
            ("run", Ok(cfg)) => run(cfg, &from_file).await,
            ("status", Ok(cfg)) => status(&cfg).await,
            ("doctor", Ok(cfg)) => doctor(&cfg).await,
            ("chat", Ok(cfg)) => chat::client(&cfg.socket_path()).await,
            ("tui", Ok(cfg)) => zephyr::tui::run(&cfg).await,
            ("pause", Ok(cfg)) => control(&cfg, "/pause").await,
            ("resume", Ok(cfg)) => control(&cfg, "/resume").await,
            ("reload", Ok(cfg)) => control(&cfg, "/reload").await,
            ("set", Ok(cfg)) => set(&cfg, &args.collect::<Vec<_>>()).await,
            ("skill", Ok(cfg)) => skill(&cfg, &args.collect::<Vec<_>>()).await,
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        },
    };
    finish(result)
}

/// `zephyr __file get|put NAME`: how the host CLI reads and writes the daemon's files once they
/// belong to root (`host::conf::Data`). `put` takes the text on stdin and replaces the file
/// whole, mode 0600. `get` of a missing file prints nothing and exits 3.
fn file_cmd(args: &[String]) -> ExitCode {
    let path = Config::default_path();
    let dir = path.parent().unwrap_or(Path::new("/"));
    let result = match args {
        [op, name] if zephyr::host::conf::DAEMON_FILES.contains(&name.as_str()) => {
            let file = dir.join(name);
            match op.as_str() {
                "get" => match std::fs::read(&file) {
                    Err(e) if e.kind() == ErrorKind::NotFound => return ExitCode::from(3),
                    r => r
                        .and_then(|text| std::io::Write::write_all(&mut std::io::stdout(), &text))
                        .map_err(Into::into),
                },
                "put" => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::stdin(), &mut text)
                        .map_err(anyhow::Error::from)
                        .and_then(|_| {
                            // A newer host CLI may write keys this daemon refuses; it would then
                            // crash-loop at its next start.
                            if name == "zephyr.toml" {
                                toml::from_str::<Config>(&text)
                                    .map_err(anyhow::Error::from)
                                    .and_then(|c| c.validate())
                                    .map_err(|e| e.context("zephyr.toml not replaced"))?;
                            }
                            zephyr::host::conf::replace_private(&file, &text)
                        })
                }
                _ => Err(anyhow::anyhow!("usage: zephyr __file get|put NAME")),
            }
        }
        _ => Err(anyhow::anyhow!("usage: zephyr __file get|put NAME")),
    };
    finish(result)
}

/// `zephyr set`'s exit code when no daemon listens (1 is a refusal): the host CLI then edits the
/// file itself.
const NOT_RUNNING: u8 = 3;

#[derive(Debug)]
struct NotRunning;

impl std::fmt::Display for NotRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the daemon is not running; edit zephyr.toml or `zephyr up` first")
    }
}

impl std::error::Error for NotRunning {}

fn finish(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zephyr: {e:#}");
            if e.downcast_ref::<NotRunning>().is_some() {
                return ExitCode::from(NOT_RUNNING);
            }
            ExitCode::FAILURE
        }
    }
}

/// Holds `<home>/zephyr.lock` for the life of `zephyr run`, so a second daemon on the same home fails.
fn lock_home(home: &Path) -> anyhow::Result<std::fs::File> {
    std::fs::create_dir_all(home)?;
    let path = home.join("zephyr.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)?;
    // SAFETY: flock on a descriptor we own; the kernel releases it when the process exits.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() == ErrorKind::WouldBlock {
            anyhow::bail!(
                "another zephyr is running on {} (lock {} is held)",
                home.display(),
                path.display()
            );
        }
        return Err(err.into());
    }
    Ok(file)
}

/// `from_file`: the variables secrets.env set, dropped before a re-exec so it reads them anew.
async fn run(cfg: Config, from_file: &[String]) -> anyhow::Result<()> {
    zephyr::agent::undumpable();
    // SAFETY: getuid has no arguments and cannot fail.
    if unsafe { libc::getuid() } == 0 && zephyr::agent::ids().is_none() {
        anyhow::bail!(
            "refusing to run as root without ZEPHYR_AGENT_UID and ZEPHYR_AGENT_GID: the agent would run as root"
        );
    }
    if zephyr::egress::bridge() {
        // Before anything is started for the agent: without the floor it does not run.
        zephyr::egress::verify().await?;
    }
    let _lock = lock_home(cfg.state_dir())?;
    let mut channels = vec![AnyChannel::Chat(ChatChannel::new(cfg.socket_path()))];
    if let Some(discord) = &cfg.discord {
        channels.push(AnyChannel::Discord(Discord::new(discord)?));
    }
    if let Some(imessage) = &cfg.imessage {
        channels.push(AnyChannel::IMessage(zephyr::imessage::IMessage::new(
            imessage,
        )?));
    }
    let shutdown = async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
        }
        eprintln!("zephyr: shutting down");
    };
    let exit = zephyr::router::run(&cfg, &channels, shutdown).await?;
    drop(channels);
    if exit == zephyr::router::Exit::Reexec {
        use std::os::unix::process::CommandExt;
        // The running binary itself, even if its file was replaced since (a rebuild, a reinstall):
        // `current_exe()` would then name a deleted path and the exec fail with ENOENT.
        let mut cmd = std::process::Command::new("/proc/self/exe");
        cmd.args(std::env::args_os().skip(1));
        for key in from_file {
            cmd.env_remove(key);
        }
        // Only returns on failure. The lock and every socket are close-on-exec.
        return Err(cmd.exec().into());
    }
    Ok(())
}

/// The running daemon's reply to `text`, sent as the owner over its chat socket. `None` when no
/// daemon listens.
async fn ask(cfg: &Config, text: &str) -> anyhow::Result<Option<String>> {
    let Ok(stream) = tokio::net::UnixStream::connect(cfg.socket_path()).await else {
        return Ok(None);
    };
    let (read, mut write) = stream.into_split();
    let id = format!("cli-{}-{}", std::process::id(), zephyr::unix_millis());
    let mut line = serde_json::to_vec(&json!({"id": id, "text": text}))?;
    line.push(b'\n');
    write.write_all(&line).await?;
    let mut lines = BufReader::new(read).lines();
    // The reply comes back on this connection; no reply means this uid is not an owner. Chat
    // writes every pending row for this uid here too: only the line whose `re` is our id is
    // the reply (a daemon before `re` sends none, and its first line is taken).
    let reply = async {
        while let Some(line) = lines.next_line().await? {
            let v: serde_json::Value = serde_json::from_str(&line)?;
            if v.get("re").is_none_or(|re| *re == id) {
                return Ok(Some(v));
            }
        }
        anyhow::Ok(None)
    };
    let v = tokio::time::timeout(Duration::from_secs(10), reply)
        .await
        .map_err(|_| {
            anyhow::anyhow!("no reply from zephyr within 10s; is this user in owners.chat?")
        })??;
    Ok(Some(
        v.as_ref()
            .and_then(|v| v["text"].as_str())
            .unwrap_or_default()
            .to_string(),
    ))
}

/// `zephyr skill list | rm NAME | add NAME` in the container, run by the host's `zephyr skill`
/// (DESIGN §4.6). `add` takes the skill folder as a tar on stdin and unpacks it into the daemon's
/// `<state>/incoming/NAME`, out of the agent's reach, for `/skill add` to check and approve.
async fn skill(cfg: &Config, args: &[String]) -> anyhow::Result<()> {
    let text = match args {
        [list] if list == "list" => "/skills".to_string(),
        [rm, name] if rm == "rm" => format!("/skill rm {name}"),
        [add, name] if add == "add" && zephyr::drafts::valid_name(name) => {
            let dir = zephyr::drafts::incoming(cfg.state_dir(), name);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir)?;
            // By full path: root runs it, and no PATH entry may choose which tar.
            let status = std::process::Command::new("/usr/bin/tar")
                .args(["-x", "--no-same-owner", "--no-same-permissions", "-C"])
                .arg(&dir)
                .status()?;
            if !status.success() {
                anyhow::bail!("unpacking the skill failed ({status})");
            }
            format!("/skill add {name}")
        }
        _ => anyhow::bail!("usage: zephyr skill list | rm NAME | add NAME (a tar on stdin)"),
    };
    let reply = match ask(cfg, &text).await? {
        Some(reply) => reply,
        // No daemon: the next child start reads `<state>/skills` anyway.
        None => zephyr::drafts::command(cfg, &text).map_or_else(String::new, |(r, _)| r),
    };
    println!("{reply}");
    Ok(())
}

/// `zephyr pause` / `resume`: through the running daemon's chat socket, so a run in progress is
/// aborted. With no daemon, the store carries the flag to the next start.
async fn control(cfg: &Config, text: &str) -> anyhow::Result<()> {
    if let Some(reply) = ask(cfg, text).await? {
        println!("{reply}");
    } else {
        let store = Store::open(&cfg.db_path())?;
        store.set_paused((text == "/pause").then_some("user"))?;
        println!("{text}: the daemon is not running; the change applies at its next start");
    }
    Ok(())
}

/// `zephyr set KEY VALUE [for D]`: the owner's `/set` over the chat socket (a strong channel).
/// Prints the daemon's reply; a refusal exits 1, no daemon 3.
async fn set(cfg: &Config, args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(args.len() >= 2, "usage: zephyr set KEY VALUE [for D]");
    let Some(reply) = ask(cfg, &format!("/set {}", args.join(" "))).await? else {
        return Err(NotRunning.into());
    };
    println!("{reply}");
    anyhow::ensure!(zephyr::settings::is_change(&reply, &args[0]), "not changed");
    Ok(())
}

/// Fails when the daemon does not answer or its heartbeat is stale: the image's HEALTHCHECK.
async fn status(cfg: &Config) -> anyhow::Result<()> {
    use zephyr::supervise::{heartbeat_age, HEARTBEAT, STALE};
    let up = tokio::net::UnixStream::connect(cfg.socket_path())
        .await
        .is_ok();
    let beat = heartbeat_age(&cfg.state_dir().join(HEARTBEAT));
    println!("version: {}", env!("CARGO_PKG_VERSION"));
    println!("daemon: {}", if up { "running" } else { "not reachable" });
    println!(
        "heartbeat: {}",
        beat.map_or("none".into(), |s| format!("{s}s ago"))
    );
    let store = Store::open(&cfg.db_path())?;
    for line in zephyr::reload::status_lines(cfg, &store)? {
        println!("{line}");
    }
    // The router's view, written with the heartbeat: `<unix> <main up> <failed starts>`.
    let file = std::fs::read_to_string(cfg.state_dir().join(HEARTBEAT)).unwrap_or_default();
    match file.split_whitespace().collect::<Vec<_>>()[..] {
        [_, up, failures] => println!(
            "main: {}, {failures} failed starts in a row",
            if up == "1" { "up" } else { "down" }
        ),
        _ => println!("main: unknown"),
    }
    let ago = |at: Option<i64>| at.map_or("none".into(), |t| format!("{}s ago", zephyr::now() - t));
    println!("oldest new message: {}", ago(store.oldest("inbox", "new")?));
    let counts = store.counts()?;
    let outbox = |status: &str| {
        (counts.iter())
            .find(|(t, s, _)| t == "outbox" && s == status)
            .map_or(0, |c| c.2)
    };
    println!(
        "replies: {} pending (oldest {}), {} failed",
        outbox("pending"),
        ago(store.oldest("outbox", "pending")?),
        outbox("failed")
    );
    for (table, status, n) in counts {
        println!("{table:<7} {status:<11} {n}");
    }
    if !up {
        anyhow::bail!("the daemon is not running");
    }
    if beat.is_none_or(|s| s >= STALE.as_secs()) {
        anyhow::bail!("the daemon is hung: no heartbeat for {}s", STALE.as_secs());
    }
    Ok(())
}

/// Whether this process holds CAP_DAC_OVERRIDE, CAP_DAC_READ_SEARCH or CAP_FOWNER (bits 1-3 of
/// `CapEff`), any of which would let the daemon into the agent's files and the agent's uid into ours.
fn overrides_files() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        .is_none_or(|caps| caps & 0b1110 != 0)
}

async fn doctor(cfg: &Config) -> anyhow::Result<()> {
    let out = zephyr::agent::cmd(&mut std::process::Command::new(&cfg.rpi))
        .arg("--version")
        .output()?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let version_ok = version.starts_with(RPI_PIN);
    println!(
        "{} rpi version: {version} (want {RPI_PIN}x)",
        mark(version_ok)
    );

    // In the container: root holding no file overrides, and the agent on its own uid (DESIGN §4.4).
    // SAFETY: getuid has no arguments and cannot fail.
    let split_ok = unsafe { libc::getuid() } != 0 || {
        let ok = zephyr::agent::ids().is_some() && !overrides_files();
        let who = zephyr::agent::ids().map_or("no uid".into(), |(u, g)| format!("{u}:{g}"));
        println!(
            "{} the agent runs as {who} and the daemon cannot override file permissions",
            mark(ok)
        );
        // A program the agent can plant on root's PATH runs as root (`agent::AGENT_PATH`).
        let planted = zephyr::agent::ids().map_or(vec![], |(u, g)| {
            zephyr::agent::writable_dirs(&std::env::var("PATH").unwrap_or_default(), u, g)
        });
        let label = match planted.is_empty() {
            true => "no directory on root's PATH is the agent's to write".to_string(),
            false => format!(
                "root's PATH has the agent's directories: {}",
                planted.join(", ")
            ),
        };
        ok & check(&label, planted.is_empty())
    };
    // In the container: the approvals gate's browser wrapper as the image lays it out (§4.4).
    // SAFETY: getuid has no arguments and cannot fail.
    let wrapper_ok = unsafe { libc::getuid() } != 0 || browser_wrapper();
    // In the container: what root starts or trusts is root's, never the agent's to swap (B5).
    // SAFETY: getuid has no arguments and cannot fail.
    let binaries_ok = unsafe { libc::getuid() } != 0 || roots_binaries(cfg);

    // A throwaway home so doctor never touches the live `main` session.
    let mut probe = cfg.clone();
    probe.home = std::env::temp_dir().join(format!("zephyr-doctor-{}", std::process::id()));
    // In pool mode, probe as the first account: the top-level model and agent_dir are unused.
    if let Some(first) = cfg.accounts.first() {
        probe.agent_dir = Some(cfg.account_dir(first));
        probe.model = Some(first.model.clone());
        probe.accounts.clear();
    }
    {
        let _fs = zephyr::agent::fs();
        std::fs::create_dir_all(&probe.home)?;
    }
    let child = Child::spawn(&probe, "doctor").await;
    let commands_ok = match child {
        Ok(child) => {
            let supported = child.ready["supportedCommands"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let missing: Vec<_> = USED_COMMANDS
                .iter()
                .filter(|c| !supported.iter().any(|s| s == *c))
                .collect();
            let settles = child.ready["semantics"]["settlementEvent"] == "agent_settled";
            println!(
                "{} ready.supportedCommands covers {USED_COMMANDS:?}{}",
                mark(missing.is_empty()),
                if missing.is_empty() {
                    String::new()
                } else {
                    format!(" (missing {missing:?})")
                }
            );
            println!("{} settlement event is agent_settled", mark(settles));
            child.stop().await;
            missing.is_empty() && settles
        }
        Err(e) => {
            println!("{} rpi --mode rpc did not start: {e:#}", mark(false));
            false
        }
    };
    {
        let _fs = zephyr::agent::fs();
        let _ = std::fs::remove_dir_all(&probe.home);
    }

    // Names and presence only: a secret's value is never printed.
    let mut ok = version_ok && split_ok && wrapper_ok && binaries_ok && commands_ok;
    // While the daemon is up, its gate socket is there: without it every send is refused
    // (approvals spec §4.9).
    if tokio::net::UnixStream::connect(cfg.socket_path())
        .await
        .is_ok()
    {
        use std::os::unix::fs::FileTypeExt;
        let socket = zephyr::gate::socket_path(cfg);
        let there = std::fs::symlink_metadata(&socket).is_ok_and(|m| m.file_type().is_socket());
        let label = format!("the approvals gate listens at {}", socket.display());
        ok &= check(&label, there);
    }
    if let Some(d) = &cfg.discord {
        let label = format!("env {} is set (discord token)", d.token_env);
        ok &= check(&label, env_set(&d.token_env));
    }
    if let Some(im) = &cfg.imessage {
        let label = format!("env {} is set (imessage password)", im.password_env);
        ok &= check(&label, env_set(&im.password_env));
        let label = format!("env {} is set (imessage webhook secret)", im.secret_env);
        ok &= check(&label, env_set(&im.secret_env));
        ok &= check(
            "BlueBubbles answers GET /api/v1/ping",
            bluebubbles_ping(im).await,
        );
        // A warning, not a failure: typing is opt-in, and the rest of Zephyr works without it.
        if im.typing {
            let on = async { zephyr::imessage::IMessage::new(im)?.private_api_on().await }.await;
            match on {
                Ok(true) => println!("ok   BlueBubbles Private API and helper are on (iMessage typing)"),
                Ok(false) => println!("warn iMessage typing needs the BlueBubbles Private API with its helper connected; enable it on the Mac or set [imessage] typing = false"),
                Err(e) => println!("warn could not read BlueBubbles server info for iMessage typing: {e:#}"),
            }
        }
    }
    if let Some(path) = cfg.attach.as_ref().and_then(|a| a.token_file.as_ref()) {
        let label = format!("{} is not readable by group or others", path.display());
        ok &= check(
            &label,
            std::fs::metadata(path).is_ok_and(|m| m.mode() & 0o077 == 0),
        );
    }
    // Warnings, not failures: a path that is not there means the CLI and the image are out of
    // step, and the child would still start (rpi skips an extension it cannot load).
    for path in cfg.policy_plugin.iter().chain(&cfg.extensions) {
        if !path.exists() {
            println!(
                "warn {} does not exist: no child can load it (the CLI and the image are out of step)",
                path.display()
            );
        }
    }
    // Warnings, not failures: the browser and desktop are extras the image brings (DESIGN §4.11).
    // `agent-browser` on the agent's PATH is the wrapper, which passes `--version` to the real one.
    if let Ok(display) = std::env::var("ZEPHYR_DESKTOP") {
        let chrome = std::env::var("AGENT_BROWSER_EXECUTABLE_PATH").unwrap_or_default();
        for (what, program, args) in [
            ("agent-browser", "agent-browser", vec!["--version"]),
            ("the agent's Chromium", chrome.as_str(), vec!["--version"]),
            (
                "the desktop",
                "xdpyinfo",
                vec!["-display", display.as_str()],
            ),
        ] {
            let mut cmd = std::process::Command::new(program);
            // The desktop takes only clients holding its cookie, as the agent's own do.
            if let Some(auth) = zephyr::desktop::xauthority() {
                cmd.env("XAUTHORITY", auth);
            }
            let out = zephyr::agent::cmd(&mut cmd).args(args).output();
            match out {
                Ok(o) if o.status.success() => println!("ok   {what} works"),
                _ => println!("warn {what} does not run ({program})"),
            }
        }
        if !Path::new("/dev/nvidia0").exists() {
            println!("warn no GPU in the container: the browser's WebGL is software (SwiftShader)");
        }
    }
    // Warnings, not failures: the agent's command-line tools from the image (DESIGN §4.11).
    for program in ["gh", "himalaya"] {
        let mut cmd = std::process::Command::new(program);
        match zephyr::agent::cmd(&mut cmd).arg("--version").output() {
            Ok(o) if o.status.success() => println!("ok   {program} works"),
            _ => println!("warn {program} does not run"),
        }
    }
    // Warnings, not failures: such a connector is left out, and the rest work (DESIGN §4.6).
    for m in &cfg.mcp {
        let missing = zephyr::drafts::missing_secrets(cfg, m);
        if !missing.is_empty() {
            println!(
                "warn connector {} is left out: secrets.env does not set {}",
                m.name,
                missing.join(", ")
            );
        }
    }
    // A warning, not a failure: providers come from the image (DESIGN §4.4).
    let ext_dir = zephyr::auth_keeper::agent_dir(cfg).join("extensions");
    let installed = {
        let _fs = zephyr::agent::fs();
        std::fs::read_dir(&ext_dir).is_ok_and(|mut d| d.next().is_some())
    };
    if installed {
        println!(
            "warn {} is not empty: rpi children ignore it (--local-only); providers come from the image",
            ext_dir.display()
        );
    }
    // A warning, not a failure: the daemon backs state.db up daily (`retention::backup`).
    if let Some(w) = zephyr::retention::backup_warning(cfg, std::time::SystemTime::now()) {
        println!("warn {w}");
    }
    if !ok {
        anyhow::bail!("doctor found problems");
    }
    Ok(())
}

fn check(label: &str, ok: bool) -> bool {
    println!("{} {label}", mark(ok));
    ok
}

/// The browser wrapper as the image lays it out (approvals spec §4.4, §4.9): `/opt/zephyr/gate/bin`
/// first on the agent's PATH, root's and not the agent's to write, its `agent-browser` this
/// binary, and no other `agent-browser` reachable on that PATH (one the agent dropped in its own
/// bin is shadowed, but says something is wrong).
fn browser_wrapper() -> bool {
    use zephyr::browser_gate::GATE_BIN;
    let path = zephyr::agent::AGENT_PATH;
    let first = path.split(':').next() == Some(GATE_BIN);
    let mut ok = check(&format!("{GATE_BIN} is first on the agent's PATH"), first);
    let theirs = zephyr::agent::ids().map_or(vec![], |(u, g)| {
        zephyr::agent::writable_dirs(GATE_BIN, u, g)
    });
    let roots = std::fs::metadata(GATE_BIN).is_ok_and(|m| m.uid() == 0 && m.mode() & 0o022 == 0);
    let label = format!("{GATE_BIN} is root's and the agent cannot write to it");
    ok &= check(&label, roots && theirs.is_empty());
    let wrapper = Path::new(GATE_BIN).join("agent-browser");
    let me = std::fs::canonicalize("/proc/self/exe");
    let is_me = matches!((std::fs::canonicalize(&wrapper), me), (Ok(a), Ok(b)) if a == b);
    let label = format!("{} is zephyr's browser wrapper", wrapper.display());
    ok &= check(&label, is_me);
    let others: Vec<String> = {
        // `/zephyr/.local/bin` is the agent's: looked at as the agent.
        let _fs = zephyr::agent::fs();
        (path.split(':').skip(1))
            .map(|d| Path::new(d).join("agent-browser"))
            .filter(|p| p.symlink_metadata().is_ok())
            .map(|p| p.display().to_string())
            .collect()
    };
    let label = match others.is_empty() {
        true => "no other agent-browser is on the agent's PATH".to_string(),
        false => format!(
            "another agent-browser is on the agent's PATH: {}",
            others.join(", ")
        ),
    };
    ok & check(&label, others.is_empty())
}

/// B5: rpi, this binary, the policy plugin and the real agent-browser are root's and writable by
/// no one else, and no folder above them is the agent's to write (`agent::replaceable`).
fn roots_binaries(cfg: &Config) -> bool {
    let Some((uid, gid)) = zephyr::agent::ids() else {
        return check("the agent's uid is known, to check root's programs", false);
    };
    let me = std::fs::canonicalize("/proc/self/exe").unwrap_or_else(|_| "/proc/self/exe".into());
    let mut programs = vec![cfg.rpi.clone(), me, cfg.agent_browser.clone()];
    programs.extend(cfg.policy_plugin.clone());
    let mut ok = true;
    for p in programs {
        let why = zephyr::agent::replaceable(&p, 0, uid, gid);
        let label = match &why {
            None => format!("{} is root's and the agent cannot replace it", p.display()),
            Some(why) => format!("{} could be replaced by the agent: {why}", p.display()),
        };
        ok &= check(&label, why.is_none());
    }
    ok
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// The ping route sits behind the server password, so a 2xx also means the password is right.
/// Errors are dropped, since reqwest's text includes the URL, and the URL carries the password.
async fn bluebubbles_ping(im: &zephyr::imessage::Config) -> bool {
    let Ok(password) = std::env::var(&im.password_env) else {
        return false;
    };
    let url = format!("{}/api/v1/ping", im.server_url.trim_end_matches('/'));
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    else {
        return false;
    };
    client
        .get(url)
        .query(&[("password", password)])
        .send()
        .await
        .is_ok_and(|r| r.status().is_success())
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok  "
    } else {
        "FAIL"
    }
}
