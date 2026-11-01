//! Discord channel adapter for Zephyr: DMs only. See DESIGN.md section 4.3.

use crate::channel::{Activity, Channel, Inbound, Outbound};
use anyhow::Context as _;
use serde::Deserialize;
use serenity::async_trait;
use serenity::builder::{CreateAttachment, CreateMessage, EditMessage};
use serenity::client::{Client, Context, EventHandler};
use serenity::http::{Http, Typing};
use serenity::model::channel::{Message, Nonce};
use serenity::model::gateway::GatewayIntents;
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

/// Discord's per-message content limit.
pub const MAX_CONTENT: usize = 2000;
/// Discord's `nonce` limit: "up to 25 characters" (Create Message, REST docs).
pub const MAX_NONCE: usize = 25;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Name of the env var that holds the bot token. The token is never stored here or logged.
    pub token_env: String,
    /// Post a progress note while a run is still going after `progress_after_secs`.
    #[serde(default = "yes")]
    pub progress: bool,
    #[serde(default = "default_progress_after_secs")]
    pub progress_after_secs: u64,
}

fn yes() -> bool {
    true
}

fn default_progress_after_secs() -> u64 {
    20
}

/// The progress note is edited at most this often.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(5);
/// Longest progress note; a longer tool list is cut with an ellipsis.
pub const MAX_PROGRESS: usize = 120;

/// The progress note, e.g. `≋ working · 0:24 · bash, read ×3`: tools in order of first use,
/// repeats collapsed.
pub fn progress_text(elapsed: Duration, tools: &[String]) -> String {
    let s = elapsed.as_secs();
    let mut text = format!("≋ working · {}:{:02}", s / 60, s % 60);
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for t in tools {
        match counts.iter_mut().find(|(name, _)| name == t) {
            Some((_, n)) => *n += 1,
            None => counts.push((t, 1)),
        }
    }
    for (i, (name, n)) in counts.iter().enumerate() {
        text += if i == 0 { " · " } else { ", " };
        text += name;
        if *n > 1 {
            text += &format!(" ×{n}");
        }
    }
    if text.chars().count() > MAX_PROGRESS {
        text = text.chars().take(MAX_PROGRESS - 1).chain(['…']).collect();
    }
    text
}

/// What a `Progress` tick does to the progress note.
#[derive(Debug, PartialEq)]
pub enum Step {
    Wait,
    Post,
    Edit(MessageId),
}

/// One run's progress note. A failed post is not retried and failures are logged once.
#[derive(Debug, Default)]
pub struct Progress {
    tried: bool,
    msg: Option<MessageId>,
    at: Option<Instant>,
    warned: bool,
}

impl Progress {
    /// The step for a tick at `now`, `elapsed` into the run. `after` is `None` when progress is off.
    pub fn step(&mut self, after: Option<Duration>, elapsed: Duration, now: Instant) -> Step {
        match self.msg {
            None if !self.tried && after.is_some_and(|a| elapsed >= a) => {
                self.tried = true;
                self.at = Some(now);
                Step::Post
            }
            Some(id)
                if self
                    .at
                    .is_none_or(|t| now.duration_since(t) >= PROGRESS_EVERY) =>
            {
                self.at = Some(now);
                Step::Edit(id)
            }
            _ => Step::Wait,
        }
    }

    fn warn(&mut self, what: &str, e: &serenity::Error) {
        if !std::mem::replace(&mut self.warned, true) {
            eprintln!("zephyr: discord progress {what} failed: {e}");
        }
    }
}

/// A recipient's run in progress: typing refreshes until `_typing` is dropped.
struct Run {
    dm: ChannelId,
    _typing: Typing,
    progress: Progress,
}

pub struct Discord {
    token: String,
    http: Arc<Http>,
    /// `None` when `progress = false`.
    progress_after: Option<Duration>,
    /// Keyed by Discord user id. A tokio mutex: `send` waits for an in-flight progress post, so it deletes it.
    runs: Mutex<HashMap<u64, Run>>,
}

