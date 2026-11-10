//! End to end against the pinned real `rpi`. The model is a local OpenAI-compatible
//! stub declared in a temp agent dir's models.json, so no real provider is called.
//! Run with: cargo test -p zephyr --test real_rpi -- --ignored

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Channel, Inbound, Outbound};
use zephyr::config::Config;

/// The real binary: `$RPI_BIN` (CI's `rpi-bump` workflow sets it), else the pinned release at its
/// fixed path.
static RPI: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    std::env::var("RPI_BIN")
        .unwrap_or_else(|_| "/home/user/Projects/.always-on/rpi-0.21.2/bin/rpi".into())
});

/// A test that needs the pinned rpi skips without it, unless ZEPHYR_REAL_RPI=1 (deploy/release.sh
/// sets it): then it fails, so a run cannot go green without testing anything.
fn skip_without_rpi(rpi: impl std::fmt::Display) {
    if std::env::var("ZEPHYR_REAL_RPI").as_deref() == Ok("1") {
        panic!("ZEPHYR_REAL_RPI=1 but {rpi} is not installed");
    }
    eprintln!("skipping: {rpi} is not installed");
}

struct TestChannel {
    feed: tokio::sync::Mutex<mpsc::Receiver<Inbound>>,
    sent: Mutex<Vec<Outbound>>,
}

impl Channel for TestChannel {
    fn name(&self) -> &str {
        "chat"
    }
    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        while let Some(m) = self.feed.lock().await.recv().await {
            tx.send(m).await?;
        }
        std::future::pending().await
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        self.sent.lock().unwrap().push(msg.clone());
        Ok(())
    }
}

