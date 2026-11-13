//! Telegram channel adapter: a bot in private chats, over Bot API long polling. See DESIGN.md
//! section 4.3. The token is in every URL (`/bot<token>/…`), so no URL is ever logged and every
//! error that leaves here has gone through `without_url()` and has the token redacted.

use crate::channel::{chunks, Activity, Channel, Inbound, Outbound};
use crate::discord::{progress_text, Progress, Step};
use crate::inbox::{File, Src};
use anyhow::Context as _;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// `[telegram]` takes Discord's three keys: `token_env`, `progress`, `progress_after_secs`.
pub use crate::discord::Config;

/// Telegram's message limit, in UTF-16 units (its own count).
pub const MAX_TEXT: usize = 4096;
/// The Bot API serves files up to this size.
const MAX_FILE: u64 = 20 * 1024 * 1024;
/// How long a `getUpdates` waits for an update; the client's timeout is longer.
const POLL_SECS: u64 = 50;
const TIMEOUT: Duration = Duration::from_secs(60);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);
/// `sendChatAction` lasts 5 s.
const TYPING_EVERY: Duration = Duration::from_secs(4);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
const MAX_NAMES: usize = 1000;

/// The Bot API for one token. No `Debug`: it holds the token.
#[derive(Clone)]
struct Api {
    http: reqwest::Client,
    base: String,
    token: String,
}

/// A Bot API answer that was not `ok`: the HTTP status and Telegram's description, redacted.
#[derive(Debug)]
struct Refused {
    method: String,
    status: u16,
    text: String,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "telegram {} returned {}: {}",
            self.method, self.status, self.text
        )
    }
}

impl std::error::Error for Refused {}

impl Api {
    fn redact(&self, text: &str) -> String {
        let secret = self.token.split_once(':').map_or("", |(_, s)| s);
        let text = text.replace(&self.token, "[redacted]");
        if secret.is_empty() {
            text
        } else {
            text.replace(secret, "[redacted]")
        }
    }

    /// The error with its cause chain, minus the URL and the token.
    fn scrub(&self, method: &str, e: reqwest::Error) -> anyhow::Error {
        let text = format!("{:#}", anyhow::Error::new(e.without_url()));
        anyhow::anyhow!("telegram {method}: {}", self.redact(&text))
    }

    async fn json(&self, method: &str, body: Value) -> anyhow::Result<Value> {
        self.call(method, |r| r.json(&body)).await
    }

