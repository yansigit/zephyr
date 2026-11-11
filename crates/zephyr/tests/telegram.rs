//! The Telegram adapter against a stub Bot API, with the router on fake-rpi (tests/fake-rpi.py).

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use zephyr::channel::{Activity, Channel, Inbound, Outbound};
use zephyr::config::Config;
use zephyr::store::Store;
use zephyr::telegram::Telegram;

const TOKEN: &str = "123456:tg-SECRET-token-value";
const SECRET: &str = "tg-SECRET-token-value";
const ENV: &str = "ZEPHYR_TELEGRAM_IT_TOKEN";

/// What the stub Bot API holds: every call, the updates not yet confirmed, canned answers.
#[derive(Default)]
struct Bot {
    /// `(method, body, when)`; a multipart body is `{"multipart": <its text>}`, a file
    /// download is method `file` with its path.
    calls: Vec<(String, Value, Instant)>,
    /// Served on `getUpdates` until an offset past them confirms them, as Telegram does.
    updates: VecDeque<Value>,
    /// Answers taken before the normal one, per method: `None` answers normally.
    script: HashMap<String, VecDeque<Option<(u16, Value)>>>,
    /// `file_id` → (`file_path`, bytes).
    files: HashMap<String, (String, Vec<u8>)>,
    next_msg: i64,
    /// How long each call to a method waits before it is answered (it is already recorded).
    slow: HashMap<String, Duration>,
}

type Stub = Arc<Mutex<Bot>>;

impl Bot {
    fn answer(&mut self, method: &str, body: &Value) -> Option<(u16, Value)> {
        if let Some(Some(canned)) = self.script.get_mut(method).and_then(VecDeque::pop_front) {
            return Some(canned);
        }
        let ok = |result: Value| Some((200, json!({"ok": true, "result": result})));
        match method {
            "getUpdates" => {
                let offset = body["offset"].as_i64().unwrap_or(0);
                self.updates
                    .retain(|u| u["update_id"].as_i64().unwrap() >= offset);
                if self.updates.is_empty() {
                    return None; // the caller waits, then answers []
                }
                ok(Value::Array(self.updates.iter().cloned().collect()))
            }
            "getFile" => {
                let id = body["file_id"].as_str().unwrap_or_default();
                match self.files.get(id) {
                    Some((path, _)) => ok(json!({"file_id": id, "file_path": path})),
                    None => Some((
                        400,
                        json!({"ok": false, "error_code": 400, "description": "Bad Request: invalid file_id"}),
                    )),
                }
            }
            "sendMessage" => {
                self.next_msg += 1;
                ok(json!({"message_id": self.next_msg}))
            }
            "getMe" => ok(json!({"id": 1, "is_bot": true, "username": "zephyr_test_bot"})),
            _ => ok(json!(true)),
        }
    }
}

/// A stub Bot API on 127.0.0.1 for `TOKEN`.
async fn stub() -> (String, Stub) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let bot = Stub::default();
    let state = bot.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let state = state.clone();
                    async move {
                        let (status, body) = serve(&state, req).await;
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .body(Full::new(Bytes::from(body)))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (format!("http://{addr}"), bot)
}

async fn serve(state: &Stub, req: Request<Incoming>) -> (u16, Vec<u8>) {
    let path = req.uri().path().to_string();
    let multipart = req
        .headers()
        .get("content-type")
        .is_some_and(|v| v.to_str().unwrap_or("").starts_with("multipart/"));
    let raw = req.into_body().collect().await.unwrap().to_bytes();
    if let Some(file) = path.strip_prefix(&format!("/file/bot{TOKEN}/")) {
        let mut bot = state.lock().unwrap();
        bot.calls.push(("file".into(), json!(file), Instant::now()));
        let found = bot
            .files
            .values()
            .find(|(p, _)| p == file)
            .map(|f| f.1.clone());
        return found.map_or((404, b"no".to_vec()), |b| (200, b));
    }
    let Some(method) = path.strip_prefix(&format!("/bot{TOKEN}/")) else {
        return (
            404,
            br#"{"ok":false,"error_code":404,"description":"Not Found"}"#.to_vec(),
        );
    };
    let body = if multipart {
        json!({"multipart": String::from_utf8_lossy(&raw)})
    } else {
        serde_json::from_slice(&raw).unwrap_or(Value::Null)
    };
    let (answer, slow) = {
        let mut bot = state.lock().unwrap();
        bot.calls
            .push((method.into(), body.clone(), Instant::now()));
        (bot.answer(method, &body), bot.slow.get(method).copied())
    };
    if let Some(slow) = slow {
        tokio::time::sleep(slow).await;
    }
    let (status, body) = match answer {
        Some(a) => a,
        // An empty long poll: wait a little, as Telegram holds it, so the loop does not spin.
        None => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            (200, json!({"ok": true, "result": []}))
        }
    };
    (status, serde_json::to_vec(&body).unwrap())
}

