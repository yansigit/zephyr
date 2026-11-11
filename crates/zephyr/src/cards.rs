//! Virtual cards (DESIGN §4.22): the agent asks for a card for one checkout, the gate holds it as
//! a payment, the daemon issues it through the owner's provider and fills it only on the
//! checkout's origin. The number lives in memory only and never reaches the agent's text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::config::Config;
use crate::facts::{Fact, FactKind};
use crate::store::Card;

/// What `cards.provider` may be set to.
pub const PROVIDERS: [&str; 2] = ["manual", "link"];
/// `cards.max` when unset, in `approvals.currency`.
pub const MAX: f64 = 100.0;
/// How long a closed card's number is still scrubbed out of what the agent's commands print.
const KEEP_SECS: i64 = 3600;
/// How long a number the owner typed waits for its `/card put` to claim it.
const ENTER_SECS: i64 = 60;
/// What every card tool answers when no provider can be reached.
pub const NOT_SET_UP: &str = "not run: the card provider is not set up";

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

/// What the configured provider can do, without reaching it: `None` when cards are off.
pub fn caps(cfg: &Config) -> Option<Caps> {
    match provider(cfg)? {
        "manual" => Some(Manual::CAPS),
        "link" => Some(crate::cards_link::Link::CAPS),
        "fake" if cfg.cards_fake => Some(Fake::caps(cfg.state_dir())),
        _ => None,
    }
}

/// What a provider can bind and report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Caps {
    /// The issuer refuses a second charge.
    pub single_use: bool,
    /// The issuer asks the owner itself before the card exists (Link's approval), so zephyr
    /// asks for no `/ok` of its own.
    pub asks_owner: bool,
    /// The issuer reports charges; without it the owner is told to check their card app.
    pub charges: bool,
    /// The currencies it issues in; `None` for any.
    pub currencies: Option<&'static [&'static str]>,
}

/// What a card is bound to, as the provider is told. `origin` and `merchant` are zephyr's own
/// reading of the checkout; the agent's reason is never in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CardSpec {
    pub card: i64,
    pub cap_minor: i64,
    pub currency: String,
    pub origin: String,
    pub merchant: String,
    pub expires_at: i64,
}

/// What `issue` answers: the provider's id for the card, and where the owner approves it when
/// the issuer asks them itself.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Issued {
    pub provider_ref: String,
    pub approval_url: Option<String>,
}

/// A card's number, expiry and code. Held in memory only; `Debug` shows the last four digits.
#[derive(Clone, PartialEq, Eq)]
pub struct Pan {
    pub number: String,
    pub exp_month: u8,
    pub exp_year: u16,
    pub cvc: String,
    /// The cardholder's name, when the issuer gives one.
    pub name: Option<String>,
}

impl std::fmt::Debug for Pan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Pan(…{})", self.last4())
    }
}

impl Pan {
    /// A number as the owner types it (spaces and dashes allowed), an expiry `MM/YY` or
    /// `MM/YYYY`, and a code of 3 or 4 digits. The error never repeats what was typed.
    pub fn parse(number: &str, expiry: &str, cvc: &str) -> Result<Pan, String> {
        let digits: String = (number.chars())
            .filter(|c| !matches!(c, ' ' | '-'))
            .collect();
        let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !all_digits(&digits) || !(12..=19).contains(&digits.len()) || !luhn(&digits) {
            return Err("that is not a card number (12 to 19 digits; check for a typo)".into());
        }
        let (month, year) = expiry.trim().split_once('/').ok_or("the expiry is MM/YY")?;
        let month: u8 = month.trim().parse().map_err(|_| "the expiry is MM/YY")?;
        let year: u16 = year.trim().parse().map_err(|_| "the expiry is MM/YY")?;
        let year = if year < 100 { 2000 + year } else { year };
        if !(1..=12).contains(&month) || !(2024..=2099).contains(&year) {
            return Err("the expiry is MM/YY".into());
        }
        let cvc = cvc.trim();
        if !all_digits(cvc) || !(3..=4).contains(&cvc.len()) {
            return Err("the code is 3 or 4 digits".into());
        }
        Ok(Pan {
            number: digits,
            exp_month: month,
            exp_year: year,
            cvc: cvc.into(),
            name: None,
        })
    }

    pub fn last4(&self) -> String {
        let n = self.number.len();
        self.number[n.saturating_sub(4)..].to_string()
    }

    /// `MM/YY`, as a checkout's one expiry field takes it.
    pub fn expiry(&self) -> String {
        format!("{:02}/{:02}", self.exp_month, self.exp_year % 100)
    }
}

