//! Virtual cards (DESIGN §4.22): the agent asks for a card for one checkout, the gate holds it as
//! a payment, the daemon issues it through the owner's provider and fills it only on the
//! checkout's origin. The number lives in memory only and never reaches the agent's text.

use crate::config::Config;

/// What `cards.provider` may be set to.
pub const PROVIDERS: [&str; 2] = ["manual", "link"];
/// `cards.max` when unset, in `approvals.currency`.
pub const MAX: f64 = 100.0;

/// `cards.provider`: `None` when cards are off.
pub fn provider(cfg: &Config) -> Option<&str> {
    cfg.cards.as_ref()?.provider.as_deref()
}

/// The most one card may be for, in the minor unit of `approvals.currency`.
pub fn max_minor(cfg: &Config) -> i64 {
    let max = cfg.cards.as_ref().and_then(|c| c.max).unwrap_or(MAX);
    crate::approvals::minor_of(max, &crate::approvals::currency(cfg))
}

/// How long an issued card stays open.
pub fn minutes(cfg: &Config) -> u32 {
    cfg.cards.as_ref().and_then(|c| c.minutes).unwrap_or(15)
}
