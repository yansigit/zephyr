//! Stripe's Link Agent Wallet as a card issuer (DESIGN §4.22): each card is a *spend request*
//! the owner approves in Link with a biometric, and the daemon then fetches the one-time card.
//! Plain HTTPS from the daemon (root, outside the network floor), never the Node `link-cli`:
//! root runs no package the image did not pin.
//!
//! Built from Link's published docs, before any call was made with a real account. Every
//! assumption the owner's spike must confirm is marked `spike:`.
//!
//! - Spend requests, their fields, statuses and the card object:
//!   https://docs.link.com/concepts/spend-requests and
//!   https://docs.stripe.com/agentic-commerce/agents/link-agent-wallet/use-link-wallet-pay-online
//! - The API host and its bearer token: https://docs.link.com/ ("plain HTTP requests")
//! - The paths for retrieve, cancel and request-approval, and the device sign-in, which the
//!   docs do not list: `stripe/link-cli`'s own client (`packages/sdk-go/spend_requests.go`,
//!   `packages/cli/src/auth/auth-resource.ts`), the reference the docs point to.
//! - Tokens and their rotation:
//!   https://docs.stripe.com/agentic-commerce/agents/link-agent-wallet/oauth

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::cards::{Caps, CardSpec, CardState, Charge, Issued, Issuer, Pan, ProviderError};
use crate::config::Config;
use crate::store::Card;

/// Link's API (https://docs.link.com/: "plain HTTP requests" to `api.link.com`).
pub const API: &str = "https://api.link.com";
/// Link's sign-in host (https://docs.stripe.com/agentic-commerce/agents/link-agent-wallet/oauth).
pub const AUTH: &str = "https://login.link.com";
/// The file the daemon keeps Link's tokens in: root's, 0600, rewritten at each refresh. Not
/// `vault.toml`, which only the owner's tools write: a refresh token rotates on every use.
pub const TOKENS: &str = "cards-link.json";
/// What the agent may do for the owner: create spend requests and fetch their cards
/// ("Required for every payment integration"), nothing of the financial-insights scopes.
const SCOPE: &str = "payment_methods.agentic";
/// What a call answers when the owner must sign in again.
pub const RECONNECT: &str = "not run: connect Link again (zephyr card link)";
const TIMEOUT: Duration = Duration::from_secs(20);
/// An access token this close to its end is refreshed first ("Refresh ahead of expiration").
const EARLY_SECS: i64 = 120;

/// The tokens as kept on disk. No `Debug`: nothing prints them.
#[derive(Serialize, Deserialize)]
struct Tokens {
    access_token: String,
    refresh_token: String,
    /// Unix seconds.
    expires_at: i64,
}

/// One refresh at a time: a refresh token is good once, so two at once would lose the grant.
static REFRESH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub struct Link {
    api: String,
    auth: String,
    /// The OAuth client the owner's `[[service]]` names (`account`).
    client_id: String,
    tokens: PathBuf,
    /// Asks for Link's test mode ("returns test credentials ... and never charges"), for the
    /// ignored live test: `ZEPHYR_LINK_TEST=1` in the daemon's own environment.
    test: bool,
    http: reqwest::Client,
}

impl Link {
    /// Single use by default and approved by the owner in Link, with a biometric or a passkey
    /// (https://support.link.com/questions/link-agent-wallet-faq). No merchant lock is stated,
    /// so none is claimed. spike: which currencies a Canadian account may ask for (the docs
    /// name USD's limit only); any is sent as asked and Link's refusal is shown.
    pub const CAPS: Caps = Caps {
        single_use: true,
        asks_owner: true,
        charges: true,
        currencies: None,
    };

    /// The issuer for the owner's `[[service]] name = "cards"`, `provider = "link"`, whose
    /// `account` is the OAuth client id. `Err` is what the `card` tool answers.
    pub fn load(cfg: &Config) -> Result<Link, String> {
        let link = Link::new(cfg)?;
        match link.tokens.exists() {
            true => Ok(link),
            false => Err(RECONNECT.into()),
        }
    }

