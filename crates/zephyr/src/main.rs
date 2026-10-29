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

const USAGE: &str = "usage: zephyr <run|status|doctor|chat|pause|resume>  (config: $ZEPHYR_CONFIG or ~/agent/zephyr.toml)";

#[tokio::main]
async fn main() -> ExitCode {
    let cmd = std::env::args().nth(1).unwrap_or_default();
    let result = match Config::load(&Config::default_path()) {
        Err(e) if cmd != "-h" && cmd != "--help" => Err(e),
        cfg => match (cmd.as_str(), cfg) {
            ("run", Ok(cfg)) => run(cfg).await,
            ("status", Ok(cfg)) => status(&cfg).await,
            ("doctor", Ok(cfg)) => doctor(&cfg).await,
            ("chat", Ok(cfg)) => chat::client(&cfg.socket_path()).await,
            ("pause", Ok(cfg)) => control(&cfg, "/pause").await,
            ("resume", Ok(cfg)) => control(&cfg, "/resume").await,
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::from(2);
            }
        },
    };
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

async fn run(cfg: Config) -> anyhow::Result<()> {
    let _lock = lock_home(&cfg.home)?;
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
    zephyr::router::run(&cfg, &channels, shutdown).await
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
    let paused = store.pause_reason()?;
    println!(
        "paused: {}",
        paused.map_or("no".into(), |r| format!("yes ({r})"))
    );
    let day = store.today()?;
    let (tokens, cost) = store.spend(&day)?;
    let budget = cfg.budget.clone().unwrap_or_default();
    println!(
        "spend:  {day} UTC: {tokens} tokens (limit {}), ${cost:.4} (limit {})",
        limit(budget.daily_tokens),
        limit(budget.daily_cost_usd)
    );
    for (table, status, n) in store.counts()? {
        println!("{table:<7} {status:<11} {n}");
    }
    Ok(())
}

fn limit<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("none".into(), |v| v.to_string())
}

async fn doctor(cfg: &Config) -> anyhow::Result<()> {
    let out = std::process::Command::new(&cfg.rpi)
        .arg("--version")
        .output()?;
    let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let version_ok = version.starts_with(RPI_PIN);
    println!(
        "{} rpi version: {version} (want {RPI_PIN}x)",
        mark(version_ok)
    );

    // A throwaway home so doctor never touches the live `main` session.
    let mut probe = cfg.clone();
    probe.home = std::env::temp_dir().join(format!("zephyr-doctor-{}", std::process::id()));
    std::fs::create_dir_all(&probe.home)?;
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
    let _ = std::fs::remove_dir_all(&probe.home);

    // Names and presence only: a secret's value is never printed.
    let mut ok = version_ok && commands_ok;
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
    }
    if let Some(path) = cfg.attach.as_ref().and_then(|a| a.token_file.as_ref()) {
        let label = format!("{} is not readable by group or others", path.display());
        ok &= check(
            &label,
            std::fs::metadata(path).is_ok_and(|m| m.mode() & 0o077 == 0),
        );
    }
    // rpi loads AGENTS.md only if trust.json does not deny the home (DESIGN §4.6).
    let context_ok =
        match zephyr::memory::context_trusted(&cfg.home, &zephyr::auth_keeper::agent_dir(cfg)) {
            Ok(ok) => ok,
            Err(e) => {
                println!("{} reading trust.json: {e:#}", mark(false));
                false
            }
        };
    ok &= check(
        &format!(
            "rpi loads AGENTS.md from {} (trust.json does not deny it)",
            cfg.home.display()
        ),
        context_ok,
    );
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
