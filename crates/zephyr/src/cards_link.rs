//! Stripe's Link Agent Wallet as a card issuer (DESIGN §4.22).

use crate::cards::{Caps, CardSpec, CardState, Issued, Issuer, Pan, ProviderError};
use crate::config::Config;
use crate::store::Card;

/// Link's API (https://docs.link.com/: "plain HTTP requests" to `api.link.com`).
pub const API: &str = "https://api.link.com";
/// Link's sign-in host (https://docs.stripe.com/agentic-commerce/agents/link-agent-wallet/oauth).
pub const AUTH: &str = "https://login.link.com";

pub struct Link;

impl Link {
    /// Single use by default and approved by the owner in Link
    /// (https://support.link.com/questions/link-agent-wallet-faq); no merchant lock is stated.
    pub const CAPS: Caps = Caps {
        single_use: true,
        asks_owner: true,
        charges: true,
        currencies: None,
    };

    pub fn load(_: &Config) -> Result<Link, String> {
        Err(crate::cards::NOT_SET_UP.into())
    }
}

impl Issuer for Link {
    fn name(&self) -> &'static str {
        "link"
    }
    fn can(&self) -> Caps {
        Self::CAPS
    }
    async fn issue(&self, _: &CardSpec) -> Result<Issued, ProviderError> {
        Err(ProviderError(crate::cards::NOT_SET_UP.into()))
    }
    async fn reveal(&self, _: &Card) -> Result<Pan, ProviderError> {
        Err(ProviderError(crate::cards::NOT_SET_UP.into()))
    }
    async fn state(&self, _: &Card) -> Result<CardState, ProviderError> {
        Err(ProviderError(crate::cards::NOT_SET_UP.into()))
    }
    async fn close(&self, _: &Card) -> Result<(), ProviderError> {
        Err(ProviderError(crate::cards::NOT_SET_UP.into()))
    }
}