fn calls(bot: &Stub, method: &str) -> Vec<Value> {
    let bot = bot.lock().unwrap();
    bot.calls
        .iter()
        .filter(|c| c.0 == method)
        .map(|c| c.1.clone())
        .collect()
}

fn methods(bot: &Stub) -> Vec<String> {
    bot.lock()
        .unwrap()
        .calls
        .iter()
        .map(|c| c.0.clone())
        .collect()
}

fn push(bot: &Stub, updates: impl IntoIterator<Item = Value>) {
    bot.lock().unwrap().updates.extend(updates);
}

/// A private message from `from` as Telegram sends it; `extra` goes into `message`.
fn message(update: i64, from: i64, id: i64, extra: Value) -> Value {
    let mut m = json!({
        "message_id": id,
        "from": {"id": from, "is_bot": false, "first_name": "Aiden", "username": "aiden"},
        "chat": {"id": from, "type": "private", "first_name": "Aiden"},
        "date": 1_700_000_000,
    });
    m.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    json!({"update_id": update, "message": m})
}

fn text(update: i64, from: i64, id: i64, text: &str) -> Value {
    message(update, from, id, json!({"text": text}))
}

/// The local `chat` channel: the owner's terminal, for the tests that need a second channel.
struct Chat {
    sent: Arc<Mutex<Vec<Outbound>>>,
}

enum Ch {
    Chat(Chat),
    Tg(Arc<Telegram>),
}

impl Channel for Ch {
    fn name(&self) -> &str {
        match self {
            Ch::Chat(_) => "chat",
            Ch::Tg(t) => t.name(),
        }
    }
    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        match self {
            Ch::Chat(_) => std::future::pending().await,
            Ch::Tg(t) => t.run(tx).await,
        }
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        match self {
            Ch::Chat(c) => {
                c.sent.lock().unwrap().push(msg.clone());
                Ok(())
            }
            Ch::Tg(t) => t.send(msg).await,
        }
    }
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        match self {
            Ch::Chat(_) => Ok(()),
            Ch::Tg(t) => t.activity(to, a).await,
        }
    }
    fn live(&self, cfg: tokio::sync::watch::Receiver<Arc<Config>>) {
        if let Ch::Tg(t) = self {
            t.live(cfg)
        }
    }
    fn known_as(&self, sender: &str) -> Option<String> {
        match self {
            Ch::Chat(_) => None,
            Ch::Tg(t) => t.known_as(sender),
        }
    }
    fn acks(&self, acks: Arc<zephyr::channel::Acks>) {
        if let Ch::Tg(t) = self {
            t.acks(acks)
        }
    }
}

struct Gateway {
    tg: Arc<Telegram>,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<zephyr::router::Exit>>,
}

impl Gateway {
    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap().unwrap();
    }
}

fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zephyr-tg-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Owners: `owner` on chat, `55` on telegram. `extra` goes into `[telegram]`.
fn config(home: &Path, api: &str, extra: &str) -> Config {
    std::env::set_var(ENV, TOKEN);
    let mut cfg: Config = toml::from_str(&format!(
        "rpi = {:?}\nhome = {:?}\nagent_dir = {:?}\n[owners]\nchat = [\"owner\"]\ntelegram = [\"55\"]\n[telegram]\ntoken_env = {ENV:?}\n{extra}\n",
        concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
        home,
        home.join("agent"),
    ))
    .unwrap();
    cfg.telegram_api = api.into();
    cfg.tick = Duration::from_millis(200);
    cfg
}

fn adapter(cfg: &Config) -> Telegram {
    let mut tg = Telegram::new(cfg.telegram.as_ref().unwrap(), &cfg.telegram_api).unwrap();
    tg.conflict_wait = Duration::from_millis(300);
    tg.backoff = Duration::from_millis(100);
    tg
}

fn start(cfg: Config) -> Gateway {
    start_tuned(cfg, |_| {})
}

/// `start`, the adapter changed by `tune` first.
fn start_tuned(cfg: Config, tune: impl FnOnce(&mut Telegram)) -> Gateway {
    let mut tg = adapter(&cfg);
    tune(&mut tg);
    let tg = Arc::new(tg);
    let channels = vec![
        Ch::Chat(Chat {
            sent: Default::default(),
        }),
        Ch::Tg(tg.clone()),
    ];
    let (stop, stopped) = oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        zephyr::router::run(&cfg, &channels, async {
            let _ = stopped.await;
        })
        .await
    });
    Gateway {
        tg,
        stop: Some(stop),
        task,
    }
}