/// The Luhn check every card number passes: catches a mistyped digit.
fn luhn(digits: &str) -> bool {
    let sum: u32 = (digits.bytes().rev().enumerate())
        .map(|(i, b)| {
            let d = u32::from(b - b'0');
            match (i % 2, d * 2) {
                (0, _) => d,
                (_, twice) if twice > 9 => twice - 9,
                (_, twice) => twice,
            }
        })
        .sum();
    sum.is_multiple_of(10)
}

/// One charge as the issuer reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Charge {
    pub minor: i64,
    pub currency: String,
    /// The network's acceptor id: the payee by identifier.
    pub merchant_id: Option<String>,
    pub descriptor: Option<String>,
    pub mcc: Option<String>,
}

/// Where a card is in its life, as the issuer says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CardState {
    /// The owner has not made or approved it yet.
    Waiting,
    Open,
    Used(Vec<Charge>),
    Closed,
    /// Refused, denied or expired at the issuer, in its words.
    Failed(String),
}

/// A provider's error, in words fit for the owner: never a key, a token or a number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderError(pub String);

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A card issuer. One implementation per provider; nothing else in zephyr knows which it is.
#[allow(async_fn_in_trait)]
pub trait Issuer {
    fn name(&self) -> &'static str;
    fn can(&self) -> Caps;
    async fn issue(&self, spec: &CardSpec) -> Result<Issued, ProviderError>;
    /// The number, fetched at fill time and held in memory only.
    async fn reveal(&self, card: &Card) -> Result<Pan, ProviderError>;
    async fn state(&self, card: &Card) -> Result<CardState, ProviderError>;
    async fn close(&self, card: &Card) -> Result<(), ProviderError>;
}

pub enum AnyIssuer {
    Manual(Manual),
    Fake(Fake),
    Link(crate::cards_link::Link),
}

/// The issuer `cards.provider` names, or why there is none.
pub fn issuer(cfg: &Config) -> Result<AnyIssuer, String> {
    let state = cfg.state_dir().to_path_buf();
    match provider(cfg) {
        Some("manual") => Ok(AnyIssuer::Manual(Manual { state })),
        Some("fake") if cfg.cards_fake => Ok(AnyIssuer::Fake(Fake { state })),
        Some("link") => crate::cards_link::Link::load(cfg).map(AnyIssuer::Link),
        _ => Err(NOT_SET_UP.into()),
    }
}

impl Issuer for AnyIssuer {
    fn name(&self) -> &'static str {
        match self {
            Self::Manual(i) => i.name(),
            Self::Fake(i) => i.name(),
            Self::Link(i) => i.name(),
        }
    }
    fn can(&self) -> Caps {
        match self {
            Self::Manual(i) => i.can(),
            Self::Fake(i) => i.can(),
            Self::Link(i) => i.can(),
        }
    }
    async fn issue(&self, spec: &CardSpec) -> Result<Issued, ProviderError> {
        match self {
            Self::Manual(i) => i.issue(spec).await,
            Self::Fake(i) => i.issue(spec).await,
            Self::Link(i) => i.issue(spec).await,
        }
    }
    async fn reveal(&self, card: &Card) -> Result<Pan, ProviderError> {
        match self {
            Self::Manual(i) => i.reveal(card).await,
            Self::Fake(i) => i.reveal(card).await,
            Self::Link(i) => i.reveal(card).await,
        }
    }
    async fn state(&self, card: &Card) -> Result<CardState, ProviderError> {
        match self {
            Self::Manual(i) => i.state(card).await,
            Self::Fake(i) => i.state(card).await,
            Self::Link(i) => i.state(card).await,
        }
    }
    async fn close(&self, card: &Card) -> Result<(), ProviderError> {
        match self {
            Self::Manual(i) => i.close(card).await,
            Self::Fake(i) => i.close(card).await,
            Self::Link(i) => i.close(card).await,
        }
    }
}

/// A card the owner makes in their own card app and types in on the host (`zephyr card put`).
/// There is no issuer to ask: the cap is the balance behind the card, and zephyr sees no charge.
pub struct Manual {
    state: PathBuf,
}

impl Manual {
    const CAPS: Caps = Caps {
        single_use: false,
        asks_owner: false,
        charges: false,
        currencies: None,
    };
}