    /// POSTs to `method` (the request built by `with`, again on a retry) and returns its
    /// `result`. A 429 waits its `retry_after` and tries again.
    async fn call(
        &self,
        method: &str,
        with: impl Fn(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
    ) -> anyhow::Result<Value> {
        loop {
            let url = format!("{}/bot{}/{method}", self.base, self.token);
            let res = with(self.http.post(url))
                .send()
                .await
                .map_err(|e| self.scrub(method, e))?;
            let status = res.status().as_u16();
            let bytes = res.bytes().await.map_err(|e| self.scrub(method, e))?;
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            if (200..300).contains(&status) && v["ok"] == true {
                return Ok(v["result"].clone());
            }
            if let (429, Some(secs)) = (status, v["parameters"]["retry_after"].as_u64()) {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                continue;
            }
            // Redacted before it is cut, so a cut cannot leave part of the token.
            let text = match v["description"].as_str() {
                Some(d) => self.redact(d),
                None => self.redact(&String::from_utf8_lossy(&bytes)),
            };
            let text = text.chars().take(300).collect();
            return Err(Refused {
                method: method.into(),
                status,
                text,
            }
            .into());
        }
    }
}

/// A recipient's run in progress; its typing refreshes in `Telegram::typing` until `finish`.
#[derive(Default)]
struct Run {
    progress: Progress<i64>,
}

pub struct Telegram {
    api: Api,
    /// `None` when `progress = false`.
    progress_after: Option<Duration>,
    /// Keyed by chat id. A tokio mutex: `send` waits for an in-flight progress post, so it deletes it.
    runs: tokio::sync::Mutex<HashMap<i64, Run>>,
    /// The pieces of each reply Telegram took: it has no nonce to drop a duplicate.
    delivered: Arc<crate::channel::Delivered>,
    /// How each sender last named themselves, for the pairing notice only (`known_as`).
    names: Mutex<HashMap<String, String>>,
    /// Owners and home, for saving the files an owner sends (`inbox`).
    live: crate::inbox::Live,
    /// The router's answers for the rows sent to it (`Channel::acks`).
    acks: std::sync::OnceLock<Arc<crate::channel::Acks>>,
    /// The typing refresher per chat.
    typing: crate::channel::Typing,
    /// Set by the first typing error, so later ones are not logged.
    typing_warned: Arc<AtomicBool>,
    /// How long a 409 (another client polling this bot) waits before the next poll.
    #[doc(hidden)]
    pub conflict_wait: Duration,
    /// The first wait after a failed poll; it doubles up to 5 minutes.
    #[doc(hidden)]
    pub backoff: Duration,
    /// How long the router may take to answer for a batch's rows (it answers once each is
    /// stored) before the batch is fetched again.
    #[doc(hidden)]
    pub answered_within: Duration,
    /// The most one send may take (`channel::SEND_CAP`).
    #[doc(hidden)]
    pub send_cap: Duration,
}

impl Telegram {
    pub fn new(cfg: &Config, api: &str) -> anyhow::Result<Self> {
        let token = std::env::var(&cfg.token_env)
            .with_context(|| format!("env var {} is not set", cfg.token_env))?;
        anyhow::ensure!(!token.is_empty(), "env var {} is empty", cfg.token_env);
        Ok(Self {
            api: Api {
                http: reqwest::Client::builder().timeout(TIMEOUT).build()?,
                base: api.trim_end_matches('/').to_string(),
                token,
            },
            progress_after: cfg
                .progress
                .then(|| Duration::from_secs(cfg.progress_after_secs)),
            runs: Default::default(),
            delivered: Default::default(),
            names: Default::default(),
            live: Default::default(),
            acks: Default::default(),
            typing: Default::default(),
            typing_warned: Default::default(),
            conflict_wait: Duration::from_secs(30),
            backoff: Duration::from_secs(5),
            answered_within: Duration::from_secs(60),
            send_cap: crate::channel::SEND_CAP,
        })
    }

    /// The bot's `@username`, from `getMe` (doctor).
    pub async fn me(&self) -> anyhow::Result<String> {
        let me = self.api.json("getMe", json!({})).await?;
        let name = me["username"].as_str().context("getMe named no username")?;
        Ok(format!("@{name}"))
    }

