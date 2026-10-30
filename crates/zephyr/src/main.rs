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
const RPI_PIN: &str = "rpi 0.12.";

const USAGE: &str = "usage: zephyr <run|status|doctor|chat|tui|pause|resume|reload>  (config: $ZEPHYR_CONFIG or ~/agent/zephyr.toml)";

#[tokio::main]
async fn main() -> ExitCode {
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
    if host && cmd != "run" {
        eprintln!("{}", zephyr::host::USAGE);
        return ExitCode::from(2);
    }
    if cmd == "__file" {
        return file_cmd(&args.collect::<Vec<_>>());
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
                        .and_then(|_| zephyr::host::conf::replace_private(&file, &text))
                }
                _ => Err(anyhow::anyhow!("usage: zephyr __file get|put NAME")),
            }
        }
        _ => Err(anyhow::anyhow!("usage: zephyr __file get|put NAME")),
    };
    finish(result)
}

fn finish(result: anyhow::Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zephyr: {e:#}");
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
        let mut cmd = std::process::Command::new(std::env::current_exe()?);
        cmd.args(std::env::args_os().skip(1));
        for key in from_file {
            cmd.env_remove(key);
        }
        // Only returns on failure. The lock and every socket are close-on-exec.
        return Err(cmd.exec().into());
    }
    Ok(())
}

/// `zephyr pause` / `resume`: through the running daemon's chat socket, so a run in progress is
/// aborted. With no daemon, the store carries the flag to the next start.
async fn control(cfg: &Config, text: &str) -> anyhow::Result<()> {
    if let Ok(stream) = tokio::net::UnixStream::connect(cfg.socket_path()).await {
        let (read, mut write) = stream.into_split();
        let id = format!("cli-{}-{}", std::process::id(), zephyr::unix_millis());
        let mut line = serde_json::to_vec(&json!({"id": id, "text": text}))?;
        line.push(b'\n');
        write.write_all(&line).await?;
        let mut lines = BufReader::new(read).lines();
        // The reply comes back on this connection; no reply means this uid is not an owner.
        let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
            .await
            .map_err(|_| {
                anyhow::anyhow!("no reply from zephyr within 10s; is this user in owners.chat?")
            })??;
        let v: serde_json::Value = serde_json::from_str(reply.as_deref().unwrap_or("null"))?;
        println!("{}", v["text"].as_str().unwrap_or_default());
    } else {
        let store = Store::open(&cfg.db_path())?;
        store.set_paused((text == "/pause").then_some("user"))?;
        println!("{text}: the daemon is not running; the change applies at its next start");
    }
    Ok(())
}

async fn status(cfg: &Config) -> anyhow::Result<()> {
    let up = tokio::net::UnixStream::connect(cfg.socket_path())
        .await
        .is_ok();
    println!("daemon: {}", if up { "running" } else { "not reachable" });
    let store = Store::open(&cfg.db_path())?;
    for line in zephyr::reload::status_lines(cfg, &store)? {
        println!("{line}");
    }
    for (table, status, n) in store.counts()? {
        println!("{table:<7} {status:<11} {n}");
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
        ok
    };

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
    let mut ok = version_ok && split_ok && commands_ok;
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
    if !ok {
        anyhow::bail!("doctor found problems");
    }
    Ok(())
}

fn check(label: &str, ok: bool) -> bool {
    println!("{} {label}", mark(ok));
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