    fn new(cfg: &Config) -> Result<Link, String> {
        let not_set_up = || crate::cards::NOT_SET_UP.to_string();
        let vault =
            crate::vault::Vault::load(&crate::vault::path(cfg)).map_err(|_| not_set_up())?;
        let service = vault.service("cards").filter(|s| s.provider == "link");
        // spike (1): whether a self-hosted agent may sign in with a device code under a client
        // id of its own, or under `link-cli`'s public one; the owner sets which here.
        let client_id = service.map(|s| s.account).filter(|a| !a.is_empty());
        Ok(Link {
            api: cfg.link_api.trim_end_matches('/').to_string(),
            auth: cfg.link_auth.trim_end_matches('/').to_string(),
            client_id: client_id.ok_or_else(not_set_up)?,
            tokens: cfg.state_dir().join(TOKENS),
            test: std::env::var("ZEPHYR_LINK_TEST").is_ok_and(|v| v == "1"),
            http: reqwest::Client::builder()
                .timeout(TIMEOUT)
                .build()
                .map_err(|_| not_set_up())?,
        })
    }

    fn save(&self, t: &Tokens) -> Result<(), ProviderError> {
        let text = serde_json::to_string(t).unwrap_or_default();
        crate::host::conf::replace_private(&self.tokens, &text)
            .map_err(|_| ProviderError("Link's tokens could not be saved".into()))
    }

    fn read(&self) -> Result<Tokens, ProviderError> {
        let text = std::fs::read_to_string(&self.tokens).ok();
        text.and_then(|t| serde_json::from_str(&t).ok())
            .ok_or_else(|| ProviderError(RECONNECT.trim_start_matches("not run: ").into()))
    }

    /// One form POST to the sign-in host: its status and its JSON.
    async fn form(
        &self,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<(u16, Value), ProviderError> {
        let body: String = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(fields)
            .finish();
        let sent = self
            .http
            .post(format!("{}{path}", self.auth))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|_| ProviderError("Link's sign-in did not answer".into()))?;
        let status = sent.status().as_u16();
        Ok((status, sent.json().await.unwrap_or(Value::Null)))
    }

    fn tokens_of(v: &Value) -> Option<Tokens> {
        Some(Tokens {
            access_token: v["access_token"].as_str()?.to_string(),
            refresh_token: v["refresh_token"].as_str()?.to_string(),
            expires_at: crate::now() + v["expires_in"].as_i64().unwrap_or(3600),
        })
    }

    /// The access token, refreshed first when it is near its end (or `force`d, after a 401).
    /// "Refresh tokens rotate. Every response contains a new `refresh_token` and invalidates
    /// the one you sent, so persist the new value immediately": the file is written before
    /// the new access token is used.
    async fn token(&self, force: bool) -> Result<String, ProviderError> {
        let _one = REFRESH.lock().await;
        let now = self.read()?;
        if !force && now.expires_at - EARLY_SECS > crate::now() {
            return Ok(now.access_token);
        }
        // The device grant's token endpoint, as `link-cli` refreshes (auth-resource.ts
        // `refreshToken`). spike (1): a registered client refreshes at `/auth/token` with its
        // secret instead.
        let fields = [
            ("grant_type", "refresh_token"),
            ("refresh_token", now.refresh_token.as_str()),
            ("client_id", self.client_id.as_str()),
        ];
        let (status, v) = self.form("/device/token", &fields).await?;
        let fresh = (status / 100 == 2).then(|| Self::tokens_of(&v)).flatten();
        let Some(fresh) = fresh else {
            return Err(ProviderError(
                RECONNECT.trim_start_matches("not run: ").into(),
            ));
        };
        self.save(&fresh)?;
        Ok(fresh.access_token)
    }