impl Discord {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        let token = std::env::var(&cfg.token_env)
            .with_context(|| format!("env var {} is not set", cfg.token_env))?;
        let http = Arc::new(Http::new(&token));
        let progress_after = cfg
            .progress
            .then(|| Duration::from_secs(cfg.progress_after_secs));
        Ok(Self {
            token,
            http,
            progress_after,
            runs: Mutex::default(),
        })
    }

    /// Ends `user`'s run: stops typing and deletes the progress note.
    async fn finish(&self, user: u64) {
        let Some(mut run) = self.runs.lock().await.remove(&user) else {
            return;
        };
        if let Some(id) = run.progress.msg {
            if let Err(e) = run.dm.delete_message(&self.http, id).await {
                run.progress.warn("delete", &e);
            }
        }
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
        let mut parts = chunks(&msg.text, MAX_CONTENT)
            .into_iter()
            .enumerate()
            .map(|(i, text)| Ok((nonce(&msg.client_id, i)?, text)))
            .collect::<anyhow::Result<Vec<_>>>()?;
        // The file rides on the last message; a file without text gets a message of its own.
        let mut file = match &msg.attachment {
            Some(path) => Some(CreateAttachment::path(path).await?),
            None => None,
        };
        if parts.is_empty() && file.is_some() {
            parts.push((nonce(&msg.client_id, 0)?, String::new()));
        }
        let last = parts.len().saturating_sub(1);
        // The progress note goes before the reply, so it never sits below it.
        self.finish(user).await;
        let dm = UserId::new(user).create_dm_channel(&self.http).await?;
        for (i, (nonce, text)) in parts.into_iter().enumerate() {
            let mut builder = CreateMessage::new()
                .content(text)
                .nonce(Nonce::String(nonce))
                .enforce_nonce(true);
            if i == last {
                if let Some(file) = file.take() {
                    builder = builder.add_file(file);
                }
            }
            dm.send_message(&self.http, builder).await?;
        }
        Ok(())
    }

    async fn activity(&self, to: &str, a: &Activity) -> anyhow::Result<()> {
        let Ok(user) = to.parse::<u64>() else {
            eprintln!("zephyr: discord activity for {to:?}, which is not a user id");
            return Ok(());
        };
        match a {
            Activity::Start => {
                let mut runs = self.runs.lock().await;
                if runs.contains_key(&user) {
                    return Ok(());
                }
                // Logged once per run: without a run, Progress and Stop do nothing.
                let dm = match UserId::new(user).create_dm_channel(&self.http).await {
                    Ok(dm) => dm.id,
                    Err(e) => {
                        eprintln!("zephyr: discord activity: no DM with {user}: {e}");
                        return Ok(());
                    }
                };
                let run = Run {
                    dm,
                    _typing: dm.start_typing(&self.http),
                    progress: Progress::default(),
                };
                runs.insert(user, run);
            }
            Activity::Progress { elapsed, tools, .. } => {
                let mut runs = self.runs.lock().await;
                let Some(run) = runs.get_mut(&user) else {
                    return Ok(());
                };
                let step = run
                    .progress
                    .step(self.progress_after, *elapsed, Instant::now());
                let text = || progress_text(*elapsed, tools);
                match step {
                    Step::Wait => {}
                    Step::Post => {
                        let msg = CreateMessage::new().content(text());
                        match run.dm.send_message(&self.http, msg).await {
                            Ok(m) => run.progress.msg = Some(m.id),
                            Err(e) => run.progress.warn("post", &e),
                        }
                    }
                    Step::Edit(id) => {
                        let msg = EditMessage::new().content(text());
                        if let Err(e) = run.dm.edit_message(&self.http, id, msg).await {
                            run.progress.warn("edit", &e);
                        }
                    }
                }
            }
            Activity::Stop => self.finish(user).await,
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
            progress: true,
            progress_after_secs: 20,
        };
        let err = Discord::new(&cfg).err().expect("missing env var must fail");
        assert!(err.to_string().contains("ZEPHYR_DISCORD_TEST_UNSET_VAR"));
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn progress_text_collapses_repeats_in_first_use_order() {
        let tools = names(&["bash", "read", "read", "bash", "read"]);
        assert_eq!(
            progress_text(Duration::from_secs(24), &tools),
            "≋ working · 0:24 · bash ×2, read ×3"
        );
        assert_eq!(
            progress_text(
                Duration::from_secs(24),
                &names(&["bash", "read", "read", "read"])
            ),
            "≋ working · 0:24 · bash, read ×3"
        );
        assert_eq!(
            progress_text(Duration::from_millis(125_900), &[]),
            "≋ working · 2:05"
        );
    }

    #[test]
    fn progress_text_is_capped() {
        let tools: Vec<String> = (0..100).map(|i| format!("tool{i}")).collect();
        let text = progress_text(Duration::from_secs(5), &tools);
        assert_eq!(text.chars().count(), MAX_PROGRESS);
        assert!(text.starts_with("≋ working · 0:05 · tool0, tool1"));
        assert!(text.ends_with('…'));
    }

    #[test]
    fn progress_posts_once_after_the_threshold_then_edits_at_most_every_5s() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let after = Some(Duration::from_secs(20));
        let mut p = Progress::default();
        assert_eq!(p.step(after, Duration::from_secs(19), at(19)), Step::Wait);
        assert_eq!(p.step(after, Duration::from_secs(20), at(20)), Step::Post);
        p.msg = Some(MessageId::new(7));
        assert_eq!(p.step(after, Duration::from_secs(21), at(21)), Step::Wait);
        assert_eq!(p.step(after, Duration::from_secs(24), at(24)), Step::Wait);
        assert_eq!(
            p.step(after, Duration::from_secs(25), at(25)),
            Step::Edit(MessageId::new(7))
        );
        assert_eq!(p.step(after, Duration::from_secs(29), at(29)), Step::Wait);
        assert_eq!(
            p.step(after, Duration::from_secs(31), at(31)),
            Step::Edit(MessageId::new(7))
        );
    }

    #[test]
    fn progress_failed_post_is_not_retried_and_off_never_posts() {
        let now = Instant::now();
        let after = Some(Duration::from_secs(20));
        let mut p = Progress::default();
        assert_eq!(p.step(after, Duration::from_secs(20), now), Step::Post);
        // The post failed, so no message id was recorded.
        for s in 21..60 {
            let t = now + Duration::from_secs(s);
            assert_eq!(p.step(after, Duration::from_secs(s), t), Step::Wait);
        }
        let mut off = Progress::default();
        assert_eq!(off.step(None, Duration::from_secs(3600), now), Step::Wait);
    }

    #[test]
    fn config_progress_defaults_and_overrides() {
        let cfg: Config = toml::from_str("token_env = 'T'").unwrap();
        assert!(cfg.progress);
        assert_eq!(cfg.progress_after_secs, 20);
        let cfg: Config =
            toml::from_str("token_env = 'T'\nprogress = false\nprogress_after_secs = 5").unwrap();
        assert!(!cfg.progress);
        assert_eq!(cfg.progress_after_secs, 5);
    }
}
