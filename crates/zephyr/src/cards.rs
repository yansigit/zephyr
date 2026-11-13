//! Virtual cards (DESIGN §4.25): the agent asks for a card for one checkout, the gate holds it as
//! a payment, the daemon issues it through the owner's provider and fills it only on the
//! checkout's origin. The number lives in memory only and never reaches the agent's text.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use zephyr_policy::tools::{CardAction, CardFields};

use crate::config::Config;
use crate::facts::{Fact, FactKind};
use crate::gate::{CardAsk, Checked, Verdict};
use crate::store::{Card, NewCard, Store};

/// What `cards.provider` may be set to.
pub const PROVIDERS: [&str; 2] = ["manual", "link"];
/// `cards.max` when unset, in `approvals.currency`.
pub const MAX: f64 = 100.0;
/// How long a closed card's number is still scrubbed out of what the agent's commands print.
const KEEP_SECS: i64 = 3600;
/// How long a number the owner typed waits for its `/card put` to claim it.
const ENTER_SECS: i64 = 60;
/// The file beside the gate socket (root's, world-readable) that lists the origins of the
/// pages a card is typed into, one a line, for as long as zephyr keeps the card's number: the
/// browser wrapper refuses to read those pages back (`browser_gate`).
pub const FILLED_FILE: &str = "cards-filled";
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

/// `status`'s line: the provider, and for Link whether the daemon is signed in. Never a key.
pub fn status(cfg: &Config) -> String {
    match provider(cfg) {
        None => "cards: off".into(),
        Some("link") => match cfg.state_dir().join(crate::cards_link::TOKENS).exists() {
            true => "cards: link, connected".into(),
            false => "cards: link, not connected (zephyr card link)".into(),
        },
        Some(other) => format!("cards: {other}"),
    }
}