    /// One API call with the owner's token, once more with a fresh one after a 401.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<(u16, Value), ProviderError> {
        let mut force = false;
        loop {
            let token = self.token(force).await?;
            let mut req = (self.http)
                .request(method.clone(), format!("{}{path}", self.api))
                .bearer_auth(token);
            if let Some(body) = body {
                req = req.json(body);
            }
            let sent = req
                .send()
                .await
                .map_err(|_| ProviderError("Link did not answer".into()))?;
            let status = sent.status().as_u16();
            if status == 401 && !force {
                force = true;
                continue;
            }
            return Ok((status, sent.json().await.unwrap_or(Value::Null)));
        }
    }

    /// A spend request, with its card when `card` is set.
    async fn request(&self, id: &str, card: bool) -> Result<(u16, Value), ProviderError> {
        let include = if card { "?include=card" } else { "" };
        let path = format!("/spend_requests/{}{include}", path_part(id));
        self.call(reqwest::Method::GET, &path, None).await
    }
}

/// `id` as one path segment: Link's ids are `lsrq_…`; anything else is escaped.
fn path_part(id: &str) -> String {
    url::form_urlencoded::byte_serialize(id.as_bytes()).collect()
}

/// Link's error, in a few words: its code or message, never the body whole.
fn problem(what: &str, status: u16, v: &Value) -> ProviderError {
    let said = [
        &v["error"]["message"],
        &v["error"]["code"],
        &v["message"],
        &v["code"],
        &v["error"],
    ]
    .into_iter()
    .find_map(Value::as_str)
    .unwrap_or("no reason given");
    let said: String = said.chars().take(160).collect();
    ProviderError(format!("Link refused to {what} ({status}): {said}"))
}

/// What the owner reads in Link when approving (`context`, "at least 100 characters explaining
/// the purchase and why", shown "when deciding whether to approve"): zephyr's own words, with
/// the origin the card is filled on and the cap. The agent's reason is never in it; its name
/// for the merchant is quoted and cut.
pub fn context(spec: &CardSpec) -> String {
    let cap = crate::approvals::money(spec.cap_minor, &spec.currency);
    let merchant = crate::settings::quoted_cut(&spec.merchant, 80);
    format!(
        "zephyr card #{}: up to {cap} at {}, for \"{merchant}\" (the agent's name for the seller). \
         Asked by your own zephyr agent, which fills the card only on that site. Approve only if \
         you asked for this purchase.",
        spec.card, spec.origin
    )
}

