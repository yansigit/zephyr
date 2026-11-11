//! The contract between chat adapters and the router.

use serde::{Deserialize, Serialize};

/// A message received from a channel. `platform_msg_id` is the dedupe key in the inbox.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Inbound {
    pub channel: String,
    pub platform_msg_id: String,
    pub sender: String,
    pub text: String,
}

/// A reply to send. `client_id` lets the platform drop a retried duplicate.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Outbound {
    pub channel: String,
    pub recipient: String,
    pub client_id: String,
    pub text: String,
    /// A file to send with `text` (the agent's `send_file`, DESIGN §4.11).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachment: Option<std::path::PathBuf>,
    /// The `platform_msg_id` of the inbound message this answers; `None` for a notice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
}

/// What the agent is doing for a recipient, so a channel can show it (typing, a progress note).
#[derive(Debug, Clone, PartialEq)]
pub enum Activity {
    Start,
    /// `tools` holds tool names in call order; `current` is the one running now, if any.
    Progress {
        elapsed: std::time::Duration,
        tools: Vec<String>,
        current: Option<String>,
    },
    Stop,
}

/// A chat adapter. `run` pushes inbound messages into `tx` until the adapter stops.
#[allow(async_fn_in_trait)]
pub trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, tx: tokio::sync::mpsc::Sender<Inbound>) -> anyhow::Result<()>;
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()>;
    /// Shows `a` to `to`, the id `send` uses as `recipient`. Best effort; the default shows nothing.
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        let _ = (to, a);
        Ok(())
    }
    /// Hands over the router's live config, once before `run`: an adapter that fetches files
    /// reads the owner list from it (`inbox`). The default ignores it.
    fn live(&self, cfg: tokio::sync::watch::Receiver<std::sync::Arc<crate::config::Config>>) {
        let _ = cfg;
    }
    /// How `sender` last named themselves here (`"Aiden", @aiden`), for the pairing notice only:
    /// never an identity. The default knows no names.
    fn known_as(&self, sender: &str) -> Option<String> {
        let _ = sender;
        None
    }
    /// Hands over the router's answers for the rows the adapter sends it, once before `run`:
    /// an adapter that confirms a message to its platform (Telegram) waits for its row's. The
    /// default ignores them.
    fn acks(&self, acks: std::sync::Arc<Acks>) {
        let _ = acks;
    }
}

/// The router's answer for each row an adapter handed it, given as soon as the row is stored
/// (or deliberately left out: a text of spaces, a duplicate): `true`, and the adapter may
/// confirm it to its platform; `false` when it could not be stored (a write error, a panic),
/// and the platform must give it again. `Answer` gives `false` if dropped unanswered.
#[derive(Default)]
pub struct Acks(
    std::sync::Mutex<
        std::collections::HashMap<(String, String), tokio::sync::oneshot::Sender<bool>>,
    >,
);

impl Acks {
    /// Registers `channel`'s row `id` before it is sent: the receiver answers once the router
    /// is done with it.
    pub fn expect(&self, channel: &str, id: &str) -> tokio::sync::oneshot::Receiver<bool> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        all.insert((channel.into(), id.into()), tx);
        rx
    }

    /// The router is done with `channel`'s row `id`: `stored`, or not. Nothing waits on most.
    pub fn done(&self, channel: &str, id: &str, stored: bool) {
        let mut all = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = all.remove(&(channel.to_string(), id.to_string())) {
            let _ = tx.send(stored);
        }
    }
}

/// A row's answer, from its arrival in `Router::inbound`: `stored` once it is, and "not stored"
/// if it is dropped unanswered (an error or a panic before the write).
pub struct Answer(Option<(std::sync::Arc<Acks>, String, String)>);

impl Answer {
    pub fn new(acks: &std::sync::Arc<Acks>, m: &Inbound) -> Self {
        Self(Some((
            acks.clone(),
            m.channel.clone(),
            m.platform_msg_id.clone(),
        )))
    }

    /// The row is stored, or deliberately left out: the platform may forget it.
    pub fn stored(mut self) {
        if let Some((acks, channel, id)) = self.0.take() {
            acks.done(&channel, &id, true);
        }
    }
}

impl Drop for Answer {
    fn drop(&mut self) {
        if let Some((acks, channel, id)) = self.0.take() {
            acks.done(&channel, &id, false);
        }
    }
}

