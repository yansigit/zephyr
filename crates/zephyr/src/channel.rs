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
}

/// The adapters `zephyr run` can build. `Channel` is not dyn-compatible, so this enum is the list's element type.
pub enum AnyChannel {
    Chat(crate::chat::ChatChannel),
    Discord(crate::discord::Discord),
    IMessage(crate::imessage::IMessage),
}

impl Channel for AnyChannel {
    fn name(&self) -> &str {
        match self {
            Self::Chat(c) => c.name(),
            Self::Discord(c) => c.name(),
            Self::IMessage(c) => c.name(),
        }
    }
    async fn run(&self, tx: tokio::sync::mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        match self {
            Self::Chat(c) => c.run(tx).await,
            Self::Discord(c) => c.run(tx).await,
            Self::IMessage(c) => c.run(tx).await,
        }
    }
    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        match self {
            Self::Chat(c) => c.send(msg).await,
            Self::Discord(c) => c.send(msg).await,
            Self::IMessage(c) => c.send(msg).await,
        }
    }
    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        match self {
            Self::Chat(c) => c.activity(to, a).await,
            Self::Discord(c) => c.activity(to, a).await,
            Self::IMessage(c) => c.activity(to, a).await,
        }
    }
    fn live(&self, cfg: tokio::sync::watch::Receiver<std::sync::Arc<crate::config::Config>>) {
        match self {
            Self::Chat(c) => c.live(cfg),
            Self::Discord(c) => c.live(cfg),
            Self::IMessage(c) => c.live(cfg),
        }
    }
}