    /// Polls for ever: errors are logged and waited out, since a `run` error ends the router.
    async fn poll(&self, tx: &mpsc::Sender<Inbound>) {
        let (mut offset, mut hooked, mut backoff, mut conflicted) = (0, false, self.backoff, false);
        loop {
            match self.batch(tx, &mut offset, &mut hooked).await {
                Ok(()) => (backoff, conflicted) = (self.backoff, false),
                Err(e) if e.downcast_ref::<Refused>().is_some_and(|r| r.status == 409) => {
                    if !std::mem::replace(&mut conflicted, true) {
                        eprintln!(
                            "zephyr: telegram: another client reads this bot's updates (a second zephyr, or a side container on the same token); trying again every {}s: {e:#}",
                            self.conflict_wait.as_secs()
                        );
                    }
                    tokio::time::sleep(self.conflict_wait).await;
                }
                Err(e) => {
                    eprintln!(
                        "zephyr: telegram polling failed, trying again in {}s: {e:#}",
                        backoff.as_secs()
                    );
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    /// One `getUpdates`, its rows sent to `tx`. The offset moves past the batch only once
    /// they all went, and Telegram counts them taken only when the next call carries it, so a
    /// crash before then re-delivers them and the inbox's dedupe drops what was stored.
    async fn batch(
        &self,
        tx: &mpsc::Sender<Inbound>,
        offset: &mut i64,
        hooked: &mut bool,
    ) -> anyhow::Result<()> {
        if !*hooked {
            self.api.json("deleteWebhook", json!({})).await?;
            *hooked = true;
        }
        let body = json!({
            "offset": *offset,
            "timeout": POLL_SECS,
            "allowed_updates": ["message", "my_chat_member"],
        });
        let updates = self.api.json("getUpdates", body).await?;
        let (mut next, mut answers) = (*offset, Vec::new());
        for u in updates.as_array().into_iter().flatten() {
            let Some(id) = u["update_id"].as_i64() else {
                continue;
            };
            answers.extend(self.take(u, tx).await);
            next = next.max(id + 1);
        }
        // Telegram counts the batch taken at the next call carrying `next`: only once the
        // router is done with every row of it, so a crash before then re-delivers it.
        let all = futures_util::future::join_all(answers);
        let answered = tokio::time::timeout(self.answered_within, all)
            .await
            .map_err(|_| anyhow::anyhow!("the router took no row of a batch in time"))?;
        anyhow::ensure!(
            answered.iter().all(|a| matches!(a, Ok(true))),
            "the router could not store a row of a batch; it comes again"
        );
        *offset = next;
        Ok(())
    }

    /// The router's answer for the row `u` became, if one went to it.
    async fn take(
        &self,
        u: &Value,
        tx: &mpsc::Sender<Inbound>,
    ) -> Option<tokio::sync::oneshot::Receiver<bool>> {
        let update = match Update::deserialize(u) {
            Ok(update) => update,
            Err(e) => {
                eprintln!("zephyr: telegram update not understood: {e}");
                return None;
            }
        };
        match sort(update) {
            Sorted::Skip => None,
            Sorted::Leave(chat) => {
                let left = self.api.json("leaveChat", json!({"chat_id": chat})).await;
                match left {
                    Ok(_) => eprintln!(
                        "zephyr: telegram: left group {chat}; the bot answers private chats only"
                    ),
                    Err(e) => eprintln!("zephyr: telegram: could not leave group {chat}: {e:#}"),
                }
                None
            }
            Sorted::Row(mut row, docs, name) => {
                self.remember(&row.sender, name);
                let files = self.files(&row, docs).await;
                crate::inbox::append(&self.live, &mut row, files).await;
                let answer = (self.acks.get()).map(|a| a.expect("telegram", &row.platform_msg_id));
                // A closed receiver means run() is returning; the error comes from there.
                tx.send(row).await.ok().and(answer)
            }
        }
    }

    /// The row's files for `inbox::append`. `getFile` is called for an owner's only, so a
    /// stranger's file costs no call.
    async fn files(&self, row: &Inbound, docs: Vec<Doc>) -> Vec<File> {
        // A redelivered message is a row the router drops: no `getFile`, nothing fetched.
        if crate::inbox::stored(&self.live, row).await {
            return Vec::new();
        }
        let owner = crate::inbox::owner(&self.live, row);
        let mut out = Vec::new();
        for d in docs {
            let src = if !owner {
                Src::Skip("not downloaded")
            } else if d.size > MAX_FILE {
                Src::Skip("too large, not downloaded")
            } else {
                match self.api.json("getFile", json!({"file_id": d.id})).await {
                    Ok(f) => match f["file_path"].as_str() {
                        Some(path) => {
                            let url =
                                format!("{}/file/bot{}/{path}", self.api.base, self.api.token);
                            Src::Get(self.api.http.get(url).timeout(DOWNLOAD_TIMEOUT))
                        }
                        None => Src::Skip("download failed"),
                    },
                    Err(e) => {
                        eprintln!(
                            "zephyr: telegram attachment of {}: {e:#}",
                            row.platform_msg_id
                        );
                        Src::Skip("download failed")
                    }
                }
            };
            out.push(File {
                name: d.name,
                mime: d.mime,
                size: d.size,
                src,
            });
        }
        out
    }

    fn remember(&self, sender: &str, name: String) {
        let mut names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        crate::channel::remember(&mut names, MAX_NAMES, sender, name);
    }

    /// Ends `chat`'s run: stops typing and deletes the progress note.
    async fn finish(&self, chat: i64) {
        self.typing.stop(&chat.to_string());
        let Some(mut run) = self.runs.lock().await.remove(&chat) else {
            return;
        };
        if let Some(id) = run.progress.msg {
            let body = json!({"chat_id": chat, "message_id": id});
            if let Err(e) = self.api.json("deleteMessage", body).await {
                run.progress.warn("telegram delete", &e);
            }
        }
    }
}

impl Channel for Telegram {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        tokio::select! {
            _ = self.poll(&tx) => {}
            _ = tx.closed() => {}
        }
        anyhow::bail!("telegram adapter stopped")
    }

    /// The text in pieces of `MAX_TEXT`, plain (no `parse_mode`), then the file. shortcut:
    /// Markdown shows as typed, until a converter to Telegram's entities is worth it.
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        let chat: i64 = msg
            .recipient
            .parse()
            .context("recipient is not a Telegram user id")?;
        let parts = chunks(&msg.text, MAX_TEXT);
        let parts_total = parts.len() + usize::from(msg.attachment.is_some());
        let mut sending = (self.delivered).begin("telegram", &msg.recipient, &msg.client_id)?;
        let file = match msg
            .attachment
            .as_ref()
            .filter(|_| sending.before <= parts.len())
        {
            Some(path) => {
                let name = path
                    .file_name()
                    .map_or("file".into(), |n| n.to_string_lossy().into_owned());
                let bytes = tokio::fs::read(path)
                    .await
                    .context("reading the file to send")?;
                Some((name, bytes))
            }
            None => None,
        };
        // The progress note goes before the reply, so it never sits below it.
        self.finish(chat).await;
        // In a task of its own: the router stops waiting after its timeout, but a piece
        // Telegram took then is recorded, so no retry sends it again (`channel::Delivered`).
        let api = self.api.clone();
        let send = move |sending: crate::channel::Sending| async move {
            for (i, text) in parts.iter().enumerate().skip(sending.before) {
                let body = json!({"chat_id": chat, "text": text});
                api.json("sendMessage", body).await?;
                sending.sent(i + 1);
            }
            if let Some((name, bytes)) = file {
                // A seam for spoken replies: `voice.ogg` shows as a voice note.
                let (method, field) = match name.as_str() {
                    "voice.ogg" => ("sendVoice", "voice"),
                    _ => ("sendDocument", "document"),
                };
                api.call(method, |r| {
                    let part =
                        reqwest::multipart::Part::bytes(bytes.clone()).file_name(name.clone());
                    let form = reqwest::multipart::Form::new()
                        .text("chat_id", chat.to_string())
                        .part(field, part);
                    r.multipart(form)
                })
                .await?;
                sending.sent(parts.len() + 1);
            }
            anyhow::Ok(())
        };
        sending.total = parts_total;
        crate::channel::detach(sending, self.send_cap, send).await
    }

    fn live(&self, cfg: tokio::sync::watch::Receiver<Arc<crate::config::Config>>) {
        let _ = self.live.set(cfg);
    }

    fn acks(&self, acks: Arc<crate::channel::Acks>) {
        let _ = self.acks.set(acks);
    }

    fn known_as(&self, sender: &str) -> Option<String> {
        let names = self.names.lock().unwrap_or_else(|e| e.into_inner());
        names.get(sender).cloned()
    }

    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        let Ok(chat) = to.parse::<i64>() else {
            eprintln!("zephyr: telegram activity for {to:?}, which is not a user id");
            return Ok(());
        };
        match a {
            Activity::Start => {
                let mut runs = self.runs.lock().await;
                if runs.contains_key(&chat) {
                    return Ok(());
                }
                let (api, warned) = (self.api.clone(), self.typing_warned.clone());
                self.typing.start(&chat.to_string(), async move {
                    loop {
                        let body = json!({"chat_id": chat, "action": "typing"});
                        if let Err(e) = api.json("sendChatAction", body).await {
                            if !warned.swap(true, Ordering::Relaxed) {
                                eprintln!("zephyr: telegram typing failed, later typing errors are not logged: {e:#}");
                            }
                        }
                        tokio::time::sleep(TYPING_EVERY).await;
                    }
                });
                runs.insert(chat, Run::default());
            }
            Activity::Progress { elapsed, tools, .. } => {
                let mut runs = self.runs.lock().await;
                let Some(run) = runs.get_mut(&chat) else {
                    return Ok(());
                };
                let step = run
                    .progress
                    .step(self.progress_after, *elapsed, Instant::now());
                let text = progress_text(*elapsed, tools);
                match step {
                    Step::Wait => {}
                    Step::Post => {
                        let body = json!({"chat_id": chat, "text": text});
                        match self.api.json("sendMessage", body).await {
                            Ok(m) => run.progress.msg = m["message_id"].as_i64(),
                            Err(e) => run.progress.warn("telegram post", &e),
                        }
                    }
                    Step::Edit(id) => {
                        let body = json!({"chat_id": chat, "message_id": id, "text": text});
                        if let Err(e) = self.api.json("editMessageText", body).await {
                            run.progress.warn("telegram edit", &e);
                        }
                    }
                }
            }
            Activity::Stop => self.finish(chat).await,
        }
        Ok(())
    }
}

/// The parts of a Bot API `Update` this adapter reads.
#[derive(Deserialize)]
struct Update {
    message: Option<Message>,
    my_chat_member: Option<MemberUpdate>,
}

#[derive(Deserialize)]
struct Message {
    message_id: i64,
    from: Option<User>,
    chat: Chat,
    text: Option<String>,
    caption: Option<String>,
    photo: Option<Vec<Media>>,
    document: Option<Media>,
    audio: Option<Media>,
    voice: Option<Media>,
    video: Option<Media>,
    video_note: Option<Media>,
}

#[derive(Deserialize)]
struct User {
    id: i64,
    is_bot: bool,
    first_name: String,
    last_name: Option<String>,
    username: Option<String>,
}

#[derive(Deserialize)]
struct Chat {
    id: i64,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct MemberUpdate {
    chat: Chat,
    new_chat_member: Member,
}

#[derive(Deserialize)]
struct Member {
    status: String,
}

/// A file or a photo size.
#[derive(Deserialize)]
struct Media {
    file_id: String,
    file_size: Option<u64>,
    file_name: Option<String>,
    mime_type: Option<String>,
    width: Option<u64>,
    height: Option<u64>,
}

/// An attachment as the message describes it, before `getFile`.
#[derive(Debug, PartialEq)]
struct Doc {
    id: String,
    name: String,
    mime: Option<String>,
    size: u64,
}

#[derive(Debug, PartialEq)]
enum Sorted {
    /// A row, its files, and how the sender named themselves.
    Row(Inbound, Vec<Doc>, String),
    /// A group, supergroup or channel to leave.
    Leave(i64),
    Skip,
}

const GROUPS: [&str; 3] = ["group", "supergroup", "channel"];

/// What an update is to the adapter. Only a private chat's own user, not a bot, makes a row;
/// a group is left whether the bot was added to it or a message from it came.
fn sort(u: Update) -> Sorted {
    if let Some(m) = u.my_chat_member {
        let gone = ["left", "kicked"].contains(&m.new_chat_member.status.as_str());
        if GROUPS.contains(&m.chat.kind.as_str()) && !gone {
            return Sorted::Leave(m.chat.id);
        }
        return Sorted::Skip;
    }
    let Some(m) = u.message else {
        return Sorted::Skip;
    };
    if GROUPS.contains(&m.chat.kind.as_str()) {
        return Sorted::Leave(m.chat.id);
    }
    let private = m.chat.kind == "private";
    let Some(from) = m.from.filter(|f| private && f.id == m.chat.id && !f.is_bot) else {
        return Sorted::Skip;
    };
    let mut docs = Vec::new();
    // Telegram lists a photo's sizes; the largest is the photo.
    let photo = m.photo.into_iter().flatten();
    if let Some(p) = photo.max_by_key(|p| p.width.unwrap_or(0) * p.height.unwrap_or(0)) {
        docs.push(doc(p, "photo.jpg", Some("image/jpeg")));
    }
    for (media, name, mime) in [
        (m.document, "document", None),
        (m.audio, "audio", None),
        (m.voice, "voice.ogg", Some("audio/ogg")),
        (m.video, "video.mp4", Some("video/mp4")),
        (m.video_note, "video_note.mp4", Some("video/mp4")),
    ] {
        docs.extend(media.map(|f| doc(f, name, mime)));
    }
    let text = m.text.or(m.caption).unwrap_or_default();
    // Telegram sends `/start` when a chat opens.
    if docs.is_empty() && (text.is_empty() || text.trim() == "/start") {
        return Sorted::Skip;
    }
    let row = Inbound {
        channel: "telegram".into(),
        platform_msg_id: format!("{}-{}", m.chat.id, m.message_id),
        sender: from.id.to_string(),
        text,
        derived: None,
        sent_at: None,
    };
    Sorted::Row(row, docs, name(&from))
}

fn doc(f: Media, name: &str, mime: Option<&str>) -> Doc {
    Doc {
        id: f.file_id,
        name: f.file_name.unwrap_or_else(|| name.into()),
        mime: f.mime_type.or(mime.map(Into::into)),
        size: f.file_size.unwrap_or(0),
    }
}

/// `"Aiden Yoon", @aiden`: quoted with escapes, so a name cannot break the notice's line.
fn name(u: &User) -> String {
    let full = match &u.last_name {
        Some(last) => format!("{} {last}", u.first_name),
        None => u.first_name.clone(),
    };
    match &u.username {
        Some(user) => format!("{full:?}, @{user}"),
        None => format!("{full:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(v: Value) -> Sorted {
        sort(serde_json::from_value(v).unwrap())
    }

    fn private(from: Value, extra: Value) -> Value {
        let mut m = json!({"message_id": 7, "from": from, "chat": {"id": from["id"], "type": "private"}, "date": 0});
        m.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"update_id": 1, "message": m})
    }

