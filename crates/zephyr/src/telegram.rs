//! Telegram channel adapter: a bot in private chats, over Bot API long polling. See DESIGN.md
//! section 4.3.

/// `[telegram]` takes Discord's three keys: `token_env`, `progress`, `progress_after_secs`.
pub use crate::discord::Config;