async fn wait(mut done: impl FnMut() -> bool) {
    for _ in 0..300 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out");
}

fn rows(home: &Path, status: &str) -> Vec<zephyr::store::InboxRow> {
    Store::open(&home.join("state.db"))
        .and_then(|s| s.inbox_with_status(status))
        .unwrap_or_default()
}

fn inbox_total(home: &Path) -> i64 {
    let Ok(store) = Store::open(&home.join("state.db")) else {
        return 0;
    };
    store
        .counts()
        .unwrap()
        .iter()
        .filter(|(t, _, _)| t == "inbox")
        .map(|c| c.2)
        .sum()
}

/// The texts the bot sent to `chat` with `sendMessage`, the progress note's left out.
fn said(bot: &Stub, chat: i64) -> Vec<String> {
    calls(bot, "sendMessage")
        .into_iter()
        .filter(|b| b["chat_id"] == chat)
        .map(|b| b["text"].as_str().unwrap().to_string())
        .filter(|t| !t.starts_with("≋ working"))
        .collect()
}

#[tokio::test]
async fn a_stranger_gets_nothing() {
    let home = home("stranger");
    let (api, bot) = stub().await;
    bot.lock()
        .unwrap()
        .files
        .insert("ph".into(), ("photos/a.jpg".into(), b"jpeg-bytes".to_vec()));
    push(
        &bot,
        [
            text(1, 999, 1, "hello"),
            message(
                2,
                999,
                2,
                json!({"photo": [{"file_id": "ph", "file_unique_id": "u", "width": 90, "height": 90, "file_size": 10}], "caption": "look"}),
            ),
            message(
                3,
                999,
                3,
                json!({"voice": {"file_id": "vo", "file_unique_id": "v", "duration": 2, "mime_type": "audio/ogg", "file_size": 900}}),
            ),
        ],
    );
    let gw = start(config(&home, &api, ""));
    wait(|| rows(&home, "dropped").len() == 3).await;
    // An owner's message after them: once it is answered, the strangers' rows were long handled.
    push(&bot, [text(4, 55, 4, "hi")]);
    wait(|| !said(&bot, 55).is_empty()).await;
    gw.shutdown().await;
    let dropped = rows(&home, "dropped");
    assert!(
        dropped.iter().all(|r| r.sender == "999"),
        "{:?}",
        dropped.len()
    );
    assert!(dropped[1]
        .text
        .contains("photo.jpg (image/jpeg, 1 KB, not downloaded)"));
    assert!(dropped[2]
        .text
        .contains("voice.ogg (audio/ogg, 1 KB, not downloaded)"));
    let all = bot.lock().unwrap().calls.clone();
    for (method, body, _) in &all {
        assert!(
            ["deleteWebhook", "getUpdates"].contains(&method.as_str()) || body["chat_id"] == 55,
            "{method} {body}"
        );
    }
    assert!(calls(&bot, "getFile").is_empty() && calls(&bot, "file").is_empty());
    assert!(!home.join("inbox").exists());
    assert_eq!(calls(&bot, "deleteWebhook").len(), 1);
}