impl Issuer for Manual {
    fn name(&self) -> &'static str {
        "manual"
    }
    fn can(&self) -> Caps {
        Self::CAPS
    }
    async fn issue(&self, spec: &CardSpec) -> Result<Issued, ProviderError> {
        Ok(Issued {
            provider_ref: format!("manual-{}", spec.card),
            approval_url: None,
        })
    }
    async fn reveal(&self, card: &Card) -> Result<Pan, ProviderError> {
        live::get(&self.state, card.id)
            .ok_or_else(|| ProviderError("the owner has not entered the card yet".into()))
    }
    async fn state(&self, card: &Card) -> Result<CardState, ProviderError> {
        Ok(match live::get(&self.state, card.id) {
            Some(_) => CardState::Open,
            None => CardState::Waiting,
        })
    }
    async fn close(&self, _: &Card) -> Result<(), ProviderError> {
        Ok(())
    }
}

/// What the tests' issuer plays and what it heard.
#[derive(Clone, Debug)]
pub struct FakeScript {
    /// Each call, as `issue 1 4250 CAD https://shop.example`, `reveal 1`, `state 1`, `close 1`.
    pub log: Vec<String>,
    pub state: CardState,
    pub pan: Pan,
    pub fail_issue: Option<String>,
    pub fail_close: bool,
    pub asks_owner: bool,
}

impl Default for FakeScript {
    fn default() -> Self {
        FakeScript {
            log: Vec::new(),
            state: CardState::Open,
            pan: Pan {
                number: "4000009990001984".into(),
                exp_month: 6,
                exp_year: 2029,
                cvc: "731".into(),
                name: None,
            },
            fail_issue: None,
            fail_close: false,
            asks_owner: false,
        }
    }
}

static FAKES: Mutex<Option<HashMap<PathBuf, FakeScript>>> = Mutex::new(None);

/// The tests' issuer (`provider = "fake"` with the hidden `Config::cards_fake`): it records
/// each call and plays its script, as `fake-rpi.py` does for rpi.
pub struct Fake {
    state: PathBuf,
}

impl Fake {
    /// Reads or changes the script of the gateway whose state folder is `cfg`'s.
    pub fn script<T>(cfg: &Config, f: impl FnOnce(&mut FakeScript) -> T) -> T {
        Self::with(cfg.state_dir(), f)
    }

    fn caps(state: &std::path::Path) -> Caps {
        Caps {
            single_use: true,
            asks_owner: Self::with(state, |s| s.asks_owner),
            charges: true,
            currencies: Some(&["CAD", "USD", "JPY"]),
        }
    }

    fn with<T>(state: &std::path::Path, f: impl FnOnce(&mut FakeScript) -> T) -> T {
        let mut all = FAKES.lock().unwrap_or_else(|e| e.into_inner());
        let all = all.get_or_insert_with(HashMap::new);
        f(all.entry(state.to_path_buf()).or_default())
    }
}

impl Issuer for Fake {
    fn name(&self) -> &'static str {
        "fake"
    }
    fn can(&self) -> Caps {
        Self::caps(&self.state)
    }
    async fn issue(&self, spec: &CardSpec) -> Result<Issued, ProviderError> {
        Self::with(&self.state, |s| {
            s.log.push(format!(
                "issue {} {} {} {}",
                spec.card, spec.cap_minor, spec.currency, spec.origin
            ));
            match &s.fail_issue {
                Some(why) => Err(ProviderError(why.clone())),
                None => Ok(Issued {
                    provider_ref: format!("fake-{}", spec.card),
                    approval_url: (s.asks_owner)
                        .then(|| format!("https://issuer.example/approve/{}", spec.card)),
                }),
            }
        })
    }
    async fn reveal(&self, card: &Card) -> Result<Pan, ProviderError> {
        Self::with(&self.state, |s| {
            s.log.push(format!("reveal {}", card.id));
            Ok(s.pan.clone())
        })
    }
    async fn state(&self, card: &Card) -> Result<CardState, ProviderError> {
        Self::with(&self.state, |s| {
            s.log.push(format!("state {}", card.id));
            Ok(s.state.clone())
        })
    }
    async fn close(&self, card: &Card) -> Result<(), ProviderError> {
        Self::with(&self.state, |s| {
            s.log.push(format!("close {}", card.id));
            match s.fail_close {
                true => Err(ProviderError("the issuer did not answer".into())),
                false => Ok(()),
            }
        })
    }
}

/// The numbers of the cards in use, in the daemon's memory only (DESIGN §4.22): never on disk,
/// so a restart forgets them. Process-wide and keyed by the state folder, because every scrub
/// has only the text in hand.
pub mod live {
    use super::*;