/// `doctor`'s lines, each `ok|warn <what>`: whether the provider can be reached at all, and
/// the currency cards count in. A key is named as saved or not, never shown.
pub fn doctor(cfg: &Config) -> Vec<String> {
    let Some(provider) = provider(cfg) else {
        return vec!["ok   cards: off".into()];
    };
    let mut lines = Vec::new();
    match provider {
        "link" => {
            let vault = crate::vault::Vault::load(&crate::vault::path(cfg)).ok();
            let service = vault
                .and_then(|v| v.service("cards"))
                .filter(|s| s.provider == "link");
            lines.push(match service.is_some_and(|s| !s.account.is_empty()) {
                true => "ok   cards: link, its client id is saved".into(),
                false => "warn cards: link has no client id: zephyr vault add service cards \
                          --provider link --account CLIENT_ID"
                    .into(),
            });
            lines.push(
                match cfg.state_dir().join(crate::cards_link::TOKENS).exists() {
                    true => "ok   cards: zephyr is signed in to Link; you approve each card there"
                        .into(),
                    false => "warn cards: not signed in to Link: zephyr card link".into(),
                },
            );
        }
        "manual" => lines.push(
            "ok   cards: manual, no key needed: you make each card and enter it with zephyr \
             card put N; zephyr takes only a single-use card or a prepaid one with at most its \
             cap on it, since the agent could read a number back from its browser"
                .into(),
        ),
        other => lines.push(format!("ok   cards: {other}")),
    }
    let currency = crate::approvals::currency(cfg);
    let set = cfg.approvals.as_ref().is_some_and(|a| a.currency.is_some());
    lines.push(match set {
        true => format!(
            "ok   cards: up to {} each, open {} min",
            crate::approvals::money(max_minor(cfg), &currency),
            minutes(cfg)
        ),
        false => "warn cards: approvals.currency is unset, so cards and their caps are in USD; \
                  set it, e.g. zephyr config set approvals.currency CAD"
            .into(),
    });
    lines
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

/// A number is wiped when its copy is dropped: the daemon keeps one in `live` and makes
/// another for each fill, and none is left in freed memory.
impl Drop for Pan {
    fn drop(&mut self) {
        crate::vault::wipe_str(&mut self.number);
        crate::vault::wipe_str(&mut self.cvc);
        if let Some(name) = &mut self.name {
            crate::vault::wipe_str(name);
        }
    }
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

/// A card the owner makes in their own card app and types in on the host (`zephyr card put`):
/// single-use, or prepaid with at most the cap on it, so its own limit bounds it. There is no
/// issuer to ask, and zephyr sees no charge.
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
    /// How long `issue` takes to answer.
    pub issue_delay: std::time::Duration,
    /// How long `close` takes to answer.
    pub close_delay: std::time::Duration,
    pub fail_close: bool,
    /// What `state` says once a close went: a charge that comes in late.
    pub state_after_close: Option<CardState>,
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
            issue_delay: std::time::Duration::ZERO,
            close_delay: std::time::Duration::ZERO,
            fail_close: false,
            state_after_close: None,
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
        let delay = Self::with(&self.state, |s| {
            s.log.push(format!(
                "issue {} {} {} {}",
                spec.card, spec.cap_minor, spec.currency, spec.origin
            ));
            s.issue_delay
        });
        tokio::time::sleep(delay).await;
        Self::with(&self.state, |s| match &s.fail_issue {
            Some(why) => Err(ProviderError(why.clone())),
            None => Ok(Issued {
                provider_ref: format!("fake-{}", spec.card),
                approval_url: (s.asks_owner)
                    .then(|| format!("https://issuer.example/approve/{}", spec.card)),
            }),
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
        let delay = Self::with(&self.state, |s| {
            s.log.push(format!("close {}", card.id));
            s.close_delay
        });
        tokio::time::sleep(delay).await;
        Self::with(&self.state, |s| match s.fail_close {
            true => Err(ProviderError("the issuer did not answer".into())),
            false => {
                if let Some(state) = s.state_after_close.take() {
                    s.state = state;
                }
                Ok(())
            }
        })
    }
}

/// The numbers of the cards in use, in the daemon's memory only (DESIGN §4.25): never on disk,
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
        /// Once the number was typed into a page: the flag file that says so, and its line,
        /// the page's origin and address ([`filled`]).
        filled: Option<(PathBuf, String)>,
    }

    static LIVE: Mutex<Vec<Held>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<Held>> {
        LIVE.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The kept numbers as the log scrubbers read them (`vault::scrub_stream`): a memfd, so
    /// still memory, close-on-exec, so no command of the agent's inherits it. `procs::spawn`
    /// hands it to the scrubber alone.
    static SHARED: Mutex<Option<std::fs::File>> = Mutex::new(None);

    /// The descriptor of the shared list, made on first use. `None` when it cannot be made:
    /// then a log is scrubbed of the vault's values only, as before.
    pub fn shared() -> Option<std::os::fd::RawFd> {
        use std::os::fd::{AsRawFd, FromRawFd};
        let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        if shared.is_none() {
            // Safety: a C string literal and a plain flag; the result is checked.
            let fd = unsafe { libc::memfd_create(c"zephyr-cards".as_ptr(), libc::MFD_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            // Safety: `fd` is ours and open.
            *shared = Some(unsafe { std::fs::File::from_raw_fd(fd) });
            drop(shared);
            publish(&lock());
            shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        }
        shared.as_ref().map(|f| f.as_raw_fd())
    }

    /// The shared list's layout: a sequence number (8 bytes, odd while the list is being
    /// written), the text's length (8 bytes), then the text. Nothing is ever truncated: a
    /// reader that finds the sequence odd, or changed while it read, reads again
    /// ([`super::read_shared`]), so it sees the list before the change or after it, whole.
    pub(super) fn write_shared(file: &std::fs::File, text: &str, seq: u64) {
        use std::os::unix::fs::FileExt;
        let put = |bytes: &[u8], at: u64| file.write_all_at(bytes, at);
        let _ = put(&(seq + 1).to_le_bytes(), 0)
            .and_then(|()| put(text.as_bytes(), 16))
            .and_then(|()| put(&(text.len() as u64).to_le_bytes(), 8))
            .and_then(|()| put(&(seq + 2).to_le_bytes(), 0));
    }

    /// Even: the sequence of the list as last written.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// Rewrites the shared list after a change: one line per kept card, its number, its code
    /// and its expiry month and year, tab separated.
    fn publish(all: &[Held]) {
        let shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(file) = shared.as_ref() else { return };
        let mut text: String = (all.iter())
            .map(|h| {
                let p = &h.pan;
                format!(
                    "{}\t{}\t{}\t{}\t{}\n",
                    h.card, p.number, p.cvc, p.exp_month, p.exp_year
                )
            })
            .collect();
        let seq = SEQ.fetch_add(2, std::sync::atomic::Ordering::SeqCst);
        write_shared(file, &text, seq);
        crate::vault::wipe_str(&mut text);
    }

    /// Keeps card `card`'s number, replacing one kept before.
    pub fn put(state: &std::path::Path, card: i64, pan: Pan) {
        let mut all = lock();
        let old = all.iter().position(|h| h.state == state && h.card == card);
        let filled = old.and_then(|i| all.remove(i).filled);
        all.push(Held {
            state: state.to_path_buf(),
            card,
            pan,
            drop_at: None,
            filled,
        });
        publish(&all);
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
        let mut all = lock();
        let gone = |h: &Held| h.drop_at.is_some_and(|at| at <= now);
        let flags: Vec<PathBuf> = (all.iter().filter(|h| gone(h)))
            .filter_map(|h| h.filled.as_ref().map(|(flag, _)| flag.clone()))
            .collect();
        let before = all.len();
        all.retain(|h| !gone(h));
        if all.len() != before {
            publish(&all);
        }
        flags.iter().for_each(|flag| write_flag(flag, &all));
    }

    /// Card `card`'s number is about to be typed into `page`, on `origin`: from now until the
    /// number is dropped, the browser wrapper refuses to read that origin's pages back, and
    /// to snapshot that page until it moves on (`FILLED_FILE`, in the daemon's public folder:
    /// root's, readable by the agent).
    pub fn filled(cfg: &Config, card: i64, origin: &str, page: &str) {
        let flag = crate::drafts::public_dir(cfg).join(FILLED_FILE);
        let mut all = lock();
        let state = cfg.state_dir();
        if let Some(h) = all.iter_mut().find(|h| h.state == state && h.card == card) {
            // A page's address holds no tab or line break, as `get url` prints it.
            let page: String = page.chars().filter(|c| !c.is_control()).collect();
            h.filled = Some((flag.clone(), format!("{origin}\t{page}")));
        }
        write_flag(&flag, &all);
    }

    /// The pages whose fields hold a kept number, `origin<TAB>page` a line; no file for none.
    fn write_flag(flag: &std::path::Path, all: &[Held]) {
        let mut origins: Vec<&str> = (all.iter())
            .filter_map(|h| h.filled.as_ref())
            .filter(|(f, _)| f == flag)
            .map(|(_, line)| line.as_str())
            .collect();
        origins.sort_unstable();
        origins.dedup();
        if origins.is_empty() {
            let _ = std::fs::remove_file(flag);
            return;
        }
        if let Some(dir) = flag.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = origins.join("\n") + "\n";
        let tmp = flag.with_extension("tmp");
        if let Err(e) = crate::host::conf::replace_durably(flag, &tmp, text.as_bytes()) {
            eprintln!("zephyr: writing {}: {e}", flag.display());
        }
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
/// A command's log is scrubbed by another process, which reads the numbers from
/// `live::shared` (`stream_pairs`).
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
    let out = crate::facts::scrub_as(&facts, text, &label, &withheld);
    let tokens: Vec<_> = (all.iter())
        .flat_map(|(card, pan)| tokens(*card, &pan.cvc, pan.exp_month, pan.exp_year))
        .collect();
    let clean = scrub_tokens(out.as_bytes(), &tokens);
    // Only ASCII was replaced by ASCII, so it is text still.
    String::from_utf8(clean).unwrap_or(out)
}

/// The shared list as a log scrubber reads it ([`live::write_shared`]'s layout): `None` when
/// it is being rewritten and stays so for 50 tries, or cannot be read; the reader then keeps
/// the list it had. Never an empty or a partial list.
pub fn read_shared(file: &std::fs::File) -> Option<String> {
    use std::os::unix::fs::FileExt;
    let word = |at: u64| -> Option<u64> {
        let mut b = [0u8; 8];
        file.read_exact_at(&mut b, at).ok()?;
        Some(u64::from_le_bytes(b))
    };
    for _ in 0..50 {
        let seq = word(0)?;
        if seq % 2 == 1 {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        }
        let len = word(8)?;
        if len > 1 << 20 {
            return None;
        }
        let mut text = vec![0u8; len as usize];
        if file.read_exact_at(&mut text, 16).is_ok() && word(0) == Some(seq) {
            return String::from_utf8(text).ok();
        }
    }
    None
}

/// The shared list's bytes for `text`, as a test hands a scrubber a list in a file.
#[cfg(test)]
pub(crate) fn shared_bytes(text: &str) -> Vec<u8> {
    let mut bytes = 2u64.to_le_bytes().to_vec();
    bytes.extend((text.len() as u64).to_le_bytes());
    bytes.extend(text.as_bytes());
    bytes
}

/// A card's code and its expiry in the ways a page or a form prints them (`06/29`, `06/2029`,
/// `6/29`, `06-29`, `06 / 29`, `06.29`, `0629`, `2029-06`, ...), each to the card's label.
fn tokens(card: i64, cvc: &str, month: u8, year: u16) -> Vec<(Vec<u8>, Vec<u8>)> {
    let label = format!("[card:#{card}]").into_bytes();
    let mut forms: Vec<String> = Vec::new();
    if !cvc.is_empty() {
        forms.push(cvc.to_string());
    }
    let (yy, yyyy) = (format!("{:02}", year % 100), year.to_string());
    let mut months = vec![format!("{month:02}")];
    if month < 10 {
        months.push(month.to_string());
    }
    for m in &months {
        for y in [&yy, &yyyy] {
            for sep in ["/", "-", " / ", ".", " "] {
                forms.push(format!("{m}{sep}{y}"));
            }
        }
    }
    let mm = &months[0];
    forms.extend([format!("{mm}{yy}"), format!("{mm}{yyyy}")]);
    forms.extend([format!("{yyyy}-{mm}"), format!("{yyyy}/{mm}")]);
    forms.sort_by_key(|f| std::cmp::Reverse(f.len()));
    forms.dedup();
    (forms.into_iter())
        .map(|f| (f.into_bytes(), label.clone()))
        .collect()
}

/// `bytes` with each token replaced where no digit stands beside it: a three-digit code
/// inside a longer number is that number, and `06/29` inside `2006/29` is a date of its own.
pub fn scrub_tokens(bytes: &[u8], tokens: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut text = bytes.to_vec();
    for (from, to) in tokens.iter().filter(|(from, _)| !from.is_empty()) {
        if text.len() < from.len() {
            continue;
        }
        let (mut out, mut at, mut i) = (Vec::with_capacity(text.len()), 0, 0);
        while i + from.len() <= text.len() {
            let alone = text[i..].starts_with(from)
                && !(i > 0 && text[i - 1].is_ascii_digit())
                && !text.get(i + from.len()).is_some_and(u8::is_ascii_digit);
            if alone {
                out.extend_from_slice(&text[at..i]);
                out.extend_from_slice(to);
                i += from.len();
                at = i;
            } else {
                i += 1;
            }
        }
        out.extend_from_slice(&text[at..]);
        text = out;
    }
    text
}

/// A line of the shared list: the card, its number, its code, its expiry month and year.
fn shared_lines(shared: &str) -> Vec<(i64, &str, &str, u8, u16)> {
    fn line(l: &str) -> Option<(i64, &str, &str, u8, u16)> {
        let mut cols = l.split('\t');
        let card = cols.next()?.parse().ok()?;
        let number = cols.next()?;
        let digits = number.bytes().all(|b| b.is_ascii_digit());
        let cvc = cols.next().unwrap_or_default();
        let month = cols.next().and_then(|m| m.parse().ok()).unwrap_or(0);
        let year = cols.next().and_then(|y| y.parse().ok()).unwrap_or(0);
        (digits && number.len() >= 12).then_some((card, number, cvc, month, year))
    }
    shared.lines().filter_map(line).collect()
}

/// What a log scrubber replaces wherever it stands, from the shared list's text
/// (`live::shared`): each number as plain digits, in groups of four with spaces or dashes,
/// and its base64 forms, to its card's label.
pub fn stream_pairs(shared: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = Vec::new();
    for (card, number, ..) in shared_lines(shared) {
        let label = format!("[card:#{card}]").into_bytes();
        let grouped = |sep: &str| -> String {
            let groups: Vec<&str> = (0..number.len())
                .step_by(4)
                .map(|i| &number[i..(i + 4).min(number.len())])
                .collect();
            groups.join(sep)
        };
        let mut forms = vec![number.to_string(), grouped(" "), grouped("-")];
        forms.extend(crate::vault::base64_forms(number));
        pairs.extend(forms.into_iter().map(|f| (f.into_bytes(), label.clone())));
    }
    pairs
}

/// What a log scrubber replaces only where it stands alone ([`scrub_tokens`]): each card's
/// code and expiry.
pub fn stream_tokens(shared: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let lines = shared_lines(shared);
    let cards = lines
        .iter()
        .filter(|(.., month, _)| (1..=12).contains(month));
    cards
        .flat_map(|(card, _, cvc, month, year)| tokens(*card, cvc, *month, *year))
        .collect()
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

// ---- the card's life: ask, issue, the owner's number, fill, poll, close ----

/// Who asked for a card: where its notices and reports go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Asker {
    pub channel: String,
    pub sender: String,
    pub session: String,
}

/// What the agent is told of a card it must wait for.
const WAIT_FOR_IT: &str =
    "you get a message when it can be filled. Tell the owner and end your turn";

fn open_store(cfg: &Config) -> Result<Store, String> {
    Store::open(&cfg.db_path()).map_err(|e| format!("zephyr could not read its records: {e}"))
}

/// A store connection of the cards' own, for work that waits on an issuer: locked for one
/// statement at a time, never across a wait.
struct Db(Mutex<Store>);

impl Db {
    fn open(cfg: &Config) -> Result<Db, String> {
        open_store(cfg).map(|s| Db(Mutex::new(s)))
    }

    fn get(&self) -> std::sync::MutexGuard<'_, Store> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One audit row of a card's life (session `card`): what it is bound to, never its number.
fn audit(cfg: &Config, store: &Store, card: &Card, tool: &str, outcome: &str) {
    let last4 = card.last4.as_deref().unwrap_or("----");
    let args = format!(
        "#{} {} {} {} {} …{last4}",
        card.id, card.origin, card.cap_minor, card.currency, card.uses
    );
    let outcome = crate::vault::scrubbed(cfg, outcome.to_string());
    crate::gate::audit_row(store, "card", tool, &args, &outcome);
}

/// A notice about `card` for its owner, after its change landed: one that cannot be written now
/// is owed to a later tick (`Store::owe_notice`).
fn tell(store: &Store, card: &Card, text: &str) {
    store.owe_notice(&card.channel, &card.sender, text);
}

fn note(store: &Store, card: &Card, word: &str, text: &str) {
    if let Err(e) = store.card_note(card.id, word, text) {
        eprintln!(
            "zephyr: telling the agent about card #{} failed: {e}",
            card.id
        );
    }
}

/// `14:20`, in the owner's time zone.
fn clock(cfg: &Config, at: i64) -> String {
    let tz = crate::scheduler::zone(cfg).unwrap_or(jiff::tz::TimeZone::UTC);
    crate::settings::clock(at, &tz)
}

/// Why `currency` cannot be issued, when the provider lists what it can.
pub fn refuses_currency(cfg: &Config, currency: &str) -> Option<String> {
    let list = caps(cfg)?.currencies?;
    (!list.contains(&currency))
        .then(|| format!("not run: the card provider does not issue {currency} cards"))
}

/// Issues a card the gate let through (`Checked` is its proof): records it, asks the provider,
/// and tells the owner what to do next. `Ok` is what the agent is told, `Err` why no card was
/// issued. The payment the window counted for it is taken out again only when the issuer
/// made no card (no reference came back): one it made may still be charged until it is
/// closed there. Nothing is retried by itself.
pub async fn issue(
    cfg: &Config,
    checked: Checked,
    ask: &CardAsk,
    asker: &Asker,
    proposal: Option<i64>,
    reason: Option<&str>,
) -> Result<String, String> {
    let payment = checked.payment();
    let mut made = false;
    let done = issue_card(cfg, checked, ask, asker, proposal, reason, &mut made).await;
    if let (Err(_), Some(row), false) = (&done, payment, made) {
        if let Err(e) = open_store(cfg).and_then(|s| s.unpay(row).map_err(|e| e.to_string())) {
            eprintln!("zephyr: taking a card's payment out of the window failed: {e}");
        }
    }
    done
}

async fn issue_card(
    cfg: &Config,
    checked: Checked,
    ask: &CardAsk,
    asker: &Asker,
    proposal: Option<i64>,
    reason: Option<&str>,
    made: &mut bool,
) -> Result<String, String> {
    let issuer = issuer(cfg)?;
    let caps = issuer.can();
    let db = Db::open(cfg)?;
    let now = crate::now();
    let new = NewCard {
        proposal,
        payment: checked.payment(),
        provider: issuer.name().into(),
        origin: ask.origin.clone(),
        merchant: ask.merchant.clone(),
        cap_minor: ask.minor,
        currency: ask.currency.clone(),
        uses: if caps.single_use {
            "single"
        } else {
            "hand-made"
        }
        .into(),
        channel: asker.channel.clone(),
        sender: asker.sender.clone(),
        session: asker.session.clone(),
        expires_at: now + 60 * i64::from(minutes(cfg)),
    };
    let failed = |e: rusqlite::Error| format!("zephyr could not record the card: {e}");
    let id = db.get().add_card(&new).map_err(failed)?;
    let mut card = db
        .get()
        .card(id)
        .map_err(failed)?
        .ok_or("the card was not recorded")?;
    let spec = CardSpec {
        card: id,
        cap_minor: ask.minor,
        currency: ask.currency.clone(),
        origin: ask.origin.clone(),
        merchant: ask.merchant.clone(),
        expires_at: card.expires_at,
    };
    let issued = match issuer.issue(&spec).await {
        Ok(issued) => issued,
        Err(e) => {
            let _ = db.get().move_card(id, &["waiting"], "failed", now);
            audit(cfg, &db.get(), &card, "issue", &e.0);
            let why = crate::vault::scrubbed(cfg, e.0);
            return Err(format!("card #{id} was not issued: {why}"));
        }
    };
    *made = true;
    card.provider_ref = Some(issued.provider_ref);
    card.approval_url = issued.approval_url;
    let landed = db.get().card_issued(
        id,
        card.provider_ref.as_deref().unwrap_or_default(),
        card.approval_url.as_deref(),
    );
    // Still waiting once recorded: a close asked right after the record was made (it then saw
    // the reference, `close_at_issuer`) is one too.
    let waiting = || {
        db.get()
            .card(id)
            .ok()
            .flatten()
            .is_some_and(|c| c.status == "waiting")
    };
    if let Err(e) = &landed {
        eprintln!("zephyr: recording card #{id}'s reference failed: {e}");
    }
    if !landed.unwrap_or(false) || !waiting() {
        // Its close was asked while the issuer was answering (a `/stop`), or its reference
        // could not be recorded: what the issuer made is closed there now, and the card is
        // `closing` until the issuer takes that, tried again on the tick. The owner is never
        // told to approve it.
        let reference = card.provider_ref.as_deref().unwrap_or_default();
        if let Err(e) = db.get().card_reclose(id, reference, now) {
            eprintln!("zephyr: recording card #{id}'s close failed: {e}");
        }
        let _ = poll_due(cfg, id);
        let said = close_at_issuer(cfg, &mut card).await;
        say(cfg, card, Word::Closed(LANDED.into(), said));
        return Err(format!("card #{id} was not issued: {LANDED}"));
    }
    audit(cfg, &db.get(), &card, "issue", "ok");
    let scrub = crate::gate::Scrub::load(cfg);
    let cap = crate::approvals::money(card.cap_minor, &card.currency);
    let mins = minutes(cfg);
    let (owner, agent) = match (&card.approval_url, caps.asks_owner) {
        (Some(url), true) => {
            let mut lines = vec![format!("[zephyr] card #{id}: payment (virtual card)")];
            lines.extend(crate::gate::card_notice(cfg, ask, &scrub));
            if let Some(reason) = reason.map(|r| crate::settings::quoted(&scrub.text(r))) {
                if !reason.is_empty() {
                    lines.push(format!("the agent says: \"{reason}\""));
                }
            }
            lines.push(format!(
                "approve it in Link: {url} · /card close {id} cancels it"
            ));
            let agent =
                format!("card #{id} waits for the owner's approval at the issuer; {WAIT_FOR_IT}");
            (lines.join("\n"), agent)
        }
        // An issuer that neither asks the owner nor needs them: the card exists already.
        _ if issuer.name() != "manual" => (
            format!(
                "[zephyr] card #{id} is issued: single use, up to {cap}, filled only on {}, \
                 closed {mins} min after it opens",
                card.origin
            ),
            format!("card #{id} is being issued; {WAIT_FOR_IT}"),
        ),
        _ => (
            format!(
                "[zephyr] card #{id}: make a card for this purchase: a single-use card (a \
                 one-time card, such as Wise's), or a prepaid card with at most {cap} on it; \
                 zephyr takes no other, since the agent could read a number back from its \
                 browser and only the card's own limit bounds it. Then enter it on the host: \
                 zephyr card put {id} single-use, or zephyr card put {id} prepaid AMOUNT. It is \
                 filled only on {}, and forgotten {mins} min after you enter it; delete it \
                 after the charge",
                card.origin
            ),
            format!("card #{id} is waiting for the owner to make it; {WAIT_FOR_IT}"),
        ),
    };
    tell(&db.get(), &card, &owner);
    Ok(agent)
}

/// The agent's `card` call `call_id` (main only), answered through the gateway file. `req` is
/// the request as the gate decides it, `to` the owner this run serves.
pub fn agent(
    cfg: &Arc<Config>,
    store: &Store,
    call_id: &str,
    action: CardAction,
    req: crate::gate::Request,
    to: Option<(String, String)>,
    desktop: &Arc<crate::desktop::Control>,
) {
    let answer = |text: &str| crate::tools::answer_gateway(cfg, call_id, text);
    if caps(cfg).is_none() {
        return answer(NOT_SET_UP);
    }
    match action {
        CardAction::List => answer(&list(cfg, store)),
        CardAction::Request => {
            let Some(to) = to else {
                return answer("no owner; not run");
            };
            let currency = crate::approvals::currency(cfg);
            if let Some(why) = refuses_currency(cfg, &currency) {
                return answer(&why);
            }
            // A provider that cannot be reached holds nothing for the owner.
            if let Err(why) = issuer(cfg) {
                return answer(&why);
            }
            let asker = Asker {
                channel: to.0.clone(),
                sender: to.1.clone(),
                session: req.session.clone(),
            };
            match crate::gate::decide_card(cfg, store, &req, to, crate::now()) {
                Err(Verdict::Hold { text, .. }) | Err(Verdict::Deny(text)) => answer(&text),
                Err(_) => answer("not run"),
                Ok((checked, ask)) => {
                    let (cfg, id) = (cfg.clone(), call_id.to_string());
                    tokio::spawn(async move {
                        let reason = req.reason.as_deref();
                        let text = match issue(&cfg, checked, &ask, &asker, None, reason).await {
                            Ok(text) => text,
                            Err(why) => format!("not run: {why}"),
                        };
                        crate::tools::answer_gateway(&cfg, &id, &text);
                    });
                }
            }
        }
        CardAction::Fill { id, fields } => {
            // A fill and a takeover never overlap, as the vault's.
            let Some(filling) = desktop.fill() else {
                return answer(crate::vault::HELD);
            };
            let (cfg, call) = (cfg.clone(), call_id.to_string());
            let stop = fills::start(cfg.state_dir(), id);
            tokio::spawn(async move {
                let text = tokio::select! {
                    text = fill(&cfg, id, fields) => text,
                    // The card was closed (`/stop`, `/pause`, a close): dropping the fill
                    // here ends its browser command and wipes its copy of the number.
                    () = stop.notified() => format!(
                        "stopped: card #{id} was closed while it was being filled; nothing \
                         more was typed"
                    ),
                };
                fills::end(cfg.state_dir(), id, &stop);
                drop(filling);
                crate::tools::answer_gateway(&cfg, &call, &text);
            });
        }
    }
}

/// `/ok` took card request `a`: issues it off the router's loop, records how that ended, and
/// tells the agent when no card came of it.
pub fn issue_approved(cfg: &Arc<Config>, checked: Checked, ask: CardAsk, a: &crate::store::Action) {
    let asker = Asker {
        channel: a.origin.channel.clone(),
        sender: a.origin.sender.clone(),
        session: a.origin.session.clone(),
    };
    let (cfg, id, reason) = (cfg.clone(), a.id, a.reason.clone());
    tokio::spawn(async move {
        let done = issue(&cfg, checked, &ask, &asker, Some(id), reason.as_deref()).await;
        let Ok(store) = open_store(&cfg) else { return };
        let (status, result) = match &done {
            Ok(_) => ("done", "a card was issued".to_string()),
            Err(why) => ("failed", why.clone()),
        };
        // How it ended first, so it never stays `running`; a notice that cannot be written now
        // is owed to a later tick.
        if let Err(e) = store.finish_action(id, status, &result) {
            eprintln!("zephyr: recording how #{id} ended failed: {e}");
        }
        if let Err(why) = done {
            let text = format!("the owner approved #{id}, but {why}");
            if let Err(e) = store.action_note(id, "not run", &text) {
                eprintln!("zephyr: telling the agent about #{id} failed: {e}");
            }
            store.owe_notice(&asker.channel, &asker.sender, &format!("[zephyr] {why}"));
        }
    });
}

/// One line per card, newest first, for `/card`, the agent's `list` and `/activity`.
pub fn lines(cfg: &Config, store: &Store, limit: u32) -> Vec<String> {
    let cards = store.cards_recent(limit).unwrap_or_default();
    (cards.iter())
        .map(|c| {
            let cap = crate::approvals::money(c.cap_minor, &c.currency);
            let last4 = c
                .last4
                .as_deref()
                .map_or(String::new(), |l| format!(" …{l}"));
            let state = match c.status.as_str() {
                "waiting" if c.approval_url.is_some() => {
                    "waiting for your approval at the issuer".into()
                }
                "waiting" => format!("waiting for you to make it (zephyr card put {})", c.id),
                "open" => format!("open until {}", clock(cfg, c.expires_at)),
                "charging" => "its Pay click went; waiting for the charge".into(),
                "closing" => "closed here; the issuer has not confirmed it yet".into(),
                "used" => match (c.charged_minor, c.charged_currency.as_deref()) {
                    (Some(m), Some(cur)) => format!("charged {}", crate::approvals::money(m, cur)),
                    _ => "used".into(),
                },
                "unknown" => format!("forgotten; delete it, then /card done {}", c.id),
                other => other.to_string(),
            };
            format!("#{} {cap} at {}{last4} · {state}", c.id, c.origin)
        })
        .collect()
}

fn list(cfg: &Config, store: &Store) -> String {
    match lines(cfg, store, 10) {
        l if l.is_empty() => "[zephyr] no cards yet".into(),
        l => format!("[zephyr] cards, newest first:\n{}", l.join("\n")),
    }
}

/// What a hand-made card may be, as the owner says when entering it: one whose own limit bounds
/// what a leaked number can spend (DESIGN §4.25). `prepaid` comes with its balance.
pub const KINDS: [&str; 2] = ["single-use", "prepaid"];

/// Why a hand-made card that is not bounded is refused, for card `card`.
fn unbounded(card: &Card) -> String {
    format!(
        "[zephyr] a card you make must be single-use (a one-time card, such as Wise's) or \
         prepaid with at most {} on it: zephyr card put {id} single-use, or zephyr card put \
         {id} prepaid AMOUNT. A number typed into the agent's browser can be read back by the \
         agent, so only the card's own limit bounds what it can spend",
        crate::approvals::money(card.cap_minor, &card.currency),
        id = card.id
    )
}

/// Whether `kind` (the words after `put N`) is a bounded card for `card`: `single-use`, or
/// `prepaid` with a balance in the card's currency at most its cap.
fn bounded(card: &Card, kind: &[&str]) -> bool {
    match kind {
        ["single-use"] => true,
        ["prepaid", balance] => balance
            .parse::<f64>()
            .ok()
            .filter(|b| b.is_finite() && *b > 0.0)
            .is_some_and(|b| crate::approvals::minor_of(b, &card.currency) <= card.cap_minor),
        _ => false,
    }
}

/// The owner's `/card` on `channel` (`strong` or not), from chat line `msg_id`: `list`,
/// `put N single-use|prepaid BALANCE` (the number comes from memory, `entered`), `done N`,
/// `close N`. `None` for
/// what is not a card command.
pub fn command(
    cfg: &Arc<Config>,
    store: &Store,
    text: &str,
    msg_id: &str,
    strong: bool,
) -> Option<String> {
    let (cmd, arg) = text.split_once(' ').unwrap_or((text, ""));
    if cmd != "/card" {
        return None;
    }
    let words: Vec<&str> = arg.split_whitespace().collect();
    let number = |n: &str| n.trim_start_matches('#').parse::<i64>().ok();
    let found = |n: &str| match number(n).map(|id| store.card(id)) {
        Some(Ok(Some(card))) => Ok(card),
        Some(Err(e)) => Err(format!("[zephyr] reading the card failed: {e}")),
        _ => Err(format!("[zephyr] there is no card {n}")),
    };
    let now = crate::now();
    Some(match words[..] {
        [] | ["list"] => list(cfg, store),
        ["put", n, ref kind @ ..] => {
            // Taken first: a number that came with a refused line is dropped, never kept.
            let pan = entered(msg_id);
            let card = match found(n) {
                Ok(card) => card,
                Err(e) => return Some(e),
            };
            if !strong {
                let strong = cfg.settings.strong_channels.join(" or ");
                return Some(format!("[zephyr] entering a card needs {strong}"));
            }
            if !bounded(&card, kind) {
                return Some(unbounded(&card));
            }
            let kind = kind[0];
            if card.provider != "manual" || card.status != "waiting" {
                return Some(format!(
                    "[zephyr] card #{} is not waiting for a number ({})",
                    card.id, card.status
                ));
            }
            let Some(pan) = pan else {
                return Some(format!(
                    "[zephyr] a card number is never typed into a chat: run `zephyr card put \
                     {}` on the host",
                    card.id
                ));
            };
            put(cfg, store, card, pan, kind, now)
        }
        ["done", n] => match found(n) {
            Err(e) => e,
            // An issuer's card is closed at the issuer, and the owner hears how that went:
            // marking it closed here alone would leave it open there.
            Ok(card) if card.provider != "manual" => match card.status.as_str() {
                // Its close is asked already and tried until the issuer takes it: the owner
                // hears when it does (`closed`).
                "closing" => format!(
                    "[zephyr] card #{}'s close is still being confirmed at the issuer; you are \
                     told when it is",
                    card.id
                ),
                s if LIVE.contains(&s) => {
                    let id = card.id;
                    ask_close(cfg, store, card, "the owner said it is done");
                    format!("[zephyr] closing card #{id} at the issuer; you are told how it went")
                }
                _ => format!("[zephyr] card #{} was {} already", card.id, card.status),
            },
            Ok(card) => {
                let from = ["waiting", "open", "charging", "closing", "unknown"];
                match store.move_card(card.id, &from, "closed", now) {
                    Ok(true) => {
                        live::forget(cfg.state_dir(), card.id, now);
                        audit(cfg, store, &card, "close", "the owner deleted it");
                        format!("[zephyr] card #{} is closed", card.id)
                    }
                    Ok(false) => format!("[zephyr] card #{} was {} already", card.id, card.status),
                    Err(e) => format!("[zephyr] closing the card failed: {e}"),
                }
            }
        },
        ["close", n] => match found(n) {
            Err(e) => e,
            Ok(card) if !matches!(card.status.as_str(), "waiting" | "open" | "charging") => {
                format!("[zephyr] card #{} was {} already", card.id, card.status)
            }
            Ok(card) => {
                ask_close(cfg, store, card, "the owner closed it");
                format!("[zephyr] closing card {n}")
            }
        },
        _ => "[zephyr] usage: /card, /card done N, /card close N; a card's number is entered \
              with `zephyr card put N` on the host"
            .into(),
    })
}

/// The owner entered hand-made card `card`'s number: kept in memory, the card open from now
/// for `cards.minutes`, and the agent told it can be filled.
fn put(cfg: &Config, store: &Store, mut card: Card, pan: Pan, kind: &str, now: i64) -> String {
    card.last4 = Some(pan.last4());
    card.kind = Some(kind.into());
    card.status = "open".into();
    card.expires_at = now + 60 * i64::from(minutes(cfg));
    let opened = store.open_card(
        card.id,
        card.last4.as_deref(),
        Some(kind),
        card.expires_at,
        now,
    );
    if !opened.unwrap_or(false) {
        return format!(
            "[zephyr] card #{} is not waiting for a number any more",
            card.id
        );
    }
    live::put(cfg.state_dir(), card.id, pan);
    audit(cfg, store, &card, "reveal", "entered by the owner");
    note(store, &card, "issued", &ready(cfg, &card));
    // What the owner said it is is what bounds a number the agent could read back.
    let bound = match kind {
        "single-use" => "it pays once",
        _ => "what is on it is what it can spend",
    };
    format!(
        "[zephyr] card #{} (…{}) is ready: filled only on {}, forgotten at {}; {bound}",
        card.id,
        card.last4.as_deref().unwrap_or_default(),
        card.origin,
        clock(cfg, card.expires_at)
    )
}

/// What the agent is told when card `card` can be filled.
fn ready(cfg: &Config, card: &Card) -> String {
    format!(
        "a card for up to {}, filled only on {}, until {}; use card fill with id {} on the \
         payment form",
        crate::approvals::money(card.cap_minor, &card.currency),
        card.origin,
        clock(cfg, card.expires_at),
        card.id
    )
}

/// Types card `id` into the page open in agent-browser, only on the card's own origin
/// (scheme, host and port). The answer names the fields and the last four digits, never a value.
///
/// - The page's address is read first, so a page elsewhere fetches no number.
/// - The number is fetched then (for Link, a call to the issuer), before any field is typed:
///   nothing slow stands between a check and the typing it guards.
/// - Each field is typed by a page script that checks the top document's origin itself, in the
///   same step (`vault::type_into`): a page that moved gets nothing. A field not in the top
///   document is looked for in the payment processors' frames directly in it, and typed there
///   by a script that checks the frame's origin, its parent and the top's origin in the same
///   step (`cards_frame`); any other frame gets nothing. The address is read again after each
///   field.
/// - A page that moved away closes the card.
/// - No value is ever a process's argument; each copy made here is wiped when it is done with.
async fn fill(cfg: &Arc<Config>, id: i64, fields: CardFields) -> String {
    use crate::vault::Typed;
    let db = match Db::open(cfg) {
        Ok(s) => s,
        Err(e) => return format!("not filled: {e}"),
    };
    let Ok(Some(mut card)) = db.get().card(id) else {
        return format!("not filled: there is no card #{id}");
    };
    let now = crate::now();
    match card.status.as_str() {
        "open" if now < card.expires_at => {}
        "open" => return format!("not filled: card #{id} has expired"),
        "waiting" => return format!("not filled: card #{id} still waits for the owner"),
        "charging" => return format!("not filled: card #{id} was used for its payment already"),
        other => return format!("not filled: card #{id} is {other}"),
    }
    let selectors = [
        ("number", &fields.number),
        ("expiry", &fields.expiry),
        ("month", &fields.month),
        ("year", &fields.year),
        ("code", &fields.cvc),
        ("name", &fields.name),
    ];
    if selectors.iter().all(|(_, s)| s.is_none()) {
        return "not filled: give the CSS selector of at least one of number, expiry, month, \
                year, cvc, name"
            .into();
    }
    let page = match crate::vault::browser(cfg, &["get", "url"]).await {
        Ok(out) => crate::vault::page_address(&out).map(String::from),
        Err(e) => return format!("not filled: reading the page's address failed: {e}"),
    };
    let on_site = |p: &String| crate::vault::same_origin(&card.origin, p);
    let Some(page) = page.clone().filter(on_site) else {
        let on = page
            .as_deref()
            .and_then(crate::vault::origin)
            .map_or("no http(s) page".into(), |o| o.ascii_serialization());
        let why = format!("the page was on {on}");
        audit(cfg, &db.get(), &card, "refused", &why);
        return format!(
            "not filled: the open page is on {on}, but card #{id} is only for {}",
            card.origin
        );
    };
    // On the card's own origin: now the number is fetched, before any field's own check.
    let pan = match live::get(cfg.state_dir(), id) {
        Some(pan) => pan,
        None => {
            let revealed = match issuer(cfg) {
                Ok(issuer) => issuer.reveal(&card).await.map_err(|e| e.0),
                Err(e) => Err(e),
            };
            match revealed {
                Ok(pan) => {
                    live::put(cfg.state_dir(), id, pan.clone());
                    card.last4 = Some(pan.last4());
                    let _ = db.get().card_last4(id, &pan.last4());
                    audit(cfg, &db.get(), &card, "reveal", "ok");
                    pan
                }
                Err(e) => {
                    audit(cfg, &db.get(), &card, "reveal", &e);
                    return format!("not filled: {}", crate::vault::scrubbed(cfg, e));
                }
            }
        }
    };
    let value = |label: &str| -> Option<String> {
        Some(match label {
            "number" => pan.number.clone(),
            "expiry" => pan.expiry(),
            "month" => format!("{:02}", pan.exp_month),
            "year" => pan.exp_year.to_string(),
            "code" => pan.cvc.clone(),
            _ => pan.name.clone()?,
        })
    };
    // The page left the card's site: nothing more is typed, and the card is closed.
    let moved = |done: &[&str], on: &str| -> String {
        audit(
            cfg,
            &db.get(),
            &card,
            "fill",
            &format!("the page moved to {on}"),
        );
        let why = "the page left its site while it was being filled";
        ask_close(cfg, &db.get(), card.clone(), why);
        let typed = match done.is_empty() {
            true => "nothing was typed".to_string(),
            false => format!(
                "the {} had been typed and may have reached that page",
                done.join(" and ")
            ),
        };
        format!(
            "stopped: the page was on {on}, not {}; {typed}. Card #{id} is closed. Submit \
             nothing there, and tell the owner",
            card.origin
        )
    };
    // From here the page's fields hold the number: the browser wrapper reads it back to no
    // one until the number is dropped.
    live::filled(cfg, id, &card.origin, &page);
    let mut done: Vec<&str> = Vec::new();
    // The payment frames of the tab, found once, at the first field the page lacks.
    let mut frames: Option<crate::cards_frame::Frames> = None;
    let mut inside: Vec<String> = Vec::new();
    let mut last = page.clone();
    for (label, sel) in selectors {
        let Some(sel) = sel else { continue };
        // Wiped when dropped, so a fill stopped mid-field leaves no copy either.
        let Some(value) = value(label).map(crate::cards_frame::Secret::new) else {
            return format!("not filled: card #{id} has no {label}; leave `{label}` out");
        };
        let mut typed = crate::vault::type_into(cfg, &card.origin, sel, value.as_str()).await;
        let framed = typed == Typed::Missing;
        if framed {
            // Not in the page's own document: a payment processor's frame directly in it.
            let value = value.as_str();
            typed = in_frame(cfg, &mut frames, &card.origin, sel, value, &mut inside).await;
        }
        drop(value);
        if let Typed::Moved(on) = &typed {
            return moved(&done, on);
        }
        // A field no frame took (gone, refused, not found) may be one whose checkout left:
        // then the card closes, as when the page's own document moves.
        if framed && typed != Typed::Ok {
            let now = crate::vault::page_now(cfg).await;
            if !now.as_ref().is_some_and(on_site) {
                let on = (now.as_deref())
                    .and_then(crate::vault::origin)
                    .map_or("an unknown page".into(), |o| o.ascii_serialization());
                return moved(&done, &on);
            }
        }
        let why = match typed {
            Typed::Missing => Some(format!(
                "no field for the {label} matches that selector in the page's own document or \
                 in a payment processor's frame directly in it (Stripe, Braintree); a field in \
                 any other frame (an embedded form from another site) is never filled"
            )),
            _ => crate::vault::not_typed(&typed, label),
        };
        if let Some(why) = why {
            let done = match done.is_empty() {
                true => String::new(),
                false => format!("filled the {}, ", done.join(" and ")),
            };
            audit(cfg, &db.get(), &card, "fill", "failed");
            return crate::vault::scrubbed(cfg, format!("{done}but not the {label}: {why}"));
        }
        done.push(label);
        let now = crate::vault::page_now(cfg).await;
        match now {
            Some(now) if on_site(&now) => last = now,
            _ => {
                let on = now
                    .as_deref()
                    .and_then(crate::vault::origin)
                    .map_or("an unknown page".into(), |o| o.ascii_serialization());
                return moved(&done, &on);
            }
        }
    }
    // The page it went through on is the one whose Pay click the card covers, once the number
    // itself is in it: an expiry or a name alone pays nothing.
    if done.contains(&"number") {
        log(db.get().card_filled(id, &last));
    }
    audit(cfg, &db.get(), &card, "fill", "ok");
    if let Some(frames) = frames {
        frames.close().await;
    }
    let inside = match inside.is_empty() {
        true => String::new(),
        false => format!(", in its payment frame on {}", inside.join(" and ")),
    };
    format!(
        "filled the {} of card #{id} (…{}) on {page}{inside}",
        done.join(" and "),
        pan.last4()
    )
}

/// Types `value` into `selector` in the payment frame of the active tab that holds it,
/// opening the fill's DevTools session (`frames`) at the first field that needs it. The origin
/// of a frame typed into joins `inside`.
async fn in_frame(
    cfg: &Config,
    frames: &mut Option<crate::cards_frame::Frames>,
    site: &str,
    selector: &str,
    value: &str,
    inside: &mut Vec<String>,
) -> crate::vault::Typed {
    use crate::vault::Typed;
    if frames.is_none() {
        match crate::cards_frame::Frames::open(cfg, site).await {
            Ok(opened) => *frames = Some(opened),
            Err(why) => return Typed::Failed(format!("not typed: {why}")),
        }
    }
    let Some(frames) = frames.as_mut() else {
        return Typed::Missing;
    };
    match frames.find(cfg, selector).await {
        Ok(None) => Typed::Missing,
        Err(why) => Typed::Failed(format!("not typed: {why}")),
        Ok(Some((at, origin))) => {
            let typed = frames.type_value(at, &origin, selector, value).await;
            if typed == Typed::Ok && !inside.contains(&origin) {
                inside.push(origin);
            }
            typed
        }
    }
}

/// The fills in progress, so that closing a card stops one between two fields.
mod fills {
    use super::*;
    use tokio::sync::Notify;

    static FILLS: Mutex<Vec<(PathBuf, i64, Arc<Notify>)>> = Mutex::new(Vec::new());

    fn lock() -> std::sync::MutexGuard<'static, Vec<(PathBuf, i64, Arc<Notify>)>> {
        FILLS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A fill of `card` starts: what `stop` wakes.
    pub fn start(state: &std::path::Path, card: i64) -> Arc<Notify> {
        let stop = Arc::new(Notify::new());
        lock().push((state.to_path_buf(), card, stop.clone()));
        stop
    }

    pub fn end(state: &std::path::Path, card: i64, stop: &Arc<Notify>) {
        lock().retain(|(s, c, n)| !(s == state && *c == card && Arc::ptr_eq(n, stop)));
    }

    /// Stops every fill of `card`: one that has not reached its wait yet finds the wake stored.
    pub fn stop(state: &std::path::Path, card: i64) {
        for (s, c, stop) in lock().iter() {
            if s == state && *c == card {
                stop.notify_one();
            }
        }
    }
}

/// What an issuer said about a card. It is asked off the router's loop, and what it said is
/// recorded on the router's next tick with the router's own store ([`tick`]): the tick opens
/// no connection of its own, and every write of a card's life after its issue is made there,
/// one at a time.
enum Word {
    State(Result<CardState, String>),
    /// The answer to a close asked because of `why`.
    Closed(String, Result<(), String>),
    /// The answer to the close that follows a charge, with the charge as the owner reads it.
    UsedClosed(String, Result<(), String>),
    /// What the issuer says of a card it took a close of (`why`) and that may have been charged.
    Settled(String, Result<CardState, String>),
}

static WORDS: Mutex<Vec<(PathBuf, Card, Word)>> = Mutex::new(Vec::new());

fn say(cfg: &Config, card: Card, word: Word) {
    let mut words = WORDS.lock().unwrap_or_else(|e| e.into_inner());
    words.push((cfg.state_dir().to_path_buf(), card, word));
}

/// Closes `card` at its issuer. Its reference there is the record's, read now: a copy read
/// before the issue landed has none, and `card` gets the record's, so its word says what was
/// closed.
async fn close_at_issuer(cfg: &Config, card: &mut Card) -> Result<(), String> {
    if card.provider_ref.is_none() {
        let record = open_store(cfg)
            .ok()
            .and_then(|s| s.card(card.id).ok().flatten());
        card.provider_ref = record.and_then(|c| c.provider_ref);
    }
    // The issuer has not made it yet (its issue is still being answered): there is nothing
    // there to close. The close is kept in the card's status, and the issue closes what it
    // makes when it lands (`issue`).
    if card.provider_ref.is_none() {
        return Ok(());
    }
    match issuer(cfg) {
        Ok(issuer) => issuer.close(card).await.map_err(|e| e.0),
        Err(e) => Err(e),
    }
}

/// Closes `card`. Here it is closed at once, in this statement: `closing` and past its time,
/// its number forgotten and a fill in hand stopped, so from now on it covers no Pay click and
/// is neither filled nor opened. The issuer is asked off the caller's loop, and the router's
/// next tick only records how that went ([`closed`]).
pub fn ask_close(cfg: &Arc<Config>, store: &Store, card: Card, why: &str) {
    let now = crate::now();
    log(store.card_closing(card.id, now));
    fills::stop(cfg.state_dir(), card.id);
    live::forget(cfg.state_dir(), card.id, now);
    // This close is in hand: the tick's retry waits its turn.
    let _ = poll_due(cfg, card.id);
    let (cfg, why) = (cfg.clone(), why.to_string());
    let mut card = card;
    tokio::spawn(async move {
        let said = close_at_issuer(&cfg, &mut card).await;
        say(&cfg, card, Word::Closed(why, said));
    });
}

/// Records how a close of `card` went at its issuer. One the issuer refused leaves the card
/// `closing`: each later tick tries again, and the owner is told once.
fn closed(
    cfg: &Arc<Config>,
    store: &Store,
    card: &Card,
    why: &str,
    said: Result<(), String>,
    now: i64,
) {
    // Whatever the issuer said: the number is never filled again.
    live::forget(cfg.state_dir(), card.id, now);
    // Asked before the issuer had made the card, so with nothing there to close: if the issue
    // has landed since, the close it made is the one recorded (`issue`).
    let landed = || store.card(card.id).ok().flatten()?.provider_ref;
    if said.is_ok() && card.provider_ref.is_none() && landed().is_some() {
        return;
    }
    if let Err(e) = said {
        audit(cfg, store, card, "close", &e);
        log(store.card_closing(card.id, now));
        let e = crate::vault::scrubbed(cfg, e);
        let text = format!(
            "[zephyr] card #{} could not be closed at the issuer: {e}; close it in the \
             provider's app",
            card.id
        );
        if store.card_told_once(card.id, now).unwrap_or(false) {
            tell(store, card, &text);
        }
        return;
    }
    // Its Pay click may have gone: the issuer is asked once more what it was charged, so a
    // charge that comes in late is recorded, and counted, before the card ends closed.
    let fresh = store.card(card.id).ok().flatten();
    if let Some(fresh) = fresh.filter(|c| c.status == "closing" && chargeable(cfg, c)) {
        let (cfg, why) = (cfg.clone(), why.to_string());
        tokio::spawn(async move {
            let state = match issuer(&cfg) {
                Ok(issuer) => issuer.state(&fresh).await.map_err(|e| e.0),
                Err(e) => Err(e),
            };
            say(&cfg, fresh, Word::Settled(why, state));
        });
        return;
    }
    closed_now(cfg, store, card, why, now);
}

/// Whether closing `card` may still see a charge: an issuer that reports them, on a card that
/// was open or past its Pay click when its close was asked.
fn chargeable(cfg: &Config, card: &Card) -> bool {
    let from = card.closing_from.as_deref();
    card.provider_ref.is_some()
        && caps(cfg).is_some_and(|c| c.charges)
        && matches!(from, Some("open" | "charging"))
}

/// The close of `card` is taken: it is closed (or `unknown`, a hand-made one), and the owner
/// and the agent are told.
fn closed_now(cfg: &Config, store: &Store, card: &Card, why: &str, now: i64) {
    // A hand-made card is the owner's to delete: until they say so, it is not known closed.
    let to = match card.provider == "manual" && card.last4.is_some() {
        true => "unknown",
        false => "closed",
    };
    let owner = match () {
        _ if why == RETRY || why == LANDED => {
            format!("[zephyr] card #{} is closed at the issuer now", card.id)
        }
        _ if to == "unknown" => delete_it(card, why),
        _ => format!("[zephyr] card #{} is closed: {why}", card.id),
    };
    if !store
        .move_card(card.id, &UNSETTLED, to, now)
        .unwrap_or(false)
    {
        return;
    }
    if to == "unknown" && why != RETRY && why != LANDED {
        log(store.card_told(card.id, now));
    }
    tell(store, card, &owner);
    audit(cfg, store, card, "close", why);
    let text = format!("{why}; the card is closed and cannot be filled");
    note(store, card, "closed", &text);
}

fn delete_it(card: &Card, why: &str) -> String {
    format!(
        "[zephyr] card #{id} (…{}): {why}, and zephyr forgot its number. No charge data: check \
         your card app, delete the card there, then /card done {id}",
        card.last4.as_deref().unwrap_or_default(),
        id = card.id
    )
}

/// `charged 42.50 CAD by merchant id 4445123456 ("AMAZON.CA", MCC 5999)`: the payee as the
/// issuer names it, its identifier first.
fn charged(c: &Charge) -> String {
    let amount = crate::approvals::money(c.minor, &c.currency);
    let name =
        (c.descriptor.as_deref()).map(|d| format!("\"{}\"", crate::settings::quoted_cut(d, 80)));
    let mcc = c
        .mcc
        .as_deref()
        .map(|m| format!("MCC {}", crate::settings::quoted_cut(m, 8)));
    let extra: Vec<String> = name.into_iter().chain(mcc).collect();
    let extra = match extra.is_empty() {
        true => String::new(),
        false => format!(" ({})", extra.join(", ")),
    };
    match c.merchant_id.as_deref() {
        Some(id) => format!(
            "charged {amount} by merchant id {}{extra}",
            crate::settings::quoted_cut(id, 40)
        ),
        None => format!("charged {amount}{extra}; no merchant id from the issuer"),
    }
}

/// How often an open card's issuer is asked, as a multiple of the router's tick: 30 s.
const POLL_TICKS: u32 = 6;

static POLLED: Mutex<Vec<(PathBuf, i64, std::time::Instant)>> = Mutex::new(Vec::new());

/// Whether card `id` is due for a poll, noting that it is polled now.
fn poll_due(cfg: &Config, id: i64) -> bool {
    let every = cfg.tick * POLL_TICKS;
    let mut all = POLLED.lock().unwrap_or_else(|e| e.into_inner());
    all.retain(|(_, _, at)| at.elapsed() < every * 100);
    let state = cfg.state_dir();
    match all.iter_mut().find(|(s, c, _)| s == state && *c == id) {
        Some((_, _, at)) if at.elapsed() < every => false,
        Some((_, _, at)) => {
            *at = std::time::Instant::now();
            true
        }
        None => {
            all.push((state.to_path_buf(), id, std::time::Instant::now()));
            true
        }
    }
}

/// The statuses of a card that is not over: it may still be filled, charged or closed.
const LIVE: [&str; 3] = ["waiting", "open", "charging"];
/// Those, and `closing`: a close was asked and the issuer has not confirmed it yet.
const UNSETTLED: [&str; 4] = ["waiting", "open", "charging", "closing"];
/// Why a close is asked again: the issuer refused it before.
const RETRY: &str = "the close was tried again";
/// Why a card is closed as its issue lands: its close was asked meanwhile.
const LANDED: &str = "it was closed while it was being issued";

/// The router's tick for cards, on the router's own store. Numbers past their hour are
/// dropped. What the issuers said since the last tick is recorded: a card opens once the owner
/// approved it at the issuer, and closes at the first charge the issuer reports. A hand-made
/// card the owner has not confirmed deleted is recalled once a day. Then each card that is
/// past its expiry, or due (every 30 s), has its issuer asked, off the loop.
pub fn tick(cfg: &Arc<Config>, store: &Store, now: i64) {
    live::sweep(now);
    let words: Vec<(Card, Word)> = {
        let mut all = WORDS.lock().unwrap_or_else(|e| e.into_inner());
        let state = cfg.state_dir();
        let (mine, rest) = std::mem::take(&mut *all)
            .into_iter()
            .partition(|(s, ..)| s == state);
        *all = rest;
        let mine: Vec<(PathBuf, Card, Word)> = mine;
        mine.into_iter()
            .map(|(_, card, word)| (card, word))
            .collect()
    };
    for (card, word) in words {
        match word {
            Word::State(state) => stated(cfg, store, card, state, now),
            Word::Closed(why, said) => closed(cfg, store, &card, &why, said, now),
            Word::UsedClosed(what, said) => used_closed(cfg, store, &card, &what, said),
            Word::Settled(why, state) => match state {
                Ok(CardState::Used(charges)) => used(cfg, store, card, &charges, now),
                _ => closed_now(cfg, store, &card, &why, now),
            },
        }
    }
    for card in store.cards_in(&["unknown"]).unwrap_or_default() {
        if card.provider == "manual" && card.told_at.is_none_or(|at| at + 86_400 <= now) {
            let text = delete_it(&card, "it is past its time");
            log(store.card_told(card.id, now));
            tell(store, &card, &text);
        }
    }
    let mut asks: Vec<(Card, Option<&'static str>)> = Vec::new();
    for card in store.cards_in(&UNSETTLED).unwrap_or_default() {
        // A close the issuer refused is asked again; a card that may have been charged is
        // asked about first, so a charge is not missed ([`stated`] closes it again).
        if card.status == "closing" {
            if card.provider_ref.is_some() && poll_due(cfg, card.id) {
                let state = chargeable(cfg, &card);
                asks.push((card, (!state).then_some(RETRY)));
            }
            continue;
        }
        if now >= card.expires_at {
            // Closed here at once, and asked at the issuer now.
            log(store.card_closing(card.id, now));
            fills::stop(cfg.state_dir(), card.id);
            live::forget(cfg.state_dir(), card.id, now);
            let _ = poll_due(cfg, card.id);
            // Only an issuer that reports charges can say a card went unused.
            let why = match caps(cfg).is_some_and(|c| c.charges) {
                true => "expired unused",
                false => "it is past its time",
            };
            asks.push((card, Some(why)));
            continue;
        }
        // A card the issuer has not made yet is not asked about: its issue is in hand.
        if card.provider_ref.is_some() && poll_due(cfg, card.id) {
            asks.push((card, None));
        }
    }
    if asks.is_empty() {
        return;
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        for (mut card, close) in asks {
            let word = match close {
                Some(why) => Word::Closed(why.into(), close_at_issuer(&cfg, &mut card).await),
                None => Word::State(match issuer(&cfg) {
                    Ok(issuer) => issuer.state(&card).await.map_err(|e| e.0),
                    Err(e) => Err(e),
                }),
            };
            say(&cfg, card, word);
        }
    });
}

/// Records what the issuer said `card`'s state is.
fn stated(
    cfg: &Arc<Config>,
    store: &Store,
    card: Card,
    state: Result<CardState, String>,
    now: i64,
) {
    match state {
        Ok(CardState::Open) if card.status == "waiting" && card.provider != "manual" => {
            let until = now + 60 * i64::from(minutes(cfg));
            // Only a card still waiting and in its time opens: one whose close was asked
            // meanwhile stays as the close left it, and the tick closes it at the issuer again.
            if store
                .open_card(card.id, None, None, until, now)
                .unwrap_or(false)
            {
                let card = Card {
                    expires_at: until,
                    ..card
                };
                audit(cfg, store, &card, "issue", "approved at the issuer");
                note(store, &card, "issued", &ready(cfg, &card));
            }
        }
        Ok(CardState::Used(charges)) => used(cfg, store, card, &charges, now),
        // A closing card the issuer has not closed: its close is asked again.
        Ok(CardState::Open | CardState::Waiting) | Err(_) if card.status == "closing" => {
            let mut card = card;
            let cfg = cfg.clone();
            tokio::spawn(async move {
                let said = close_at_issuer(&cfg, &mut card).await;
                say(&cfg, card, Word::Closed(RETRY.into(), said));
            });
        }
        Ok(CardState::Closed) => {
            live::forget(cfg.state_dir(), card.id, now);
            let text = format!("[zephyr] card #{} is closed at the issuer now", card.id);
            let moved = store.move_card(card.id, &UNSETTLED, "closed", now);
            if moved.unwrap_or(false) {
                if card.status == "closing" {
                    tell(store, &card, &text);
                }
                audit(cfg, store, &card, "close", "closed at the issuer");
                let text = "the issuer closed it; it cannot be filled";
                note(store, &card, "closed", text);
            }
        }
        Ok(CardState::Failed(why)) => {
            live::forget(cfg.state_dir(), card.id, now);
            let why = crate::vault::scrubbed(cfg, why);
            let text = format!("[zephyr] card #{} was not issued: {why}", card.id);
            let moved = store.move_card(card.id, &UNSETTLED, "failed", now);
            if moved.unwrap_or(false) {
                tell(store, &card, &text);
                audit(cfg, store, &card, "issue", &why);
                let text = format!("no card: {why}; ask again if it is still wanted");
                note(store, &card, "failed", &text);
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!(
            "zephyr: asking the issuer about card #{} failed: {e}",
            card.id
        ),
    }
}

/// The issuer reported `charges` on `card`: recorded once, and a charge the card should not
/// have taken is told at once. The card is then closed at the issuer too, and the owner and
/// the agent are told who charged it when that answer is in ([`used_closed`]).
fn used(cfg: &Arc<Config>, store: &Store, card: Card, charges: &[Charge], now: i64) {
    let Some(first) = charges.first() else { return };
    live::forget(cfg.state_dir(), card.id, now);
    // The window counts what the issuer took in all, when that is more than the cap; in another
    // currency it cannot be counted, and the window holds the next payment as over.
    let same = charges.iter().all(|c| c.currency == card.currency);
    let total = same.then(|| (charges.iter()).fold(0i64, |sum, c| sum.saturating_add(c.minor)));
    let over = first.minor > card.cap_minor || first.currency != card.currency;
    let text = (over || charges.len() > 1).then(|| {
        let all: Vec<String> = charges.iter().map(charged).collect();
        format!(
            "[zephyr] card #{} took what you did not approve (up to {}, once): {}; the issuer \
             should have declined it",
            card.id,
            crate::approvals::money(card.cap_minor, &card.currency),
            all.join("; ")
        )
    });
    // The charge and what the window counts land together; the notice follows, owed when it
    // cannot be written now.
    let charge = store.atomically(|store| {
        let charged = store.card_charged(
            card.id,
            first.minor,
            &first.currency,
            first.merchant_id.as_deref(),
            first.descriptor.as_deref(),
            first.mcc.as_deref(),
            now,
        )?;
        if charged {
            store.card_paid(card.id, total)?;
        }
        Ok(charged)
    });
    if !charge.unwrap_or_else(|e| {
        eprintln!("zephyr: recording card #{}'s charge failed: {e}", card.id);
        false
    }) {
        return;
    }
    text.iter().for_each(|text| tell(store, &card, text));
    let what = charged(first);
    audit(cfg, store, &card, "charge", &what);
    let (cfg, mut card) = (cfg.clone(), card);
    tokio::spawn(async move {
        let said = close_at_issuer(&cfg, &mut card).await;
        say(&cfg, card, Word::UsedClosed(what, said));
    });
}

/// The close after a charge is in: a refusal there is told and tried no further, the card
/// being used.
fn used_closed(cfg: &Config, store: &Store, card: &Card, what: &str, said: Result<(), String>) {
    let end = match said {
        Ok(()) => "the card is closed".to_string(),
        Err(e) => {
            audit(cfg, store, card, "close", &e);
            "closing it at the issuer failed; close it in the provider's app".to_string()
        }
    };
    let text = format!("[zephyr] card #{} was {what}; {end}", card.id);
    tell(store, card, &text);
    note(store, card, "used", &format!("{what}; {end}"));
}

/// `/stop`, `/pause` and a start: every card that is not over is closed. Their numbers are
/// forgotten before this returns; the issuers are asked off the router's loop.
pub fn close_all(cfg: &Arc<Config>, store: &Store, why: &'static str) {
    for card in store.cards_in(&UNSETTLED).unwrap_or_default() {
        ask_close(cfg, store, card, why);
    }
}

fn log<T>(done: rusqlite::Result<T>) {
    if let Err(e) = done {
        eprintln!("zephyr: recording a card failed: {e}");
    }
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
    fn the_shared_list_gives_a_log_scrubber_each_numbers_forms() {
        let list = "7\t5555555555554444\t987\t11\t2030\nbroken\nx\t4242424242424242\n3\t12\n";
        let pairs = stream_pairs(list);
        let codes = stream_tokens(list);
        assert!(
            codes.contains(&(b"987".to_vec(), b"[card:#7]".to_vec())),
            "its code"
        );
        assert!(
            codes.contains(&(b"11/30".to_vec(), b"[card:#7]".to_vec())),
            "its expiry"
        );
        let froms: Vec<String> = (pairs.iter())
            .map(|(from, _)| String::from_utf8(from.clone()).unwrap())
            .collect();
        assert_eq!(
            froms[..3],
            [
                "5555555555554444",
                "5555 5555 5555 4444",
                "5555-5555-5555-4444"
            ]
        );
        assert!(froms.len() > 3, "and its base64 forms");
        assert!(pairs.iter().all(|(_, to)| to == b"[card:#7]"));
        // The daemon's own list is a descriptor no child inherits by itself.
        let state = crate::test_dir("cards-shared");
        live::put(&state, 9, pan("6011 0009 9013 9424", "111"));
        let fd = live::shared().expect("a memfd");
        // Safety: reading the descriptor's flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "close-on-exec");
        let read = || {
            let file = std::fs::File::open(format!("/proc/self/fd/{fd}")).unwrap();
            read_shared(&file).expect("a whole list")
        };
        let text = read();
        assert!(
            text.contains("9\t6011000990139424\t111\t6\t2029\n"),
            "{text}"
        );
        live::forget(&state, 9, -KEEP_SECS);
        live::sweep(1);
        let text = read();
        assert!(!text.contains("6011000990139424"), "{text}");
    }

    /// A scrubber reading the shared list while the daemon rewrites it sees the old list or
    /// the new one, never an empty or a half-written one.
    #[test]
    fn a_reader_of_the_shared_list_never_sees_it_half_written() {
        use std::os::fd::{FromRawFd, IntoRawFd};
        // Safety: a C string literal and no flags; the result is checked.
        let fd = unsafe { libc::memfd_create(c"zephyr-cards-test".as_ptr(), 0) };
        assert!(fd >= 0);
        // Safety: `fd` is ours and open.
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let long: String = (1..40)
            .map(|n| format!("{n}\t4000009990001984\t123\t6\t2029\n"))
            .collect();
        let short = "7\t5555555555554444\t987\t11\t2030\n".to_string();
        live::write_shared(&file, &short, 0);
        let reader = file.try_clone().unwrap();
        let (a, b) = (long.clone(), short.clone());
        let writer = std::thread::spawn(move || {
            for round in 1..3000u64 {
                let text = if round % 2 == 0 { &a } else { &b };
                live::write_shared(&file, text, round * 2);
            }
            file.into_raw_fd()
        });
        let mut reads = 0;
        while !writer.is_finished() {
            // `None` is "ask again"; what is read is always one list, whole.
            if let Some(text) = read_shared(&reader) {
                assert!(
                    text == long || text == short,
                    "a torn list: {} bytes",
                    text.len()
                );
                reads += 1;
            }
        }
        assert!(reads > 0);
        // Safety: the descriptor the writer thread gave back, closed once.
        drop(unsafe { std::fs::File::from_raw_fd(writer.join().unwrap()) });
    }

    #[test]
    fn the_code_and_the_expiry_scrub_where_they_stand_alone() {
        // Its own expiry and code: the numbers kept are the process's, and the tests run at once.
        let state = crate::test_dir("cards-codes");
        live::put(
            &state,
            8,
            Pan::parse("3530 1113 3330 0000", "03/31", "731").unwrap(),
        );
        for (text, want) in [
            ("exp 03/31, code 731", "exp [card:#8], code [card:#8]"),
            (
                "03/2031 3/31 03-31 03 / 31 0331 2031-03",
                "[card:#8] [card:#8] [card:#8] [card:#8] [card:#8] [card:#8]",
            ),
            (
                "{\"exp_month\":3,\"cvc\":\"731\"}",
                "{\"exp_month\":3,\"cvc\":\"[card:#8]\"}",
            ),
            // Inside a longer number neither is the card's.
            (
                "17310 2003/31 103/310 port 8731",
                "17310 2003/31 103/310 port 8731",
            ),
        ] {
            assert_eq!(scrub(text), want, "{text}");
        }
        live::forget(&state, 8, -KEEP_SECS);
        live::sweep(1);
    }

    #[test]
    fn an_entered_number_is_taken_once() {
        enter("cli-1-1", pan("4242 4242 4242 4242", "123"));
        assert!(entered("cli-1-2").is_none());
        assert_eq!(entered("cli-1-1").unwrap().last4(), "4242");
        assert!(entered("cli-1-1").is_none());
    }

    #[test]
    fn doctor_and_status_name_the_provider_and_never_a_key() {
        let dir = crate::test_dir("cards-doctor");
        let cfg = |extra: &str| -> Config {
            toml::from_str(&format!("rpi = '/r'\nhome = {dir:?}\n{extra}")).unwrap()
        };
        assert_eq!(doctor(&cfg("")), ["ok   cards: off"]);
        assert_eq!(status(&cfg("")), "cards: off");
        let manual = cfg("[approvals]\ncurrency = 'CAD'\nstrong_only = ['loosen', 'money']\n[cards]\nprovider = 'manual'\n");
        assert_eq!(
            doctor(&manual),
            [
                "ok   cards: manual, no key needed: you make each card and enter it with zephyr \
                 card put N; zephyr takes only a single-use card or a prepaid one with at most \
                 its cap on it, since the agent could read a number back from its browser",
                "ok   cards: up to 100.00 CAD each, open 15 min",
            ]
        );
        let link = cfg("[cards]\nprovider = 'link'\n");
        assert_eq!(
            status(&link),
            "cards: link, not connected (zephyr card link)"
        );
        let lines = doctor(&link);
        assert!(
            lines[0].starts_with("warn cards: link has no client id"),
            "{lines:?}"
        );
        assert_eq!(
            lines[1],
            "warn cards: not signed in to Link: zephyr card link"
        );
        assert!(
            lines[2].starts_with("warn cards: approvals.currency is unset"),
            "{lines:?}"
        );
        let service = "[[service]]\nname = \"cards\"\nprovider = \"link\"\nkey = \"sk_doctorkey42\"\naccount = \"lwlpk_x\"\n";
        std::fs::write(dir.join("vault.toml"), service).unwrap();
        std::fs::write(dir.join(crate::cards_link::TOKENS), "{}").unwrap();
        let lines = doctor(&link);
        assert_eq!(lines[0], "ok   cards: link, its client id is saved");
        assert_eq!(
            lines[1],
            "ok   cards: zephyr is signed in to Link; you approve each card there"
        );
        assert_eq!(status(&link), "cards: link, connected");
        assert!(!format!("{lines:?}").contains("sk_doctorkey42"));
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

    /// The issuer refused a close, and a poll made before the close then says the card is
    /// open: it is not opened again, its time is not extended, and the agent is told nothing.
    #[tokio::test]
    async fn an_open_that_arrives_after_a_close_does_not_reopen_the_card() {
        let dir = crate::test_dir("cards-stale-open");
        let text = format!("rpi = '/r'\nhome = {dir:?}\n[cards]\nprovider = 'fake'\n");
        let mut cfg: Config = toml::from_str(&text).unwrap();
        cfg.cards_fake = true;
        let cfg = Arc::new(cfg);
        let store = Store::open(&cfg.db_path()).unwrap();
        let now = crate::now();
        let new = NewCard {
            provider: "fake".into(),
            origin: "https://shop.example".into(),
            currency: "CAD".into(),
            cap_minor: 1000,
            expires_at: now + 900,
            ..NewCard::default()
        };
        let id = store.add_card(&new).unwrap();
        assert!(store.card_issued(id, "fake-1", None).unwrap());
        // What a poll that started before the close has in hand.
        let polled = store.card(id).unwrap().unwrap();
        let why = "the owner's /stop";
        closed(&cfg, &store, &polled, why, Err("no answer".into()), now);
        stated(&cfg, &store, polled, Ok(CardState::Open), now);
        let after = store.card(id).unwrap().unwrap();
        assert_ne!(after.status, "open");
        assert!(after.expires_at <= now, "its time is not extended");
        let notes = store.inbox_with_status("new").unwrap();
        assert!(
            notes.iter().all(|n| !n.text.contains("issued")),
            "the agent is told nothing"
        );
    }

    /// A card the issuer did not issue is no payment: the window's count is as it was.
    #[tokio::test]
    async fn a_card_that_was_not_issued_leaves_the_window_count() {
        let dir = crate::test_dir("cards-issue-failed");
        let text = format!(
            "rpi = '/r'\nhome = {dir:?}\n[approvals]\ncurrency = 'CAD'\nmoney = 'auto 100'\n\
             [cards]\nprovider = 'fake'\n"
        );
        let mut cfg: Config = toml::from_str(&text).unwrap();
        cfg.cards_fake = true;
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        let req = crate::gate::Request {
            session: "main".into(),
            call_id: Some("c1".into()),
            tool: "card".into(),
            args: serde_json::json!({"action": "request", "site": "https://shop.example/pay",
                "merchant": "Shop", "amount": "10", "currency": "CAD"}),
            browser: None,
            run: "main:1".into(),
            reason: None,
        };
        let owner = ("chat".to_string(), "o".to_string());
        let now = crate::now();
        let (checked, ask) = crate::gate::decide_card(&cfg, &store, &req, owner, now).unwrap();
        assert_eq!(store.payments_since(now - 600, "CAD").unwrap().0, 1);
        Fake::script(&cfg, |s| s.fail_issue = Some("declined".into()));
        let asker = Asker {
            channel: "chat".into(),
            sender: "o".into(),
            session: "main".into(),
        };
        let why = issue(&cfg, checked, &ask, &asker, None, None).await;
        assert!(why.unwrap_err().contains("was not issued"));
        assert_eq!(store.payments_since(now - 600, "CAD").unwrap().0, 0);
    }

    /// A card the issuer made but zephyr could not record is closed at the issuer, and its
    /// payment stays counted: until that close lands it could still be charged.
    #[tokio::test]
    async fn a_card_made_but_not_recorded_is_closed_and_stays_counted() {
        let dir = crate::test_dir("cards-issue-unrecorded");
        let text = format!(
            "rpi = '/r'\nhome = {dir:?}\n[approvals]\ncurrency = 'CAD'\nmoney = 'auto 100'\n\
             [cards]\nprovider = 'fake'\n"
        );
        let mut cfg: Config = toml::from_str(&text).unwrap();
        cfg.cards_fake = true;
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        let req = crate::gate::Request {
            session: "main".into(),
            call_id: Some("c1".into()),
            tool: "card".into(),
            args: serde_json::json!({"action": "request", "site": "https://shop.example/pay",
                "merchant": "Shop", "amount": "10", "currency": "CAD"}),
            browser: None,
            run: "main:1".into(),
            reason: None,
        };
        let owner = ("chat".to_string(), "o".to_string());
        let now = crate::now();
        let (checked, ask) = crate::gate::decide_card(&cfg, &store, &req, owner, now).unwrap();
        let block = "CREATE TRIGGER no_ref BEFORE UPDATE OF provider_ref ON cards \
                     BEGIN SELECT RAISE(ABORT, 'no ref'); END";
        store.db_for_test().execute_batch(block).unwrap();
        let asker = Asker {
            channel: "chat".into(),
            sender: "o".into(),
            session: "main".into(),
        };
        assert!(issue(&cfg, checked, &ask, &asker, None, None)
            .await
            .is_err());
        assert_eq!(store.payments_since(now - 600, "CAD").unwrap().0, 1);
        let log = Fake::script(&cfg, |s| s.log.clone());
        assert!(log.iter().any(|l| l.starts_with("close ")), "{log:?}");
    }

    /// An approved request that issued nothing ends `failed` even when its notice cannot be
    /// written then; the notice is owed and written on a later tick.
    #[tokio::test]
    async fn an_approved_card_that_failed_ends_even_with_no_notice() {
        let dir = crate::test_dir("cards-approved-owed");
        let text = format!(
            "rpi = '/r'\nhome = {dir:?}\n[approvals]\ncurrency = 'CAD'\nmoney = 'ask'\n\
             [cards]\nprovider = 'fake'\n"
        );
        let mut cfg: Config = toml::from_str(&text).unwrap();
        cfg.cards_fake = true;
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let cfg = Arc::new(cfg);
        let store = Store::open(&cfg.db_path()).unwrap();
        let req = crate::gate::Request {
            session: "main".into(),
            call_id: Some("c1".into()),
            tool: "card".into(),
            args: serde_json::json!({"action": "request", "site": "https://shop.example/pay",
                "merchant": "Shop", "amount": "10", "currency": "CAD"}),
            browser: None,
            run: "main:1".into(),
            reason: None,
        };
        let owner = ("chat".to_string(), "o".to_string());
        let held = crate::gate::decide(&cfg, &store, &req, owner, crate::now());
        let crate::gate::Verdict::Hold { id, .. } = held else {
            panic!("{held:?}")
        };
        let a = store.take_approved(id).unwrap().unwrap();
        let (checked, ask) = crate::gate::approved_card(&cfg, &store, &a).unwrap();
        Fake::script(&cfg, |s| s.fail_issue = Some("declined".into()));
        let db = store.db_for_test();
        let block = "CREATE TRIGGER no_outbox BEFORE INSERT ON outbox \
                     BEGIN SELECT RAISE(ABORT, 'no outbox'); END";
        db.execute_batch(block).unwrap();
        let before = store.pending_outbox().unwrap().len();
        issue_approved(&cfg, checked, ask, &a);
        let status = || store.action(id).unwrap().unwrap().status;
        for _ in 0..100 {
            if status() != "running" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert_eq!(status(), "failed");
        db.execute_batch("DROP TRIGGER no_outbox").unwrap();
        store.retry_owed();
        let out = store.pending_outbox().unwrap();
        assert_eq!(out.len(), before + 1);
        assert!(
            out.last().unwrap().text.contains("was not issued"),
            "{:?}",
            out.last()
        );
    }

    /// A close asked with a copy read before the issue landed closes what the issuer made: the
    /// reference is read from the record, not from the copy.
    #[tokio::test]
    async fn a_close_with_a_stale_copy_closes_the_card_the_issue_made() {
        let dir = crate::test_dir("cards-stale-close");
        let text = format!("rpi = '/r'\nhome = {dir:?}\n[cards]\nprovider = 'fake'\n");
        let mut cfg: Config = toml::from_str(&text).unwrap();
        cfg.cards_fake = true;
        let store = Store::open(&cfg.db_path()).unwrap();
        let new = NewCard {
            provider: "fake".into(),
            origin: "https://shop.example".into(),
            currency: "CAD".into(),
            cap_minor: 1000,
            expires_at: crate::now() + 900,
            ..NewCard::default()
        };
        let id = store.add_card(&new).unwrap();
        let mut stale = store.card(id).unwrap().unwrap();
        assert!(store.card_issued(id, "fake-1", None).unwrap());
        assert_eq!(close_at_issuer(&cfg, &mut stale).await, Ok(()));
        let log = Fake::script(&cfg, |s| s.log.clone());
        assert_eq!(log, [format!("close {id}")]);
    }
}
