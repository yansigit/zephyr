use std::process::ExitCode;
use zephyr::channel::AnyChannel;
use zephyr::chat::{self, ChatChannel};
use zephyr::config::Config;
use zephyr::discord::Discord;
use zephyr::rpc::{Child, USED_COMMANDS};
use zephyr::store::Store;

/// The rpi minor this build speaks (DESIGN §3).
const RPI_PIN: &str = "rpi 0.12.";

const USAGE: &str =
    "usage: zephyr <run|status|doctor|chat>  (config: $ZEPHYR_CONFIG or ~/agent/zephyr.toml)";

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

async fn run(cfg: Config) -> anyhow::Result<()> {
    let mut channels = vec![AnyChannel::Chat(ChatChannel::new(cfg.socket_path()))];
    if let Some(discord) = &cfg.discord {
        channels.push(AnyChannel::Discord(Discord::new(discord)?));
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

async fn status(cfg: &Config) -> anyhow::Result<()> {
    let up = tokio::net::UnixStream::connect(cfg.socket_path())
        .await
        .is_ok();
    println!("daemon: {}", if up { "running" } else { "not reachable" });
    for (table, status, n) in Store::open(&cfg.db_path())?.counts()? {
        println!("{table:<7} {status:<11} {n}");
    }
    Ok(())
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
    if !(version_ok && commands_ok) {
        anyhow::bail!("doctor found problems");
    }
    Ok(())
}

fn mark(ok: bool) -> &'static str {
    if ok {
        "ok  "
    } else {
        "FAIL"
    }
}