    struct Held {
        state: PathBuf,
        card: i64,
        pan: Pan,
        /// Set once the card closed: no longer filled, still scrubbed until then.
        drop_at: Option<i64>,
    }

    static LIVE: Mutex<Vec<Held>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<Held>> {
        LIVE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Keeps card `card`'s number, replacing one kept before.
    pub fn put(state: &std::path::Path, card: i64, pan: Pan) {
        let mut all = lock();
        all.retain(|h| !(h.state == state && h.card == card));
        all.push(Held {
            state: state.to_path_buf(),
            card,
            pan,
            drop_at: None,
        });
    }

    /// The number of a card still in use.
    pub fn get(state: &std::path::Path, card: i64) -> Option<Pan> {
        let all = lock();
        let held = all.iter().find(|h| h.state == state && h.card == card);
        held.filter(|h| h.drop_at.is_none()).map(|h| h.pan.clone())
    }

    /// The card closed: its number is never filled again, and is scrubbed for one hour more.
    pub fn forget(state: &std::path::Path, card: i64, now: i64) {
        for h in lock().iter_mut() {
            if h.state == state && h.card == card && h.drop_at.is_none() {
                h.drop_at = Some(now + KEEP_SECS);
            }
        }
    }

    /// Drops the numbers whose hour is over.
    pub fn sweep(now: i64) {
        lock().retain(|h| h.drop_at.is_none_or(|at| at > now));
    }

    /// Every number kept, with its card: what [`super::scrub`] hides.
    pub(super) fn all() -> Vec<(i64, Pan)> {
        lock().iter().map(|h| (h.card, h.pan.clone())).collect()
    }
}

/// `text` with every kept card number, however it is written (spaced, dashed, percent-encoded,
/// base64: `facts::scrub`'s matching), and its code where it stands as a number of its own,
/// replaced by `[card:#N]`. Called from `vault::scrub`, so every answer, audit row, card and
/// notice the daemon scrubs is covered.
/// shortcut: a command's log file is scrubbed by another process that has no numbers
/// (`vault::scrub_stream`); what the agent is shown of it is scrubbed here. Hand that process
/// the numbers if a read-back in a log on disk ever matters.
pub fn scrub(text: &str) -> String {
    let all = live::all();
    if all.is_empty() {
        return text.to_string();
    }
    let facts: Vec<Fact> = (all.iter())
        .map(|(card, pan)| Fact {
            name: format!("#{card}"),
            kind: FactKind::Digits,
            value: pan.number.clone(),
            allow: Vec::new(),
        })
        .collect();
    let label = |name: &str| format!("[card:{name}]");
    let withheld = |name: &str| format!("[withheld: it holds the number of card {name}]");
    let mut out = crate::facts::scrub_as(&facts, text, &label, &withheld);
    for (card, pan) in &all {
        out = scrub_code(&out, &pan.cvc, &format!("[card:#{card}]"));
    }
    out
}

/// `code` replaced where no digit stands beside it: a three-digit code inside a longer number
/// is that number, not the code.
fn scrub_code(text: &str, code: &str, label: &str) -> String {
    if code.is_empty() || !text.contains(code) {
        return text.to_string();
    }
    let digit = |c: Option<char>| c.is_some_and(|c| c.is_ascii_digit());
    let (mut out, mut at) = (String::new(), 0);
    for (i, _) in text.match_indices(code) {
        if i < at {
            continue;
        }
        let before = text[..i].chars().next_back();
        let after = text[i + code.len()..].chars().next();
        if digit(before) || digit(after) {
            continue;
        }
        out.push_str(&text[at..i]);
        out.push_str(label);
        at = i + code.len();
    }
    out.push_str(&text[at..]);
    out
}

static ENTERED: Mutex<Vec<(String, Pan, i64)>> = Mutex::new(Vec::new());

/// A number the owner typed on the host, kept under the id of the chat line that carried it
/// until that line's `/card put` claims it (`entered`), at most a minute. The chat channel calls
/// this, so the number is never in an inbox row.
pub fn enter(msg_id: &str, pan: Pan) {
    let now = crate::now();
    let mut all = ENTERED.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(id, _, at)| id != msg_id && *at + ENTER_SECS > now);
    all.push((msg_id.to_string(), pan, now));
}