/// Splits `text` into pieces of at most `max` UTF-16 units, breaking after newlines where possible.
/// UTF-16 is counted because it is never smaller than the code-point count, so a piece that fits
/// here fits whichever way a platform counts (Discord, Telegram, Signal). shortcut: a line longer than `max` is hard-split by char.
pub fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0;
    for line in text.split_inclusive('\n') {
        let line_len: usize = line.encode_utf16().count();
        if cur_len > 0 && cur_len + line_len > max {
            out.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        for ch in line.chars() {
            let n = ch.len_utf16();
            if cur_len + n > max {
                out.push(std::mem::take(&mut cur));
                cur_len = 0;
            }
            cur.push(ch);
            cur_len += n;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// What a channel with no client nonce (Telegram, Signal) delivered of each reply, so a retry
/// starts after it. The router stops waiting on a send after its timeout, but the platform may
/// still take what was asked: so a send runs in a task of its own (`detach`; `Sending` moves into
/// it) and records each piece the platform took. While it goes, a retry of that reply answers
/// `StillGoing` rather than run beside it, and the router neither fails the row nor drops its
/// file. A send has `SEND_CAP` in all (429 waits too); its error, if the router never saw it, is
/// what the next retry answers, so the row then takes the normal failure path. Replies to one
/// chat take turns, and one left part-sent keeps its chat's turn: later replies to that chat
/// answer `StillGoing` until it is done or its retry window lapses, so replies never interleave.
/// Kept for the router's `retry_window` of the channel once no send of it goes.
#[derive(Default)]
pub struct Delivered {
    sent: std::sync::Mutex<std::collections::HashMap<String, Sent>>,
    /// One turn per `(channel, chat)`: a fair lock, so later replies queue behind the first.
    turns:
        std::sync::Mutex<std::collections::HashMap<String, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// Per `(channel, chat)`, the reply left part-sent, and when: it goes on first.
    owed: std::sync::Mutex<std::collections::HashMap<String, (String, std::time::Instant)>>,
}

struct Sent {
    pieces: usize,
    at: std::time::Instant,
    going: bool,
    /// The error of a send the router stopped waiting for: the next retry answers it.
    failed: Option<String>,
}

/// A send of this reply is still going in a task of its own: not a failure. The router keeps
/// the row pending, whatever its window, and records the true result at a later retry.
#[derive(Debug)]
pub struct StillGoing;

impl std::fmt::Display for StillGoing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a send of this reply is still going")
    }
}

impl std::error::Error for StillGoing {}

/// The channel's account is held for a while (Signal's setup re-registering): the reply waits,
/// pending, as long as the hold's lease runs, whatever its window.
#[derive(Debug)]
pub struct Paused;

impl std::fmt::Display for Paused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the channel is paused while its account is set up")
    }
}

impl std::error::Error for Paused {}

/// How long `detach` waits for its task before it answers `StillGoing`; well inside the
/// router's own send timeout, so the router never cuts one off itself.
const DETACH_AFTER: std::time::Duration = std::time::Duration::from_secs(3);
/// The most one send may take, its waits for a rate limit included.
pub const SEND_CAP: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// One send of a reply, from `Delivered::begin` until it is dropped.
pub struct Sending {
    of: std::sync::Arc<Delivered>,
    id: String,
    chat: String,
    turn: std::sync::Arc<tokio::sync::Mutex<()>>,
    /// The pieces an earlier send delivered: this one starts after them.
    pub before: usize,
    /// The reply's pieces in all (its file counts as one): fewer delivered leaves it owed.
    pub total: usize,
}

impl Delivered {
    /// Starts a send of `client_id` to `chat` on `channel`. Answers `StillGoing` while an
    /// earlier send of it goes, or while an earlier reply to `chat` is owed; the error of a send
    /// the router stopped waiting for, once.
    pub fn begin(
        self: &std::sync::Arc<Self>,
        channel: &str,
        chat: &str,
        client_id: &str,
    ) -> anyhow::Result<Sending> {
        let window = std::time::Duration::from_secs(crate::router::retry_window(channel) as u64);
        let key = format!("{channel} {chat}");
        {
            let mut owed = self.owed.lock().unwrap_or_else(|e| e.into_inner());
            owed.retain(|_, (_, at)| at.elapsed() < window);
            if owed.get(&key).is_some_and(|(id, _)| id != client_id) {
                return Err(StillGoing.into());
            }
        }
        let mut all = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        all.retain(|_, s| s.going || s.at.elapsed() < window);
        let sent = all.entry(client_id.to_string()).or_insert(Sent {
            pieces: 0,
            at: std::time::Instant::now(),
            going: false,
            failed: None,
        });
        if sent.going {
            return Err(StillGoing.into());
        }
        if let Some(e) = sent.failed.take() {
            return Err(anyhow::anyhow!(e));
        }
        sent.going = true;
        let before = sent.pieces;
        drop(all);
        let mut turns = self.turns.lock().unwrap_or_else(|e| e.into_inner());
        // A chat's lock goes once no send holds or waits for it.
        turns.retain(|_, t| std::sync::Arc::strong_count(t) > 1);
        let turn = turns.entry(key.clone()).or_default().clone();
        Ok(Sending {
            of: self.clone(),
            id: client_id.to_string(),
            chat: key,
            turn,
            before,
            total: 0,
        })
    }
}

