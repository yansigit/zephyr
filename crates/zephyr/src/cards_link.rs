//! Stripe's Link Agent Wallet as a card issuer (DESIGN §4.22).

/// Link's API (https://docs.link.com/: "plain HTTP requests" to `api.link.com`).
pub const API: &str = "https://api.link.com";
/// Link's sign-in host (https://docs.stripe.com/agentic-commerce/agents/link-agent-wallet/oauth).
pub const AUTH: &str = "https://login.link.com";