/// Takes the number that came with chat line `msg_id`, once.
pub fn entered(msg_id: &str) -> Option<Pan> {
    let now = crate::now();
    let mut all = ENTERED.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(_, _, at)| *at + ENTER_SECS > now);
    let at = all.iter().position(|(id, ..)| id == msg_id)?;
    Some(all.remove(at).1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pan(number: &str, cvc: &str) -> Pan {
        Pan::parse(number, "06/29", cvc).unwrap()
    }

    #[test]
    fn a_pan_debugs_as_its_last_four() {
        let p = pan("4242 4242 4242 4242", "123");
        assert_eq!(format!("{p:?}"), "Pan(…4242)");
        assert_eq!(
            (p.number.as_str(), p.expiry().as_str()),
            ("4242424242424242", "06/29")
        );
        assert_eq!(
            Pan::parse("4242-4242-4242-4242", "6/2029", "1234")
                .unwrap()
                .exp_year,
            2029
        );
        for (n, e, c) in [
            ("4242424242424241", "06/29", "123"), // a typo: the Luhn check
            ("4242", "06/29", "123"),
            ("4242424242424242", "13/29", "123"),
            ("4242424242424242", "0629", "123"),
            ("4242424242424242", "06/29", "12"),
            ("4242424242424242", "06/29", "12a"),
        ] {
            let e = Pan::parse(n, e, c).unwrap_err();
            assert!(!e.contains("4242"), "the error repeats nothing typed: {e}");
        }
    }

    #[test]
    fn the_number_and_its_forms_scrub_to_the_card() {
        use base64::Engine;
        let state = crate::test_dir("cards-scrub");
        live::put(&state, 7, pan("5555 5555 5555 4444", "987"));
        let b64 = base64::engine::general_purpose::STANDARD.encode("card=5555555555554444&x");
        for (text, want) in [
            ("n 5555555555554444.", "n [card:#7]."),
            ("n 5555 5555 5555 4444.", "n [card:#7]."),
            ("n 5555-5555-5555-4444.", "n [card:#7]."),
            ("cvc 987 ok", "cvc [card:#7] ok"),
            ("port 19876 and 9870", "port 19876 and 9870"),
            ("nothing here", "nothing here"),
        ] {
            assert_eq!(scrub(text), want);
            // Every vault scrub does it too, whatever the vault holds.
            assert_eq!(crate::vault::scrub(text, &[]), want);
        }
        let hidden = scrub(&format!("body {b64}"));
        assert!(
            hidden.contains("[card:#7]") && !hidden.contains(&b64[8..20]),
            "{hidden}"
        );
        live::forget(&state, 7, -KEEP_SECS);
        live::sweep(1);
    }

    #[test]
    fn a_dropped_number_scrubs_for_an_hour_and_then_is_gone() {
        let state = crate::test_dir("cards-live");
        live::put(&state, 3, pan("6011 1111 1111 1117", "456"));
        assert_eq!(live::get(&state, 3).unwrap().last4(), "1117");
        assert!(live::get(&state, 4).is_none());
        assert!(
            live::get(&crate::test_dir("cards-other"), 3).is_none(),
            "another gateway's"
        );
        live::forget(&state, 3, 1000);
        assert!(live::get(&state, 3).is_none(), "never filled again");
        live::sweep(1000 + KEEP_SECS - 1);
        assert_eq!(scrub("6011111111111117"), "[card:#3]", "still scrubbed");
        live::sweep(1000 + KEEP_SECS);
        assert_eq!(scrub("6011111111111117"), "6011111111111117");
    }

    #[test]
    fn an_entered_number_is_taken_once() {
        enter("cli-1-1", pan("4242 4242 4242 4242", "123"));
        assert!(entered("cli-1-2").is_none());
        assert_eq!(entered("cli-1-1").unwrap().last4(), "4242");
        assert!(entered("cli-1-1").is_none());
    }

    #[tokio::test]
    async fn the_manual_issuer_waits_for_the_owners_number() {
        let state = crate::test_dir("cards-manual");
        let manual = Manual {
            state: state.clone(),
        };
        let card = Card {
            id: 2,
            ..Card::default()
        };
        assert!(!manual.can().asks_owner && !manual.can().charges);
        assert_eq!(manual.state(&card).await, Ok(CardState::Waiting));
        assert!(manual.reveal(&card).await.is_err());
        live::put(&state, 2, pan("378282246310005", "1234"));
        assert_eq!(manual.state(&card).await, Ok(CardState::Open));
        assert_eq!(manual.reveal(&card).await.unwrap().last4(), "0005");
        live::forget(&state, 2, -KEEP_SECS);
        live::sweep(1);
    }
}