impl Sending {
    /// The platform took the first `pieces` pieces.
    pub fn sent(&self, pieces: usize) {
        let mut all = self.of.sent.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(s) = all.get_mut(&self.id) {
            (s.pieces, s.at) = (pieces, std::time::Instant::now());
        }
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        let pieces = {
            let mut all = self.of.sent.lock().unwrap_or_else(|e| e.into_inner());
            let Some(s) = all.get_mut(&self.id) else {
                return;
            };
            (s.going, s.at) = (false, std::time::Instant::now());
            s.pieces
        };
        let mut owed = self.of.owed.lock().unwrap_or_else(|e| e.into_inner());
        if pieces < self.total {
            owed.insert(
                self.chat.clone(),
                (self.id.clone(), std::time::Instant::now()),
            );
        } else if owed.get(&self.chat).is_some_and(|(id, _)| *id == self.id) {
            owed.remove(&self.chat);
        }
    }
}

/// Runs `send` in a task of its own, in its chat's turn, holding `sending` until it ends, for at
/// most `cap`. Waits for it at most `DETACH_AFTER`, then answers `StillGoing` while it carries
/// on; an error it ends with after that is kept for the next retry to answer.
pub async fn detach<T, F, Fut>(
    sending: Sending,
    cap: std::time::Duration,
    send: F,
) -> anyhow::Result<T>
where
    T: Send + 'static,
    F: FnOnce(Sending) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = anyhow::Result<T>> + Send,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let (of, id) = (sending.of.clone(), sending.id.clone());
    tokio::spawn(async move {
        let turn = sending.turn.clone();
        let _turn = turn.lock_owned().await;
        let ended = tokio::time::timeout(cap, send(sending)).await;
        let ended = ended.unwrap_or_else(|_| {
            Err(anyhow::anyhow!(
                "the send took over {} min; it is given up",
                cap.as_secs_f64() / 60.0
            ))
        });
        // Nobody waits for it any more: its error is the next retry's answer.
        if let Err(Err(e)) = tx.send(ended) {
            let mut all = of.sent.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(s) = all.get_mut(&id) {
                s.failed = Some(format!("{e:#}"));
            }
        }
    });
    match tokio::time::timeout(DETACH_AFTER, rx).await {
        Ok(Ok(done)) => done,
        Ok(Err(_)) => Err(anyhow::anyhow!("the send's task ended without a result")),
        Err(_) => Err(StillGoing.into()),
    }
}

/// The adapters `zephyr run` can build. `Channel` is not dyn-compatible, so this enum is the list's element type.
pub enum AnyChannel {
    Chat(crate::chat::ChatChannel),
    Discord(crate::discord::Discord),
    IMessage(crate::imessage::IMessage),
    Telegram(crate::telegram::Telegram),
    Signal(crate::signal::Signal),
}