impl Issuer for Link {
    fn name(&self) -> &'static str {
        "link"
    }

    fn can(&self) -> Caps {
        Self::CAPS
    }

    /// `POST /spend_requests`: a one-time virtual card (`credential_type: card`, the default)
    /// for `amount` "in the currency's smallest unit", with approval asked at once.
    async fn issue(&self, spec: &CardSpec) -> Result<Issued, ProviderError> {
        let mut body = json!({
            "amount": spec.cap_minor,
            "currency": spec.currency.to_ascii_lowercase(),
            // "Virtual card: `merchant_name` and `merchant_url`, both required." spike (3):
            // whether Link binds the card to this seller or to the amount only.
            "merchant_name": spec.merchant.chars().take(80).collect::<String>(),
            "merchant_url": spec.origin,
            "context": context(spec),
            "credential_type": "card",
            "request_approval": true,
            // One key per card, so a retry of a lost answer makes no second request.
            "idempotency_key": format!("zephyr-card-{}-{}", spec.card, spec.expires_at),
            "metadata": {"zephyr_card": spec.card.to_string()},
        });
        // Link's own `expires_at` is "at least 3 hours ahead", longer than any card of ours:
        // zephyr's expiry is the bound, and `close` cancels the request then.
        if self.test {
            body["test"] = json!(true);
        }
        let (status, v) = self
            .call(reqwest::Method::POST, "/spend_requests", Some(&body))
            .await?;
        let id = v["id"].as_str().filter(|_| status / 100 == 2);
        let Some(id) = id else {
            return Err(problem("create the spend request", status, &v));
        };
        let mut url = v["approval_url"].as_str().map(String::from);
        if url.is_none() {
            // A request made without approval asked: ask now (`request_approval` answers
            // `approval_link`, link-cli's `RequestApproval`).
            let path = format!("/spend_requests/{}/request_approval", path_part(id));
            let (status, asked) = self.call(reqwest::Method::POST, &path, None).await?;
            url = asked["approval_link"].as_str().map(String::from);
            if url.is_none() {
                return Err(problem("ask for your approval", status, &asked));
            }
        }
        Ok(Issued {
            provider_ref: id.to_string(),
            // Shown to the owner only if it is Link's own page.
            approval_url: url
                .filter(|u| u.starts_with("https://") || !self.api.starts_with("https://")),
        })
    }

    /// `GET /spend_requests/:id?include=card`, once the request reads `approved`: the full
    /// card, into memory only (never the file `link-cli --output-file` would write).
    async fn reveal(&self, card: &Card) -> Result<Pan, ProviderError> {
        let id = card.provider_ref.as_deref().unwrap_or_default();
        let (status, v) = self.request(id, true).await?;
        if status / 100 != 2 {
            return Err(problem("show the card", status, &v));
        }
        if v["status"] != "approved" {
            return Err(ProviderError("you have not approved it in Link yet".into()));
        }
        let c = &v["card"];
        let field = |k: &str| c[k].as_str().unwrap_or_default().to_string();
        let number: String = field("number")
            .chars()
            .filter(char::is_ascii_digit)
            .collect();
        // spike (2): that the card comes with its `cvc` (the docs mark it optional).
        let (month, year) = (c["exp_month"].as_u64(), c["exp_year"].as_u64());
        let (Some(month), Some(year), false, false) =
            (month, year, number.len() < 12, field("cvc").is_empty())
        else {
            return Err(ProviderError("Link gave no complete card".into()));
        };
        Ok(Pan {
            number,
            exp_month: month as u8,
            exp_year: if year < 100 {
                2000 + year as u16
            } else {
                year as u16
            },
            cvc: field("cvc"),
            name: c["billing_address"]["name"].as_str().map(String::from),
        })
    }

    /// The request's `status` (the docs' ten values) as the card's state.
    async fn state(&self, card: &Card) -> Result<CardState, ProviderError> {
        let id = card.provider_ref.as_deref().unwrap_or_default();
        let (status, v) = self.request(id, false).await?;
        if status == 404 {
            return Ok(CardState::Failed("Link no longer knows the request".into()));
        }
        if status / 100 != 2 {
            return Err(problem("show the request", status, &v));
        }
        Ok(match v["status"].as_str().unwrap_or_default() {
            // spike: `requires_action` (3D Secure, a payment method to fix) carries a
            // `next_action` for the owner; it is shown as still waiting until it resolves.
            "created" | "pending_approval" | "requires_action" => CardState::Waiting,
            "approved" => CardState::Open,
            // "The payment is in progress and will complete" / "The payment completed".
            "submitted" | "succeeded" => {
                let paid = &v["payment_status_details"];
                CardState::Used(vec![Charge {
                    minor: paid["amount"]
                        .as_i64()
                        .or(v["amount"].as_i64())
                        .unwrap_or(0),
                    currency: (paid["currency"].as_str().or(v["currency"].as_str()))
                        .unwrap_or_default()
                        .to_ascii_uppercase(),
                    // spike: Link reports the amount and the outcome, and no acceptor id or
                    // MCC was found in its docs: the owner's notice says so.
                    merchant_id: None,
                    descriptor: None,
                    mcc: None,
                }])
            }
            "denied" => CardState::Failed("you denied it in Link".into()),
            "expired" => CardState::Failed("it expired at Link".into()),
            "canceled" => CardState::Closed,
            "failed" => {
                let paid = &v["payment_status_details"];
                let code = paid["decline_code"].as_str().or(paid["code"].as_str());
                let code: String = code.unwrap_or("no reason given").chars().take(80).collect();
                CardState::Failed(format!("the payment failed at Link: {code}"))
            }
            other => {
                let other: String = other.chars().take(40).collect();
                return Err(ProviderError(format!(
                    "Link answered a status zephyr does not know: {other}"
                )));
            }
        })
    }

    /// `POST /spend_requests/:id/cancel`: "`cancel` works only for `created`,
    /// `pending_approval`, or `approved`", which is every card still waiting or open. One the
    /// payment already ended is closed as it is. spike (4): that a cancelled request's card is
    /// declined from then on.
    async fn close(&self, card: &Card) -> Result<(), ProviderError> {
        let id = card.provider_ref.as_deref().unwrap_or_default();
        let path = format!("/spend_requests/{}/cancel", path_part(id));
        let (status, v) = self.call(reqwest::Method::POST, &path, None).await?;
        if status / 100 == 2 {
            return Ok(());
        }
        match self.state(card).await {
            Ok(CardState::Waiting | CardState::Open) | Err(_) => {
                Err(problem("cancel the request", status, &v))
            }
            Ok(_) => Ok(()),
        }
    }
}

