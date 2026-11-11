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