/// Streams "pong" for every chat completion; holds the request open while `hang` is set.
/// Each request (head and body) is kept in `seen`.
async fn stub_provider(hang: Arc<AtomicBool>, seen: Arc<Mutex<Vec<String>>>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let hang = hang.swap(false, Ordering::SeqCst);
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 1 << 20];
                let mut got = Vec::new();
                while let Ok(n @ 1..) = sock.read(&mut buf).await {
                    got.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&got);
                    let Some(end) = text.find("\r\n\r\n") else {
                        continue;
                    };
                    let len = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|v| v.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if got.len() >= end + 4 + len {
                        break;
                    }
                }
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&got).into_owned());
                if hang {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
                let chunk = |delta: &str, finish: &str| {
                    format!("data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":0,\"model\":\"m\",\"choices\":[{{\"index\":0,\"delta\":{delta},\"finish_reason\":{finish}}}]}}\n\n")
                };
                let body = chunk(r#"{"role":"assistant","content":"pong"}"#, "null")
                    + &chunk("{}", "\"stop\"")
                    + "data: [DONE]\n\n";
                let resp = format!("HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
#[ignore = "needs the pinned rpi binary"]
async fn real_rpi_round_trip_and_kill_mid_run() {
    if !std::path::Path::new(&*RPI).exists() {
        skip_without_rpi(&*RPI);
        return;
    }
    let rpi: &str = &RPI;
    let hang = Arc::new(AtomicBool::new(false));
    let port = stub_provider(hang.clone(), Default::default()).await;
    let home = std::env::temp_dir().join(format!(
        "zephyr-real-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let agent = home.join("rpi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        format!(r#"{{"providers":{{"stub":{{"api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","apiKey":"not-a-key","models":[{{"id":"m","name":"M"}}]}}}}}}"#),
    )
    .unwrap();
    let cfg: Config = toml::from_str(&format!(
        "rpi = {rpi:?}\nhome = {home:?}\nagent_dir = {agent:?}\nmodel = \"stub/m\"\n[owners]\nchat = [\"owner\"]\n"
    ))
    .unwrap();

    let (feed, rx) = mpsc::channel(4);
    let channel = Arc::new(TestChannel {
        feed: tokio::sync::Mutex::new(rx),
        sent: Mutex::default(),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let ch = channel.clone();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, std::slice::from_ref(&*ch), async {
            let _ = stopped.await;
        })
        .await
    });
    let say = |id: &str, text: &str| Inbound {
        channel: "chat".into(),
        platform_msg_id: id.into(),
        sender: "owner".into(),
        text: text.into(),
    };
    let replies = |n: usize| {
        let channel = channel.clone();
        async move {
            for _ in 0..400 {
                if channel.sent.lock().unwrap().len() >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            channel
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|m| m.text.clone())
                .collect::<Vec<_>>()
        }
    };

    feed.send(say("m1", "ping")).await.unwrap();
    assert_eq!(replies(1).await, ["pong"]);

    // Kill the child while the provider holds the second request open.
    hang.store(true, Ordering::SeqCst);
    feed.send(say("m2", "again")).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let pattern = format!("--session-dir {}", home.join("sessions").display());
    let killed = std::process::Command::new("pkill")
        .args(["-9", "-f", "--", &pattern])
        .status()
        .unwrap();
    assert!(killed.success(), "no rpi child found");
    assert_eq!(replies(2).await, ["pong", "pong"]);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        channel.sent.lock().unwrap().len(),
        2,
        "a reply was sent twice"
    );

    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

/// DESIGN §4.6: zephyr records a clean conversation's saved file after each settle and around
/// a restart, so what the real rpi writes itself never reads as a change made outside it.
#[tokio::test]
#[ignore = "needs the pinned rpi binary"]
async fn real_rpi_keeps_a_conversation_clean_across_runs_and_a_restart() {
    if !std::path::Path::new(RPI).exists() {
        skip_without_rpi(RPI);
        return;
    }
    let port = stub_provider(Default::default(), Default::default()).await;
    let home = std::env::temp_dir().join(format!(
        "zephyr-real-clean-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let agent = home.join("rpi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        format!(r#"{{"providers":{{"stub":{{"api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","apiKey":"not-a-key","models":[{{"id":"m","name":"M"}}]}}}}}}"#),
    )
    .unwrap();
    let cfg: Config = toml::from_str(&format!(
        "rpi = {RPI:?}\nhome = {home:?}\nagent_dir = {agent:?}\nmodel = \"stub/m\"\n[owners]\nchat = [\"owner\"]\n"
    ))
    .unwrap();
    let (feed, rx) = mpsc::channel(4);
    let channel = Arc::new(TestChannel {
        feed: tokio::sync::Mutex::new(rx),
        sent: Mutex::default(),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let ch = channel.clone();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, std::slice::from_ref(&*ch), async {
            let _ = stopped.await;
        })
        .await
    });
    let say = |id: &str, text: &str| Inbound {
        channel: "chat".into(),
        platform_msg_id: id.into(),
        sender: "owner".into(),
        text: text.into(),
    };
    let replies = |n: usize| {
        let channel = channel.clone();
        async move {
            for _ in 0..400 {
                if channel.sent.lock().unwrap().len() >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            channel.sent.lock().unwrap().len()
        }
    };
    for (n, (id, text)) in [
        ("m1", "/new"),
        ("m2", "ping"),
        ("m3", "ping"),
        ("m4", "/rule be brief"),
        ("m5", "ping"),
        ("m6", "/thinking low"),
        ("m7", "ping"),
    ]
    .into_iter()
    .enumerate()
    {
        feed.send(say(id, text)).await.unwrap();
        assert_eq!(replies(n + 1).await, n + 1, "no answer to {text}");
    }
    let pongs = (channel.sent.lock().unwrap().iter())
        .filter(|m| m.text == "pong")
        .count();
    assert_eq!(pongs, 4, "{:?}", channel.sent.lock().unwrap());
    let store = zephyr::store::Store::open(&home.join("state.db")).unwrap();
    let id = store.main_session().unwrap();
    assert!(id.starts_with("main-"), "{id}");
    assert_eq!(
        store.session_origin(&id).unwrap(),
        zephyr::store::SessionOrigin::Clean,
        "{:?}",
        channel.sent.lock().unwrap()
    );
    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

/// Answers every request with HTTP 429 and counts them.
async fn rate_limited_provider(hits: Arc<std::sync::atomic::AtomicUsize>) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            hits.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0; 1 << 16];
                let _ = sock.read(&mut buf).await;
                let body =
                    r#"{"error":{"message":"Rate limit reached","type":"rate_limit_error"}}"#;
                let resp = format!("HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len());
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    port
}

#[tokio::test]
#[ignore = "needs the pinned rpi binary"]
async fn real_rpi_fails_over_to_the_next_account() {
    if !std::path::Path::new(&*RPI).exists() {
        skip_without_rpi(&*RPI);
        return;
    }
    let rpi: &str = &RPI;
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let limited = rate_limited_provider(hits.clone()).await;
    let ok = stub_provider(Arc::new(AtomicBool::new(false)), Default::default()).await;
    let home = std::env::temp_dir().join(format!(
        "zephyr-real-pool-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let agent = home.join("rpi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    let provider = |port: u16| {
        format!(
            r#"{{"api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","apiKey":"not-a-key","models":[{{"id":"m","name":"M"}}]}}"#
        )
    };
    std::fs::write(
        agent.join("models.json"),
        format!(
            r#"{{"providers":{{"pa":{},"pb":{}}}}}"#,
            provider(limited),
            provider(ok)
        ),
    )
    .unwrap();
    let cfg: Config = toml::from_str(&format!(
        "rpi = {rpi:?}\nhome = {home:?}\nagent_dir = {agent:?}\n[owners]\nchat = [\"owner\"]\n[[account]]\nname = \"a\"\nmodel = \"pa/m\"\n[[account]]\nname = \"b\"\nmodel = \"pb/m\"\n"
    ))
    .unwrap();

    let (feed, rx) = mpsc::channel(4);
    let channel = Arc::new(TestChannel {
        feed: tokio::sync::Mutex::new(rx),
        sent: Mutex::default(),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let ch = channel.clone();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, std::slice::from_ref(&*ch), async {
            let _ = stopped.await;
        })
        .await
    });
    feed.send(Inbound {
        channel: "chat".into(),
        platform_msg_id: "m1".into(),
        sender: "owner".into(),
        text: "ping".into(),
    })
    .await
    .unwrap();
    for _ in 0..600 {
        if channel.sent.lock().unwrap().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let sent: Vec<String> = channel
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.text.clone())
        .collect();
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert!(
        sent[0].starts_with("[zephyr] account a is rate-limited until "),
        "{sent:?}"
    );
    assert_eq!(sent[1], "pong");
    // The shared settings.json caps rpi's own retries at one.
    assert_eq!(hits.load(Ordering::SeqCst), 2);

    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "needs the pinned rpi binary"]
async fn real_rpi_btw_runs_with_no_tools_and_no_session_mid_run() {
    if !std::path::Path::new(&*RPI).exists() {
        skip_without_rpi(&*RPI);
        return;
    }
    let rpi: &str = &RPI;
    let (hang, seen) = (Arc::new(AtomicBool::new(false)), Arc::default());
    let port = stub_provider(hang.clone(), Arc::clone(&seen)).await;
    let home = std::env::temp_dir().join(format!(
        "zephyr-real-btw-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let agent = home.join("rpi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        format!(r#"{{"providers":{{"stub":{{"api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","apiKey":"not-a-key","models":[{{"id":"m","name":"M"}}]}}}}}}"#),
    )
    .unwrap();
    // The policy plugin, when this checkout has built it: the btw child must still load it.
    let plugin = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/debug/libzephyr_policy.so"
    );
    let plugin = match std::path::Path::new(plugin).exists() {
        true => format!("policy_plugin = {plugin:?}\n"),
        false => String::new(),
    };
    let cfg: Config = toml::from_str(&format!(
        "rpi = {rpi:?}\nhome = {home:?}\nagent_dir = {agent:?}\nmodel = \"stub/m\"\n{plugin}[owners]\nchat = [\"owner\"]\n"
    ))
    .unwrap();

    let (feed, rx) = mpsc::channel(4);
    let channel = Arc::new(TestChannel {
        feed: tokio::sync::Mutex::new(rx),
        sent: Mutex::default(),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let ch = channel.clone();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, std::slice::from_ref(&*ch), async {
            let _ = stopped.await;
        })
        .await
    });
    let say = |id: &str, text: &str| Inbound {
        channel: "chat".into(),
        platform_msg_id: id.into(),
        sender: "owner".into(),
        text: text.into(),
    };
    let replies = |n: usize| {
        let channel = channel.clone();
        async move {
            for _ in 0..400 {
                if channel.sent.lock().unwrap().len() >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            channel
                .sent
                .lock()
                .unwrap()
                .iter()
                .map(|m| m.text.clone())
                .collect::<Vec<_>>()
        }
    };
    // Every session file under the home, by name.
    let sessions = || -> Vec<String> {
        let (mut found, mut dirs) = (Vec::new(), vec![home.clone()]);
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap().flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() {
                    dirs.push(e.path());
                } else if name.contains(".jsonl") {
                    found.push(name);
                }
            }
        }
        found.sort();
        found
    };

    feed.send(say("m1", "ping")).await.unwrap();
    assert_eq!(replies(1).await, ["pong"]);
    let before = sessions();
    assert!(
        before.iter().any(|f| f.ends_with("_main.jsonl")),
        "{before:?}"
    );

    // Main is mid-run (the provider holds its request open) when the side question comes.
    hang.store(true, Ordering::SeqCst);
    feed.send(say("m2", "again")).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    feed.send(say("m3", "/btw what did I say?")).await.unwrap();
    assert_eq!(replies(2).await, ["pong", "[btw] pong"]);
    assert_eq!(sessions(), before, "the btw child wrote no session file");

    let bodies: Vec<String> = seen.lock().unwrap().clone();
    let body = |r: &str| -> serde_json::Value {
        serde_json::from_str(r.split_once("\r\n\r\n").unwrap().1).unwrap()
    };
    let btw: Vec<&String> = bodies
        .iter()
        .filter(|r| r.contains("side question"))
        .collect();
    assert_eq!(btw.len(), 1, "{bodies:?}");
    let btw = body(btw[0]);
    assert!(
        btw["tools"].as_array().is_none_or(|t| t.is_empty()),
        "{btw}"
    );
    let text = btw.to_string();
    assert!(text.contains("user: ping"), "{text}");
    assert!(text.contains("assistant: pong"), "{text}");
    assert!(text.contains("user: again"), "read mid-run: {text}");
    assert!(body(&bodies[0])["tools"]
        .as_array()
        .is_some_and(|t| !t.is_empty()));

    feed.send(say("m4", "/stop")).await.unwrap();
    replies(3).await;
    let _ = stop.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
#[ignore = "needs the pinned rpi binary"]
async fn real_rpi_undo_retry_and_compact() {
    if !std::path::Path::new(&*RPI).exists() {
        skip_without_rpi(&*RPI);
        return;
    }
    let rpi: &str = &RPI;
    let seen: Arc<Mutex<Vec<String>>> = Arc::default();
    let port = stub_provider(Arc::default(), Arc::clone(&seen)).await;
    let home = std::env::temp_dir().join(format!(
        "zephyr-real-undo-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let agent = home.join("rpi-agent");
    std::fs::create_dir_all(&agent).unwrap();
    std::fs::write(
        agent.join("models.json"),
        format!(r#"{{"providers":{{"stub":{{"api":"openai-completions","baseUrl":"http://127.0.0.1:{port}/v1","apiKey":"not-a-key","models":[{{"id":"m","name":"M"}}]}}}}}}"#),
    )
    .unwrap();
    let cfg: Config = toml::from_str(&format!(
        "rpi = {rpi:?}\nhome = {home:?}\nagent_dir = {agent:?}\nmodel = \"stub/m\"\n[owners]\nchat = [\"owner\"]\n"
    ))
    .unwrap();
    // Every command zephyr sends is one this rpi supports (`doctor` checks the same).
    let probe = zephyr::rpc::Child::spawn(&cfg, "probe").await.unwrap();
    let supported = probe.ready["supportedCommands"].as_array().unwrap().clone();
    for c in zephyr::rpc::USED_COMMANDS {
        assert!(supported.iter().any(|s| s == c), "{c} not in {supported:?}");
    }
    probe.stop().await;

    let (feed, rx) = mpsc::channel(4);
    let channel = Arc::new(TestChannel {
        feed: tokio::sync::Mutex::new(rx),
        sent: Mutex::default(),
    });
    let (stop, stopped) = oneshot::channel::<()>();
    let ch = channel.clone();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, std::slice::from_ref(&*ch), async {
            let _ = stopped.await;
        })
        .await
    });
    let say = |id: &str, text: &str| Inbound {
        channel: "chat".into(),
        platform_msg_id: id.into(),
        sender: "owner".into(),
        text: text.into(),
    };
    let replies = |n: usize| {
        let channel = channel.clone();
        async move {
            for _ in 0..400 {
                if channel.sent.lock().unwrap().len() >= n {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let sent = channel.sent.lock().unwrap();
            sent.iter().map(|m| m.text.clone()).collect::<Vec<_>>()
        }
    };
    // The newest request the provider got.
    let last_request = || seen.lock().unwrap().last().cloned().unwrap_or_default();

    feed.send(say("m1", "ping")).await.unwrap();
    replies(1).await;
    feed.send(say("m2", "again")).await.unwrap();
    replies(2).await;
    feed.send(say("m3", "/undo")).await.unwrap();
    let undone = replies(3).await[2].clone();
    assert!(undone.starts_with("[zephyr] undone:"), "{undone}");
    // The model no longer sees the undone exchange; the one before it stays.
    feed.send(say("m4", "third")).await.unwrap();
    assert_eq!(replies(4).await[3], "pong");
    let request = last_request();
    assert!(request.contains("[chat #1] ping"), "{request}");
    assert!(!request.contains("[chat #2] again"), "{request}");

    feed.send(say("m5", "/retry")).await.unwrap();
    let sent = replies(6).await;
    assert_eq!(
        sent[4..],
        [
            "[zephyr] undone; sending your last message again as #6",
            "pong"
        ]
    );
    let request = last_request();
    assert!(request.contains("[chat #6] third"), "{request}");
    assert!(!request.contains("[chat #4] third"), "{request}");

    feed.send(say("m6", "/compact keep it short"))
        .await
        .unwrap();
    let summary = replies(7).await[6].clone();
    assert!(
        summary.starts_with("[zephyr] Conversation compacted"),
        "{summary}"
    );
    assert!(last_request().contains("keep it short"));
    feed.send(say("m7", "after")).await.unwrap();
    assert_eq!(replies(8).await[7], "pong");

    // rpi starts at medium and offers off..high for this model.
    feed.send(say("m8", "/thinking next")).await.unwrap();
    assert_eq!(replies(9).await[8], "[zephyr] thinking high");
    // rpi 0.18 has no `fork` command: zephyr copies the session, and the copy carries on.
    feed.send(say("m9", "/fork")).await.unwrap();
    let forked = replies(10).await[9].clone();
    assert!(
        forked.starts_with("[zephyr] forked main into main-"),
        "{forked}"
    );
    feed.send(say("m10", "forked turn")).await.unwrap();
    assert_eq!(replies(11).await[10], "pong");
    let request = last_request();
    assert!(request.contains("[chat #8] after"), "{request}");
    assert!(request.contains("[chat #11] forked turn"), "{request}");
    feed.send(say("m11", "/undo 2")).await.unwrap();
    let undone = replies(12).await[11].clone();
    assert!(undone.contains("your last 2 messages"), "{undone}");
    feed.send(say("m12", "last")).await.unwrap();
    assert_eq!(replies(13).await[12], "pong");
    let request = last_request();
    assert!(!request.contains("[chat #8] after"), "{request}");
    assert!(!request.contains("[chat #11] forked turn"), "{request}");

    let _ = stop.send(());
    task.await.unwrap().unwrap();
}