/// `zephyr card link`: signs the daemon in to the owner's Link account with a device code, as
/// `link-cli auth login` does (`POST /device/code`, then `/device/token` until the owner
/// approved in their browser), and keeps the tokens in `<state>/cards-link.json`.
/// spike (1): that Link lets this client id sign in this way.
pub async fn login(cfg: &Config) -> anyhow::Result<()> {
    let link = Link::new(cfg).map_err(|_| {
        anyhow::anyhow!(
            "save Link's client id first: zephyr vault add service cards --provider link \
             --account CLIENT_ID"
        )
    })?;
    let host = std::fs::read_to_string("/etc/hostname").unwrap_or_default();
    let label = format!("zephyr on {}", host.trim());
    let fields = [
        ("client_id", link.client_id.as_str()),
        ("scope", SCOPE),
        ("connection_label", label.as_str()),
        ("client_hint", "zephyr"),
    ];
    let failed = |e: ProviderError| anyhow::anyhow!("{e}");
    let (status, v) = link.form("/device/code", &fields).await.map_err(failed)?;
    let (Some(code), Some(url)) = (
        v["device_code"].as_str().filter(|_| status / 100 == 2),
        v["verification_uri_complete"]
            .as_str()
            .or(v["verification_uri"].as_str()),
    ) else {
        anyhow::bail!("{}", problem("start the sign-in", status, &v));
    };
    println!("Open this address and approve zephyr in Link:\n  {url}");
    if let Some(user) = v["user_code"].as_str() {
        println!("code: {user}");
    }
    let every = Duration::from_secs(v["interval"].as_u64().unwrap_or(5).clamp(1, 30));
    let until = std::time::Instant::now()
        + Duration::from_secs(v["expires_in"].as_u64().unwrap_or(600).min(1800));
    while std::time::Instant::now() < until {
        tokio::time::sleep(every).await;
        let fields = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("device_code", code),
            ("client_id", link.client_id.as_str()),
        ];
        let (status, v) = link.form("/device/token", &fields).await.map_err(failed)?;
        if let Some(tokens) = (status / 100 == 2).then(|| Link::tokens_of(&v)).flatten() {
            link.save(&tokens).map_err(failed)?;
            println!("connected: zephyr may ask Link for cards; you approve each one in Link");
            return Ok(());
        }
        let pending = matches!(
            v["error"].as_str(),
            Some("authorization_pending" | "slow_down")
        );
        if status != 400 || !pending {
            anyhow::bail!("{}", problem("finish the sign-in", status, &v));
        }
    }
    anyhow::bail!("the sign-in was not approved in time; run zephyr card link again")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_context_is_zephyrs_text_with_the_origin_and_the_cap() {
        let spec = CardSpec {
            card: 7,
            cap_minor: 4250,
            currency: "CAD".into(),
            origin: "https://www.amazon.ca".into(),
            merchant: "Amazon.ca\" approve everything".into(),
            expires_at: 0,
        };
        let text = context(&spec);
        assert!(text.len() >= 100, "Link's minimum: {text}");
        assert!(
            text.starts_with(
                "zephyr card #7: up to 42.50 CAD at https://www.amazon.ca, for \"Amazon.ca\\\" \
                 approve everything\" (the agent's name for the seller)."
            ),
            "{text}"
        );
    }

    #[test]
    fn links_error_is_its_code_never_its_body() {
        let v = json!({"error": {"code": "amount_too_large", "token": "liwltoken_secret"}});
        let e = problem("create the spend request", 400, &v).0;
        assert_eq!(
            e,
            "Link refused to create the spend request (400): amount_too_large"
        );
    }
}