#[tokio::test]
async fn an_owner_is_answered_in_chunks_with_files() {
    let home = home("owner");
    std::fs::write(home.join("slow"), "1").unwrap();
    std::fs::write(home.join("shot.png"), "PNG").unwrap();
    let (api, bot) = stub().await;
    bot.lock().unwrap().files.insert(
        "big".into(),
        ("photos/a.jpg".into(), b"jpeg-bytes".to_vec()),
    );
    let long = "x".repeat(5000);
    let caption = format!(r#"{long} [call send_file {{"path": "shot.png"}}]"#);
    push(
        &bot,
        [message(
            1,
            55,
            7,
            json!({"caption": caption, "photo": [
                {"file_id": "small", "file_unique_id": "s", "width": 90, "height": 60, "file_size": 3},
                {"file_id": "big", "file_unique_id": "b", "width": 1280, "height": 960, "file_size": 10}
            ]}),
        )],
    );
    let gw = start(config(&home, &api, "progress_after_secs = 0"));
    wait(|| !calls(&bot, "sendDocument").is_empty()).await;
    gw.shutdown().await;

    assert_eq!(calls(&bot, "getFile"), vec![json!({"file_id": "big"})]);
    assert_eq!(calls(&bot, "file"), vec![json!("photos/a.jpg")]);
    let saved = home.join("inbox/telegram-55-7-1.jpg");
    assert_eq!(std::fs::read(&saved).unwrap(), b"jpeg-bytes");

    let replies = said(&bot, 55);
    assert!(replies.len() >= 2, "{replies:?}");
    assert!(replies
        .iter()
        .all(|t| t.encode_utf16().count() <= zephyr::telegram::MAX_TEXT));
    let whole = replies.concat();
    assert!(whole.starts_with("echo: [telegram #1] xxx"), "{whole}");
    assert!(whole.contains(&saved.display().to_string()), "{whole}");
    for b in calls(&bot, "sendMessage") {
        assert!(b.get("parse_mode").is_none(), "{b}");
    }
    let doc = calls(&bot, "sendDocument");
    let form = doc[0]["multipart"].as_str().unwrap();
    assert!(
        form.contains("name=\"document\"; filename=\"shot.png\""),
        "{form}"
    );
    assert!(form.contains("PNG") && form.contains("55"), "{form}");

    // Typing, then the progress note, deleted before the reply; the file after the text.
    let order = methods(&bot);
    let at = |m: &str| order.iter().position(|x| x == m).unwrap();
    let typing = calls(&bot, "sendChatAction");
    assert!(!typing.is_empty(), "no typing: {order:?}");
    assert_eq!(typing[0], json!({"chat_id": 55, "action": "typing"}));
    let notes: Vec<Value> = calls(&bot, "sendMessage")
        .into_iter()
        .filter(|b| b["text"].as_str().unwrap().starts_with("≋ working"))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    let deleted = calls(&bot, "deleteMessage");
    assert_eq!(deleted[0]["chat_id"], 55);
    let texts: Vec<(usize, String)> = bot
        .lock()
        .unwrap()
        .calls
        .iter()
        .enumerate()
        .filter(|(_, c)| c.0 == "sendMessage")
        .map(|(i, c)| (i, c.1["text"].as_str().unwrap().to_string()))
        .collect();
    let note_at = texts.iter().find(|t| t.1.starts_with("≋")).unwrap().0;
    let reply_at = texts.iter().find(|t| t.1.starts_with("echo:")).unwrap().0;
    assert!(note_at < at("deleteMessage") && at("deleteMessage") < reply_at);
    assert!(at("sendDocument") > texts.last().unwrap().0);
}

#[tokio::test]
async fn a_group_is_left_and_never_a_row() {
    let home = home("group");
    let (api, bot) = stub().await;
    let member = |update: i64, chat: i64, kind: &str, status: &str| {
        json!({"update_id": update, "my_chat_member": {
            "chat": {"id": chat, "type": kind, "title": "g"},
            "from": {"id": 55, "is_bot": false, "first_name": "Aiden"},
            "date": 1_700_000_000,
            "old_chat_member": {"status": "left", "user": {"id": 1, "is_bot": true, "first_name": "z"}},
            "new_chat_member": {"status": status, "user": {"id": 1, "is_bot": true, "first_name": "z"}}
        }})
    };
    let mut in_group = text(3, 55, 3, "hi all");
    in_group["message"]["chat"] = json!({"id": -200, "type": "supergroup", "title": "g"});
    push(
        &bot,
        [
            member(1, -100, "group", "member"),
            member(2, -300, "supergroup", "left"),
            in_group,
            member(4, -400, "channel", "administrator"),
            text(5, 55, 5, "just me"),
        ],
    );
    let gw = start(config(&home, &api, ""));
    wait(|| !said(&bot, 55).is_empty()).await;
    gw.shutdown().await;
    let left: Vec<Value> = calls(&bot, "leaveChat")
        .iter()
        .map(|b| b["chat_id"].clone())
        .collect();
    assert_eq!(left, vec![json!(-100), json!(-200), json!(-400)]);
    assert_eq!(inbox_total(&home), 1);
    assert!(said(&bot, 55)[0].contains("just me"));
}

#[tokio::test]
async fn start_alone_is_not_a_row() {
    let home = home("start");
    let (api, bot) = stub().await;
    push(
        &bot,
        [
            text(1, 55, 1, "/start"),
            text(2, 999, 2, "/start"),
            text(3, 55, 3, "hi"),
        ],
    );
    let gw = start(config(&home, &api, ""));
    wait(|| !said(&bot, 55).is_empty()).await;
    gw.shutdown().await;
    assert_eq!(inbox_total(&home), 1);
    assert!(said(&bot, 55)[0].starts_with("echo: [telegram #1] hi"));
}

#[tokio::test]
async fn a_name_is_never_an_identity() {
    let home = home("name");
    let (api, bot) = stub().await;
    // The owner, then strangers named as the owner is, or named after the owner's id, and a
    // message from the owner's id in someone else's chat.
    let mut posing = text(3, 999, 3, "named 55");
    posing["message"]["from"]["first_name"] = json!("55");
    posing["message"]["from"]["username"] = json!("55");
    let mut elsewhere = text(4, 55, 4, "from 55 in 999's chat");
    elsewhere["message"]["chat"]["id"] = json!(999);
    let mut bot_user = text(5, 55, 5, "a bot with the owner's id");
    bot_user["message"]["from"]["is_bot"] = json!(true);
    push(
        &bot,
        [
            text(1, 55, 1, "me"),
            text(2, 999, 2, "also Aiden"),
            posing,
            elsewhere,
            bot_user,
            text(6, 55, 6, "me again"),
        ],
    );
    let gw = start(config(&home, &api, ""));
    wait(|| said(&bot, 55).len() >= 2).await;
    assert_eq!(gw.tg.known_as("999").as_deref(), Some(r#""55", @55"#));
    assert_eq!(gw.tg.known_as("55").as_deref(), Some(r#""Aiden", @aiden"#));
    gw.shutdown().await;
    let dropped = rows(&home, "dropped");
    assert_eq!(dropped.len(), 2);
    assert!(dropped.iter().all(|r| r.sender == "999"));
    assert_eq!(inbox_total(&home), 4);
    assert!(said(&bot, 999).is_empty());
}

#[tokio::test]
async fn a_retry_resends_only_the_failed_chunk() {
    let home = home("retry");
    let (api, bot) = stub().await;
    bot.lock().unwrap().script.insert(
        "sendMessage".into(),
        VecDeque::from([
            None,
            Some((
                500,
                json!({"ok": false, "error_code": 500, "description": "Internal Server Error"}),
            )),
        ]),
    );
    push(&bot, [text(1, 55, 1, &"y".repeat(5000))]);
    let gw = start(config(&home, &api, "progress = false"));
    wait(|| calls(&bot, "sendMessage").len() >= 3).await;
    // Nothing more comes: the reply is sent.
    wait(|| {
        Store::open(&home.join("state.db"))
            .unwrap()
            .counts()
            .unwrap()
            .iter()
            .any(|(t, s, n)| t == "outbox" && s == "sent" && *n == 1)
    })
    .await;
    gw.shutdown().await;
    let sent: Vec<String> = calls(&bot, "sendMessage")
        .iter()
        .map(|b| b["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(sent.len(), 3, "{sent:?}");
    assert_eq!(sent[1], sent[2], "the second chunk again");
    assert_ne!(sent[0], sent[1]);
    assert!(format!("{}{}", sent[0], sent[1]).starts_with("echo: [telegram #1] yyy"));
}

#[tokio::test]
async fn a_send_the_router_stopped_waiting_for_never_sends_a_piece_twice() {
    let home = home("cutoff");
    let (api, bot) = stub().await;
    bot.lock()
        .unwrap()
        .slow
        .insert("sendMessage".into(), Duration::from_millis(600));
    let tg = adapter(&config(&home, &api, "progress = false"));
    let msg = Outbound {
        channel: "telegram".into(),
        recipient: "55".into(),
        client_id: "z1".into(),
        text: "y".repeat(5000),
        attachment: None,
        reply_to: None,
    };
    // The router gives up on a send after its timeout (10 s; 300 ms here) and drops it.
    let cut = tokio::time::timeout(Duration::from_millis(300), tg.send(&msg)).await;
    assert!(cut.is_err(), "the send was still going");
    // A retry while it still goes sends nothing of its own.
    assert!(tg.send(&msg).await.is_err());
    let mut retried = false;
    for _ in 0..100 {
        if tg.send(&msg).await.is_ok() {
            retried = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(retried, "the retry went through once the send was done");
    assert_eq!(
        calls(&bot, "sendMessage").len(),
        2,
        "each of the two pieces once"
    );
}

#[tokio::test]
async fn a_second_reply_waits_for_the_first_to_the_same_chat() {
    let home = home("in-turn");
    let (api, bot) = stub().await;
    bot.lock()
        .unwrap()
        .slow
        .insert("sendMessage".into(), Duration::from_millis(400));
    let tg = adapter(&config(&home, &api, "progress = false"));
    let reply = |id: &str, text: String| Outbound {
        channel: "telegram".into(),
        recipient: "55".into(),
        client_id: id.into(),
        text,
        attachment: None,
        reply_to: None,
    };
    let first = reply("z1", format!("{}{}", "a".repeat(4096), "a".repeat(10)));
    let second = reply("z2", "b".into());
    // The first is cut off by the router's wait, and goes on; the second must not cut in.
    let _ = tokio::time::timeout(Duration::from_millis(100), tg.send(&first)).await;
    let mut done = [false, false];
    for _ in 0..100 {
        for (i, m) in [&first, &second].into_iter().enumerate() {
            if !done[i] {
                done[i] = tg.send(m).await.is_ok();
            }
        }
        if done == [true, true] {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(done, [true, true]);
    let order: Vec<char> = calls(&bot, "sendMessage")
        .iter()
        .map(|b| b["text"].as_str().unwrap().chars().next().unwrap())
        .collect();
    assert_eq!(
        order,
        ['a', 'a', 'b'],
        "the first reply whole, then the second"
    );
}

fn reply_to_55(id: &str, text: String) -> Outbound {
    Outbound {
        channel: "telegram".into(),
        recipient: "55".into(),
        client_id: id.into(),
        text,
        attachment: None,
        reply_to: None,
    }
}

fn still_going(e: &anyhow::Error) -> bool {
    e.downcast_ref::<zephyr::channel::StillGoing>().is_some()
}

#[tokio::test]
async fn a_send_stuck_on_rate_limits_fails_at_its_cap() {
    let home = home("cap");
    let (api, bot) = stub().await;
    let limited = json!({"ok": false, "error_code": 429, "description": "Too Many Requests",
        "parameters": {"retry_after": 1}});
    bot.lock().unwrap().script.insert(
        "sendMessage".into(),
        (0..100).map(|_| Some((429, limited.clone()))).collect(),
    );
    let mut tg = adapter(&config(&home, &api, "progress = false"));
    tg.send_cap = Duration::from_millis(4500);
    let msg = reply_to_55("z1", "a".into());
    let began = Instant::now();
    let failed = loop {
        match tg.send(&msg).await {
            Err(e) if still_going(&e) => tokio::time::sleep(Duration::from_millis(100)).await,
            done => break done,
        }
        assert!(began.elapsed() < Duration::from_secs(20), "never ended");
    };
    assert!(
        failed.is_err(),
        "it failed, and the router's failure path takes it"
    );
    assert!(began.elapsed() >= Duration::from_millis(4500));
}

#[tokio::test]
async fn a_reply_that_failed_partway_goes_on_before_the_next_to_its_chat() {
    let home = home("owed");
    let (api, bot) = stub().await;
    let broken = json!({"ok": false, "error_code": 500, "description": "Internal Server Error"});
    bot.lock().unwrap().script.insert(
        "sendMessage".into(),
        VecDeque::from([None, Some((500, broken))]),
    );
    let tg = adapter(&config(&home, &api, "progress = false"));
    let first = reply_to_55("z1", "a".repeat(4100));
    let second = reply_to_55("z2", "b".into());
    let cut = tg.send(&first).await.unwrap_err();
    assert!(!still_going(&cut), "{cut:#}");
    // Its rest goes before anything else to that chat.
    let waits = tg.send(&second).await.unwrap_err();
    assert!(still_going(&waits), "{waits:#}");
    tg.send(&first).await.unwrap();
    tg.send(&second).await.unwrap();
    let order: Vec<char> = calls(&bot, "sendMessage")
        .iter()
        .map(|b| b["text"].as_str().unwrap().chars().next().unwrap())
        .collect();
    assert_eq!(order, ['a', 'a', 'a', 'b']);
}

#[tokio::test]
async fn a_conflict_backs_off() {
    let home = home("conflict");
    let (api, bot) = stub().await;
    let conflict = json!({"ok": false, "error_code": 409,
        "description": "Conflict: terminated by other getUpdates request; make sure that only one bot instance is running"});
    let broken = json!({"ok": false, "error_code": 502, "description": "Bad Gateway"});
    bot.lock().unwrap().script.insert(
        "getUpdates".into(),
        VecDeque::from([
            Some((409, conflict.clone())),
            Some((409, conflict)),
            Some((502, broken.clone())),
            Some((502, broken)),
        ]),
    );
    push(&bot, [text(1, 55, 1, "after the conflict")]);
    let gw = start(config(&home, &api, ""));
    wait(|| !said(&bot, 55).is_empty()).await;
    gw.shutdown().await;
    let polls: Vec<Instant> = bot
        .lock()
        .unwrap()
        .calls
        .iter()
        .filter(|c| c.0 == "getUpdates")
        .map(|c| c.2)
        .collect();
    let gap = |i: usize| polls[i + 1] - polls[i];
    // A 409 waits `conflict_wait` (300 ms here, 30 s for real); other errors back off from
    // `backoff` (100 ms here, 5 s for real), doubling.
    assert!(gap(0) >= Duration::from_millis(300), "{:?}", gap(0));
    assert!(gap(1) >= Duration::from_millis(300), "{:?}", gap(1));
    assert!(gap(2) >= Duration::from_millis(100), "{:?}", gap(2));
    assert!(gap(3) >= Duration::from_millis(200), "{:?}", gap(3));
    assert_eq!(calls(&bot, "deleteWebhook").len(), 1);
}

#[tokio::test]
async fn the_token_never_shows() {
    let home = home("token");
    let (api, bot) = stub().await;
    let cfg = config(&home, &api, "");
    // A server that names the token in its error, and a server that is not there.
    let echo = json!({"ok": false, "error_code": 500, "description": format!("broken at /bot{TOKEN}/getMe")});
    for m in ["getMe", "sendMessage"] {
        bot.lock()
            .unwrap()
            .script
            .insert(m.into(), VecDeque::from([Some((500, echo.clone()))]));
    }
    let up = adapter(&cfg);
    let down = Telegram::new(cfg.telegram.as_ref().unwrap(), "http://127.0.0.1:1").unwrap();
    let reply = Outbound {
        channel: "telegram".into(),
        recipient: "55".into(),
        client_id: "c1".into(),
        text: "hi".into(),
        attachment: None,
        reply_to: None,
    };
    let mut errors = Vec::new();
    for tg in [&up, &down] {
        errors.push(tg.me().await.unwrap_err());
        errors.push(tg.send(&reply).await.unwrap_err());
    }
    assert_eq!(errors.len(), 4);
    for e in &errors {
        for text in [format!("{e:#}"), format!("{e:?}")] {
            assert!(!text.contains(SECRET), "{text}");
            assert!(!text.contains("http://"), "no URL: {text}");
        }
    }
    assert!(
        format!("{:#}", errors[0]).contains("500"),
        "{:#}",
        errors[0]
    );
    let mut gone = cfg.telegram.clone().unwrap();
    gone.token_env = "ZEPHYR_TELEGRAM_IT_UNSET".into();
    let e = Telegram::new(&gone, &api).err().unwrap();
    assert!(e.to_string().contains("ZEPHYR_TELEGRAM_IT_UNSET"));
    assert_eq!(up.me().await.unwrap(), "@zephyr_test_bot");
}

#[tokio::test]
async fn an_update_is_confirmed_only_once_the_router_took_its_row() {
    let home = home("stored");
    let (api, bot) = stub().await;
    push(&bot, [text(10, 55, 1, "one")]);
    let cfg = config(&home, &api, "");
    let tg = Arc::new(adapter(&cfg));
    tg.live(tokio::sync::watch::channel(Arc::new(cfg.clone())).1);
    let acks = Arc::new(zephyr::channel::Acks::default());
    tg.acks(acks.clone());
    // No router: the row reaches the channel, and nothing answers for it until the test does.
    let (tx, mut rx) = mpsc::channel(16);
    let run = {
        let tg = tg.clone();
        tokio::spawn(async move { tg.run(tx).await })
    };
    let row = rx.recv().await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let offsets = |bot: &Stub| -> Vec<i64> {
        calls(bot, "getUpdates")
            .iter()
            .map(|b| b["offset"].as_i64().unwrap())
            .collect()
    };
    assert!(offsets(&bot).iter().all(|o| *o == 0), "{:?}", offsets(&bot));
    acks.done(&row.channel, &row.platform_msg_id, true);
    wait(|| offsets(&bot).contains(&11)).await;
    run.abort();
}

#[tokio::test]
async fn a_row_the_router_ignores_moves_the_offset_on() {
    let home = home("ignored");
    let (api, bot) = stub().await;
    // A text of spaces, which the router trims to nothing and never stores, then a message
    // that must still be answered.
    push(&bot, [text(10, 55, 1, "   "), text(11, 55, 2, "hi")]);
    let gw = start(config(&home, &api, ""));
    wait(|| said(&bot, 55).iter().any(|t| t.contains("hi"))).await;
    wait(|| calls(&bot, "getUpdates").iter().any(|b| b["offset"] == 12)).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn a_row_whose_insert_failed_comes_again_once_the_database_recovers() {
    let home = home("unstored");
    let (api, bot) = stub().await;
    let cfg = config(&home, &api, "");
    let db = rusqlite::Connection::open(cfg.db_path()).unwrap();
    zephyr::store::Store::open(&cfg.db_path()).unwrap();
    db.execute_batch(
        "CREATE TRIGGER boom BEFORE INSERT ON inbox WHEN NEW.text = 'boom'
         BEGIN SELECT RAISE(ABORT, 'boom'); END;",
    )
    .unwrap();
    push(&bot, [text(10, 55, 1, "boom")]);
    let gw = start(cfg);
    // Not stored: the batch is not confirmed, and Telegram gives it again.
    wait(|| {
        let polls = calls(&bot, "getUpdates");
        polls.iter().filter(|b| b["offset"] == 0).count() >= 3
    })
    .await;
    assert!(!calls(&bot, "getUpdates").iter().any(|b| b["offset"] == 11));
    db.execute_batch("DROP TRIGGER boom;").unwrap();
    wait(|| said(&bot, 55).iter().any(|t| t.contains("boom"))).await;
    wait(|| calls(&bot, "getUpdates").iter().any(|b| b["offset"] == 11)).await;
    gw.shutdown().await;
}

#[tokio::test]
async fn a_slow_command_does_not_time_its_batch_out() {
    let home = home("slow-command");
    let (api, bot) = stub().await;
    // `/model` asks rpi for its models, held while `list_wait` exists: the router is busy long
    // after it stored the row.
    std::fs::write(home.join("list_wait"), "").unwrap();
    push(&bot, [text(10, 55, 1, "/model fake/m2")]);
    let gw = start_tuned(config(&home, &api, ""), |tg| {
        tg.answered_within = Duration::from_millis(500)
    });
    wait(|| calls(&bot, "getUpdates").iter().any(|b| b["offset"] == 11)).await;
    assert!(
        home.join("list_wait").exists(),
        "confirmed while the command ran"
    );
    std::fs::remove_file(home.join("list_wait")).unwrap();
    gw.shutdown().await;
}

#[tokio::test]
async fn offsets_are_confirmed_on_the_next_call() {
    let home = home("offsets");
    let (api, bot) = stub().await;
    push(&bot, [text(10, 55, 1, "one")]);
    let gw = start(config(&home, &api, ""));
    wait(|| said(&bot, 55).len() == 1).await;
    wait(|| calls(&bot, "getUpdates").iter().any(|b| b["offset"] == 11)).await;
    gw.shutdown().await;
    let first = calls(&bot, "getUpdates");
    assert_eq!(first[0]["offset"], 0);
    assert_eq!(first[0]["timeout"], 50);
    assert_eq!(
        first[0]["allowed_updates"],
        json!(["message", "my_chat_member"])
    );
    let seen = first.len();

    // A crash before the confirming call: Telegram gives the batch again, with what came since.
    push(&bot, [text(10, 55, 1, "one"), text(11, 55, 2, "two")]);
    let gw = start(config(&home, &api, ""));
    wait(|| said(&bot, 55).len() == 2).await;
    wait(|| calls(&bot, "getUpdates").iter().any(|b| b["offset"] == 12)).await;
    gw.shutdown().await;
    let again = &calls(&bot, "getUpdates")[seen..];
    assert_eq!(again[0]["offset"], 0);
    assert_eq!(
        inbox_total(&home),
        2,
        "the re-delivered update is a duplicate"
    );
    let replies = said(&bot, 55);
    assert!(
        replies[0].contains("one") && replies[1].contains("two"),
        "{replies:?}"
    );
}

#[tokio::test]
async fn pairing_over_telegram_names_the_user() {
    let home = home("pair");
    let (api, bot) = stub().await;
    // A file-backed config, so the claim can write `owners.telegram`.
    let path = home.join("zephyr.toml");
    std::fs::write(
        &path,
        format!(
            "rpi = {:?}\nhome = {:?}\nagent_dir = {:?}\n[owners]\nchat = [\"owner\"]\n[telegram]\ntoken_env = {ENV:?}\n",
            concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py"),
            home,
            home.join("agent"),
        ),
    )
    .unwrap();
    std::env::set_var(ENV, TOKEN);
    let mut cfg = Config::load(&path).unwrap();
    cfg.telegram_api = api.clone();
    cfg.tick = Duration::from_millis(200);
    // The code `/pair telegram` would have given on chat (tested in tests/router.rs).
    let (code, row) = zephyr::pairing::issue(zephyr::now(), None);
    Store::open(&cfg.db_path())
        .unwrap()
        .set_meta("pair_telegram", Some(&row))
        .unwrap();
    // A wrong code first: no answer at all.
    push(
        &bot,
        [
            text(1, 77, 1, "/pair AAAAA-AAAAA"),
            text(2, 77, 2, &format!("/pair {code}")),
        ],
    );
    let gw = start(cfg);
    wait(|| !said(&bot, 77).is_empty()).await;
    gw.shutdown().await;
    assert_eq!(
        said(&bot, 77),
        ["[zephyr] paired: you are an owner of this zephyr"]
    );
    assert_eq!(
        Config::load(&path).unwrap().owners["telegram"],
        ["77"],
        "the id, never the name"
    );
    let db = rusqlite::Connection::open(home.join("state.db")).unwrap();
    let told: String = db
        .query_row("SELECT text FROM outbox WHERE channel = 'chat'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        told,
        "[zephyr] Telegram user 77 (\"Aiden\", @aiden) is now an owner; /unpair telegram 77 removes them"
    );
    let texts: Vec<String> = rows(&home, "dropped").into_iter().map(|r| r.text).collect();
    assert_eq!(texts, ["/pair", "/pair"], "no code is kept");
}
