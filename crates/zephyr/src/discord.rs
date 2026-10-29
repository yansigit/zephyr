//! Discord channel adapter for Zephyr: DMs only. See DESIGN.md section 4.3.

use crate::channel::{Channel, Inbound, Outbound};
use anyhow::Context as _;
use serde::Deserialize;
use serenity::async_trait;
use serenity::builder::CreateMessage;
use serenity::client::{Client, Context, EventHandler};
use serenity::http::Http;
use serenity::model::channel::{Message, Nonce};
use serenity::model::gateway::GatewayIntents;
use serenity::model::id::UserId;
use tokio::sync::mpsc;

/// Discord's per-message content limit.
pub const MAX_CONTENT: usize = 2000;
/// Discord's `nonce` limit: "up to 25 characters" (Create Message, REST docs).
pub const MAX_NONCE: usize = 25;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Name of the env var that holds the bot token. The token is never stored here or logged.
    pub token_env: String,
}

pub struct Discord {
    token: String,
    http: Http,
}

impl Discord {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let token = std::env::var(&cfg.token_env)
            .with_context(|| format!("env var {} is not set", cfg.token_env))?;
        let http = Http::new(&token);
        Ok(Self { token, http })
    }
}

/// Splits `text` into pieces of at most `max` UTF-16 units, breaking after newlines where possible.
/// UTF-16 is counted because it is never smaller than the code-point count, so a piece that fits
/// here fits whichever way Discord counts. shortcut: a line longer than `max` is hard-split by char.
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

/// Nonce for chunk `index` of a reply. Stable for a given client id and chunking, so a retry
/// produces the same nonces and Discord can drop the duplicates.
pub fn nonce(client_id: &str, index: usize) -> anyhow::Result<String> {
    let n = format!("{client_id}-{index}");
    anyhow::ensure!(
        n.chars().count() <= MAX_NONCE,
        "client_id {client_id:?} with chunk index {index} exceeds Discord's {MAX_NONCE}-char nonce limit"
    );
    Ok(n)
}

/// Maps a gateway message to an inbound row. Pure so the filter rules can be tested without a gateway.
pub fn to_inbound(
    is_dm: bool,
    author_is_bot: bool,
    msg_id: u64,
    author_id: u64,
    text: &str,
) -> Option<Inbound> {
    if !is_dm || author_is_bot || text.is_empty() {
        return None;
    }
    Some(Inbound {
        channel: "discord".into(),
        platform_msg_id: msg_id.to_string(),
        sender: author_id.to_string(),
        text: text.into(),
    })
}

struct Handler {
    tx: mpsc::Sender<Inbound>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, _ctx: Context, msg: Message) {
        let inbound = to_inbound(
            msg.guild_id.is_none(),
            msg.author.bot,
            msg.id.get(),
            msg.author.id.get(),
            &msg.content,
        );
        if let Some(inbound) = inbound {
            // A closed receiver means run() is returning; the error comes from there.
            let _ = self.tx.send(inbound).await;
        }
    }
}

impl Channel for Discord {
    fn name(&self) -> &str {
        "discord"
    }

    async fn run(&self, tx: mpsc::Sender<Inbound>) -> anyhow::Result<()> {
        // DIRECT_MESSAGES only. DM content arrives without MESSAGE_CONTENT (privileged).
        let mut client = Client::builder(&self.token, GatewayIntents::DIRECT_MESSAGES)
            .event_handler(Handler { tx: tx.clone() })
            .await?;
        tokio::select! {
            r = client.start() => r?,
            _ = tx.closed() => {}
        }
        anyhow::bail!("discord adapter stopped")
    }

    async fn send(&self, msg: &Outbound) -> anyhow::Result<()> {
        let user: u64 = msg
            .recipient
            .parse()
            .context("recipient is not a Discord user id")?;
        // Build every nonce before the first send, so a bad client id cannot leave a half-sent reply.
        let parts = chunks(&msg.text, MAX_CONTENT)
            .into_iter()
            .enumerate()
            .map(|(i, text)| Ok((nonce(&msg.client_id, i)?, text)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let dm = UserId::new(user).create_dm_channel(&self.http).await?;
        for (nonce, text) in parts {
            let builder = CreateMessage::new()
                .content(text)
                .nonce(Nonce::String(nonce))
                .enforce_nonce(true);
            dm.send_message(&self.http, builder).await?;
        }
        Ok(())
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
        assert!(units(&text) > MAX_CONTENT);
        let pieces = chunks(&text, MAX_CONTENT);
        assert!(pieces.len() > 1);
        assert!(pieces.iter().all(|p| units(p) <= MAX_CONTENT));
        assert_eq!(pieces.concat(), text);
    }

    #[test]
    fn nonce_fits_within_discord_limit() {
        // 23 chars + "-0" is exactly 25.
        assert_eq!(
            nonce(&"a".repeat(23), 0).unwrap().chars().count(),
            MAX_NONCE
        );
        assert_eq!(nonce("abc", 7).unwrap(), "abc-7");
    }

    #[test]
    fn nonce_rejects_overflow() {
        // The index suffix grows with the chunk count: "-10" pushes 23 chars past 25.
        assert!(nonce(&"a".repeat(23), 10).is_err());
        assert!(nonce(&"a".repeat(25), 0).is_err());
    }

    #[test]
    fn to_inbound_maps_a_dm() {
        let got = to_inbound(true, false, 111, 222, "hi").unwrap();
        assert_eq!(
            got,
            Inbound {
                channel: "discord".into(),
                platform_msg_id: "111".into(),
                sender: "222".into(),
                text: "hi".into(),
            }
        );
    }

    #[test]
    fn to_inbound_drops_guild_bot_and_empty_messages() {
        assert!(to_inbound(false, false, 1, 2, "guild").is_none());
        assert!(to_inbound(true, true, 1, 2, "bot or own message").is_none());
        assert!(to_inbound(true, false, 1, 2, "").is_none());
    }

    #[test]
    fn new_errors_without_leaking_the_token() {
        let cfg = Config {
            token_env: "ZEPHYR_DISCORD_TEST_UNSET_VAR".into(),
        };
        let err = Discord::new(&cfg).err().expect("missing env var must fail");
        assert!(err.to_string().contains("ZEPHYR_DISCORD_TEST_UNSET_VAR"));
    }
}