    fn aiden() -> Value {
        json!({"id": 55, "is_bot": false, "first_name": "Aiden", "username": "aiden"})
    }

    #[test]
    fn a_private_message_is_a_row_keyed_by_chat_and_message() {
        let Sorted::Row(row, docs, name) = sorted(private(aiden(), json!({"text": "hi"}))) else {
            panic!("a row");
        };
        assert_eq!(
            row,
            Inbound {
                channel: "telegram".into(),
                platform_msg_id: "55-7".into(),
                sender: "55".into(),
                text: "hi".into(),
                derived: None,
                sent_at: None,
            }
        );
        assert!(docs.is_empty());
        assert_eq!(name, r#""Aiden", @aiden"#);
    }

    #[test]
    fn bots_other_chats_empty_messages_and_start_are_not_rows() {
        let mut bot = aiden();
        bot["is_bot"] = json!(true);
        assert_eq!(sorted(private(bot, json!({"text": "hi"}))), Sorted::Skip);
        let mut other = private(aiden(), json!({"text": "hi"}));
        other["message"]["chat"]["id"] = json!(999);
        assert_eq!(sorted(other), Sorted::Skip);
        assert_eq!(sorted(private(aiden(), json!({}))), Sorted::Skip);
        assert_eq!(
            sorted(private(aiden(), json!({"text": "/start"}))),
            Sorted::Skip
        );
        assert!(matches!(
            sorted(private(aiden(), json!({"text": "/start now"}))),
            Sorted::Row(..)
        ));
        assert_eq!(sorted(json!({"update_id": 3})), Sorted::Skip);
    }

    #[test]
    fn groups_are_left_unless_the_bot_is_already_out() {
        let member = |kind: &str, status: &str| {
            json!({"update_id": 1, "my_chat_member": {"chat": {"id": -5, "type": kind},
                "new_chat_member": {"status": status}}})
        };
        assert_eq!(sorted(member("group", "member")), Sorted::Leave(-5));
        assert_eq!(
            sorted(member("supergroup", "administrator")),
            Sorted::Leave(-5)
        );
        assert_eq!(sorted(member("channel", "member")), Sorted::Leave(-5));
        assert_eq!(sorted(member("group", "left")), Sorted::Skip);
        assert_eq!(sorted(member("supergroup", "kicked")), Sorted::Skip);
        assert_eq!(sorted(member("private", "member")), Sorted::Skip);
        let mut msg = private(aiden(), json!({"text": "hi"}));
        msg["message"]["chat"] = json!({"id": -9, "type": "group"});
        assert_eq!(sorted(msg), Sorted::Leave(-9));
    }

    #[test]
    fn files_are_described_with_the_largest_photo() {
        let extra = json!({"caption": "look",
            "photo": [{"file_id": "s", "width": 90, "height": 90, "file_size": 1},
                      {"file_id": "l", "width": 900, "height": 900, "file_size": 9}],
            "voice": {"file_id": "v", "file_size": 4},
            "document": {"file_id": "d", "file_name": "a.pdf", "mime_type": "application/pdf"}});
        let Sorted::Row(row, docs, _) = sorted(private(aiden(), extra)) else {
            panic!("a row");
        };
        assert_eq!(row.text, "look");
        let doc = |id: &str, name: &str, mime: &str, size| Doc {
            id: id.into(),
            name: name.into(),
            mime: Some(mime.into()),
            size,
        };
        assert_eq!(
            docs,
            vec![
                doc("l", "photo.jpg", "image/jpeg", 9),
                doc("d", "a.pdf", "application/pdf", 0),
                doc("v", "voice.ogg", "audio/ogg", 4),
            ]
        );
        // A file alone is a row, even with no caption.
        let only = json!({"video_note": {"file_id": "n"}});
        assert!(matches!(sorted(private(aiden(), only)), Sorted::Row(..)));
    }

    #[test]
    fn names_are_quoted_and_escaped() {
        let user: User = serde_json::from_value(
            json!({"id": 1, "is_bot": false, "first_name": "A\"b\nc", "last_name": "Yoon"}),
        )
        .unwrap();
        assert_eq!(name(&user), r#""A\"b\nc Yoon""#);
    }

    fn adapter() -> Telegram {
        std::env::set_var("ZEPHYR_TELEGRAM_UNIT_TOKEN", "1:unit-secret");
        let cfg: Config = toml::from_str("token_env = 'ZEPHYR_TELEGRAM_UNIT_TOKEN'").unwrap();
        Telegram::new(&cfg, "http://127.0.0.1:1").unwrap()
    }

    #[test]
    fn known_as_is_the_last_name_a_sender_used() {
        let tg = adapter();
        assert_eq!(tg.known_as("55"), None);
        tg.remember("55", r#""Aiden""#.into());
        tg.remember("55", r#""Aiden", @aiden"#.into());
        assert_eq!(tg.known_as("55").as_deref(), Some(r#""Aiden", @aiden"#));
        for i in 0..MAX_NAMES {
            tg.remember(&i.to_string(), "x".into());
        }
        assert!(tg.names.lock().unwrap().len() <= MAX_NAMES);
    }

    #[test]
    fn the_token_is_redacted_whole_or_in_part() {
        let tg = adapter();
        assert_eq!(
            tg.api.redact("at /bot1:unit-secret/x and unit-secret"),
            "at /bot[redacted]/x and [redacted]"
        );
    }

    #[test]
    fn replies_are_cut_at_telegrams_limit() {
        let pieces = chunks(&"x".repeat(5000), MAX_TEXT);
        assert_eq!(
            pieces.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![4096, 904]
        );
    }
}