impl Channel for AnyChannel {
    fn name(&self) -> &str {
        match self {
            Self::Chat(c) => c.name(),
            Self::Discord(c) => c.name(),
            Self::IMessage(c) => c.name(),
            Self::Telegram(c) => c.name(),
            Self::Signal(c) => c.name(),
        }
    }
    async fn run(&self, tx: tokio::sync::mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        match self {
            Self::Chat(c) => c.run(tx).await,
            Self::Discord(c) => c.run(tx).await,
            Self::IMessage(c) => c.run(tx).await,
            Self::Telegram(c) => c.run(tx).await,
            Self::Signal(c) => c.run(tx).await,
        }
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        match self {
            Self::Chat(c) => c.send(msg).await,
            Self::Discord(c) => c.send(msg).await,
            Self::IMessage(c) => c.send(msg).await,
            Self::Telegram(c) => c.send(msg).await,
            Self::Signal(c) => c.send(msg).await,
        }
    }
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        match self {
            Self::Chat(c) => c.activity(to, a).await,
            Self::Discord(c) => c.activity(to, a).await,
            Self::IMessage(c) => c.activity(to, a).await,
            Self::Telegram(c) => c.activity(to, a).await,
            Self::Signal(c) => c.activity(to, a).await,
        }
    }
    fn live(&self, cfg: tokio::sync::watch::Receiver<std::sync::Arc<crate::config::Config>>) {
        match self {
            Self::Chat(c) => c.live(cfg),
            Self::Discord(c) => c.live(cfg),
            Self::IMessage(c) => c.live(cfg),
            Self::Telegram(c) => c.live(cfg),
            Self::Signal(c) => c.live(cfg),
        }
    }
    fn acks(&self, acks: std::sync::Arc<Acks>) {
        match self {
            Self::Chat(c) => c.acks(acks),
            Self::Discord(c) => c.acks(acks),
            Self::IMessage(c) => c.acks(acks),
            Self::Telegram(c) => c.acks(acks),
            Self::Signal(c) => c.acks(acks),
        }
    }
    fn known_as(&self, sender: &str) -> Option<String> {
        match self {
            Self::Chat(c) => c.known_as(sender),
            Self::Discord(c) => c.known_as(sender),
            Self::IMessage(c) => c.known_as(sender),
            Self::Telegram(c) => c.known_as(sender),
            Self::Signal(c) => c.known_as(sender),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_retry_starts_after_what_went_and_waits_for_a_send_still_going() {
        let d = std::sync::Arc::new(Delivered::default());
        let first = d.begin("telegram", "55", "z1").unwrap();
        assert_eq!(first.before, 0);
        first.sent(2);
        let going = d.begin("telegram", "55", "z1").err().unwrap();
        assert!(going.downcast_ref::<StillGoing>().is_some(), "still going");
        assert_eq!(d.begin("telegram", "55", "z2").unwrap().before, 0);
        drop(first);
        let again = d.begin("telegram", "55", "z1").unwrap();
        assert_eq!(again.before, 2);
        drop(again);
        // Forgotten after the channel's retry window, as the router stops retrying then.
        let window = std::time::Duration::from_secs(crate::router::retry_window("telegram") as u64);
        if let Some(old) = std::time::Instant::now().checked_sub(window) {
            d.sent.lock().unwrap().get_mut("z1").unwrap().at = old;
            assert_eq!(d.begin("telegram", "55", "z1").unwrap().before, 0);
        }
    }

    fn units(s: &str) -> usize {
        s.encode_utf16().count()
    }

    #[test]
    fn chunks_short_text_is_one_piece() {
        assert_eq!(chunks("hello\nworld", 100), vec!["hello\nworld"]);
    }

    #[test]
    fn chunks_empty_text_sends_nothing() {
        assert!(chunks("", 10).is_empty());
    }

    #[test]
    fn chunks_break_on_line_boundaries() {
        // Each line is 10 units including its newline; two lines fit in 25, three do not.
        let text = "aaaaaaaaa\nbbbbbbbbb\nccccccccc\nddddddddd\n";
        assert_eq!(
            chunks(text, 25),
            vec!["aaaaaaaaa\nbbbbbbbbb\n", "ccccccccc\nddddddddd\n",]
        );
    }

    #[test]
    fn chunks_hard_split_a_long_line_without_newlines() {
        let text = "x".repeat(25);
        let pieces = chunks(&text, 10);
        assert_eq!(
            pieces.iter().map(|p| units(p)).collect::<Vec<_>>(),
            vec![10, 10, 5]
        );
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn chunks_never_split_a_surrogate_pair() {
        // Each emoji is 2 UTF-16 units, so a limit of 3 holds one per piece.
        let text = "😀😀😀";
        assert_eq!(chunks(text, 3), vec!["😀", "😀", "😀"]);
    }

    #[test]
    fn chunks_unicode_pieces_fit_and_rejoin() {
        let text = "가나다라마바사아자차카타파하\n한글 mixed 😀 text ".repeat(200);
        assert!(units(&text) > 2000);
        let pieces = chunks(&text, 2000);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| units(p) <= 2000));
        assert_eq!(pieces.concat(), text);
    }
}
