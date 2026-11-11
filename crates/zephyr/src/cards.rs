//! Virtual cards (DESIGN §4.22): the agent asks for a card for one checkout, the gate holds it as
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
             card put N"
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

    /// Rewrites the shared list after a change: one `card<TAB>number` line per kept number.
    fn publish(all: &[Held]) {
        use std::os::unix::fs::FileExt;
        let shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
        let Some(file) = shared.as_ref() else { return };
        let text: String = (all.iter())
            .map(|h| format!("{}\t{}\n", h.card, h.pan.number))
            .collect();
        // Shorter first, so a reader never takes an old tail for a number.
        let _ = file
            .set_len(0)
            .and_then(|()| file.write_all_at(text.as_bytes(), 0));
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
        let before = all.len();
        all.retain(|h| h.drop_at.is_none_or(|at| at > now));
        if all.len() != before {
            publish(&all);
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
    let mut out = crate::facts::scrub_as(&facts, text, &label, &withheld);
    for (card, pan) in &all {
        out = scrub_code(&out, &pan.cvc, &format!("[card:#{card}]"));
    }
    out
}

/// What a log scrubber replaces, from the shared list's text (`live::shared`): each number as
/// plain digits, in groups of four with spaces or dashes, and its base64 forms, to its card's
/// label.
/// shortcut: a stream cannot tell a three-digit code from part of a longer number, so the
/// code alone is left in a log; the answer the agent gets has it scrubbed (`scrub`).
pub fn stream_pairs(shared: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs = Vec::new();
    for line in shared.lines() {
        let Some((card, number)) = line.split_once('\t') else {
            continue;
        };
        let digits = number.bytes().all(|b| b.is_ascii_digit());
        if !digits || number.len() < 12 || card.parse::<i64>().is_err() {
            continue;
        }
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

fn tell(store: &Store, card: &Card, text: &str) {
    let told = match card.channel.is_empty() {
        true => store.notify_last_owner(text),
        false => store.notify(&card.channel, &card.sender, text),
    };
    if let Err(e) = told {
        eprintln!(
            "zephyr: telling the owner about card #{} failed: {e}",
            card.id
        );
    }
}

fn note(store: &Store, card: &Card, word: &str, text: &str) {
    if let Err(e) = store.card_note(card.id, word, text) {
        eprintln!(
            "zephyr: telling the agent about card #{} failed: {e}",
            card.id
        );
    }
}

fn update(store: &Store, card: &Card) {
    if let Err(e) = store.update_card(card) {
        eprintln!("zephyr: recording card #{} failed: {e}", card.id);
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
/// issued. Nothing is retried by itself.
pub async fn issue(
    cfg: &Config,
    checked: Checked,
    ask: &CardAsk,
    asker: &Asker,
    proposal: Option<i64>,
    reason: Option<&str>,
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
    card.provider_ref = Some(issued.provider_ref);
    card.approval_url = issued.approval_url;
    update(&db.get(), &card);
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
                "[zephyr] card #{id}: make a card for this purchase: a new digital card with \
                 about {cap} behind it, or a prepaid virtual card loaded with {cap}; never your \
                 main card. Then enter it on the host: zephyr card put {id}. It is filled only \
                 on {}, and forgotten {mins} min after you enter it; delete it after the charge",
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
            tokio::spawn(async move {
                let text = fill(&cfg, id, fields).await;
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
        if let Err(e) = store.finish_action(id, status, &result) {
            eprintln!("zephyr: recording how #{id} ended failed: {e}");
        }
        if let Err(why) = done {
            let text = format!("the owner approved #{id}, but {why}");
            let _ = store.action_note(id, "not run", &text);
            let _ = match asker.channel.is_empty() {
                true => store.notify_last_owner(&format!("[zephyr] {why}")),
                false => store.notify(&asker.channel, &asker.sender, &format!("[zephyr] {why}")),
            };
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

/// What a hand-made card is, as the owner says when entering it.
pub const KINDS: [&str; 3] = ["new", "prepaid", "other"];

/// The owner's `/card` on `channel` (`strong` or not), from chat line `msg_id`: `list`,
/// `put N [KIND]` (the number comes from memory, `entered`), `done N`, `close N`. `None` for
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
            let kind = kind.first().copied().unwrap_or("other");
            let card = match found(n) {
                Ok(card) => card,
                Err(e) => return Some(e),
            };
            if !strong {
                let strong = cfg.settings.strong_channels.join(" or ");
                return Some(format!("[zephyr] entering a card needs {strong}"));
            }
            if !KINDS.contains(&kind) {
                return Some(format!("[zephyr] a card is {}", KINDS.join(", ")));
            }
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
            Ok(card) => {
                let from = ["waiting", "open", "charging", "unknown"];
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
                let cfg = cfg.clone();
                tokio::spawn(async move {
                    if let Ok(db) = Db::open(&cfg) {
                        close(&cfg, &db, &card, "the owner closed it", crate::now()).await;
                    }
                });
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
    live::put(cfg.state_dir(), card.id, pan);
    update(store, &card);
    audit(cfg, store, &card, "reveal", "entered by the owner");
    note(store, &card, "issued", &ready(cfg, &card));
    // What the owner said it is decides what bounds a leaked number.
    let bound = match kind {
        "new" => "its cap is the balance behind it",
        "prepaid" => "its cap is what you loaded",
        _ => "zephyr cannot tell what bounds it: keep the balance behind it low",
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

/// Types card `id` into the page open in agent-browser, once that page is on the card's own
/// origin: the address is read before the number is fetched and again after each field, as the
/// vault's fill does. The answer names the fields and the last four digits, never a value.
async fn fill(cfg: &Config, id: i64, fields: CardFields) -> String {
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
    if selectors
        .iter()
        .any(|(_, s)| s.as_deref().is_some_and(|s| s.starts_with('-')))
    {
        return "not filled: a selector cannot start with '-'".into();
    }
    let page = match crate::vault::browser(cfg, &["get", "url"]).await {
        Ok(out) => crate::vault::page_address(&out).map(String::from),
        Err(e) => return format!("not filled: reading the page's address failed: {e}"),
    };
    let Some(page) = page
        .clone()
        .filter(|p| crate::vault::same_origin(&card.origin, p))
    else {
        let on = page
            .as_deref()
            .and_then(crate::vault::origin)
            .map_or("no http(s) page".into(), |o| o.ascii_serialization());
        audit(
            cfg,
            &db.get(),
            &card,
            "refused",
            &format!("the page was on {on}"),
        );
        return format!(
            "not filled: the open page is on {on}, but card #{id} is only for {}",
            card.origin
        );
    };
    // Only now, on the card's own origin, is the number fetched.
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
                    update(&db.get(), &card);
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
    let mut done: Vec<&str> = Vec::new();
    for (label, sel) in selectors {
        let Some(sel) = sel else { continue };
        let Some(value) = value(label) else {
            return format!("not filled: card #{id} has no {label}; leave `{label}` out");
        };
        if let Err(e) = crate::vault::browser(cfg, &["fill", sel, &value]).await {
            let done = match done.is_empty() {
                true => String::new(),
                false => format!("filled the {}, ", done.join(" and ")),
            };
            audit(cfg, &db.get(), &card, "fill", "failed");
            return crate::vault::scrubbed(cfg, format!("{done}but not the {label}: {e}"));
        }
        done.push(label);
        let now = crate::vault::page_now(cfg).await;
        if let Some(stop) = crate::vault::moved(&card.origin, now.as_deref(), &done) {
            audit(cfg, &db.get(), &card, "fill", "the page moved away");
            return stop;
        }
    }
    if done.is_empty() {
        return "not filled: give the CSS selector of at least one of number, expiry, month, \
                year, cvc, name"
            .into();
    }
    audit(cfg, &db.get(), &card, "fill", "ok");
    format!(
        "filled the {} of card #{id} (…{}) on {page}",
        done.join(" and "),
        pan.last4()
    )
}

/// Closes `card` at the issuer and forgets its number. A close the issuer refuses leaves the
/// card as it is, to be tried again on a later tick, and the owner is told once.
async fn close(cfg: &Config, db: &Db, card: &Card, why: &str, now: i64) {
    let closed = match issuer(cfg) {
        Ok(issuer) => issuer.close(card).await.map_err(|e| e.0),
        Err(e) => Err(e),
    };
    // Whatever the issuer said, the number is never filled again.
    live::forget(cfg.state_dir(), card.id, now);
    if let Err(e) = closed {
        audit(cfg, &db.get(), card, "close", &e);
        // Past its time from now on: no fill fetches its number again, and each later tick
        // tries the close again.
        let mut told = card.clone();
        told.expires_at = card.expires_at.min(now);
        update(&db.get(), &told);
        if card.told_at.is_none() {
            told.told_at = Some(now);
            update(&db.get(), &told);
            let e = crate::vault::scrubbed(cfg, e);
            let text = format!(
                "[zephyr] card #{} could not be closed at the issuer: {e}; close it in the \
                 provider's app",
                card.id
            );
            tell(&db.get(), card, &text);
        }
        return;
    }
    // A hand-made card is the owner's to delete: until they say so, it is not known closed.
    let to = match card.provider == "manual" && card.last4.is_some() {
        true => "unknown",
        false => "closed",
    };
    if !db
        .get()
        .move_card(card.id, &["waiting", "open", "charging"], to, now)
        .unwrap_or(false)
    {
        return;
    }
    audit(cfg, &db.get(), card, "close", why);
    note(
        &db.get(),
        card,
        "closed",
        &format!("{why}; the card is closed and cannot be filled"),
    );
    if to == "unknown" {
        let mut told = card.clone();
        (told.status, told.told_at, told.closed_at) = ("unknown".into(), Some(now), Some(now));
        update(&db.get(), &told);
        tell(&db.get(), card, &delete_it(card, why));
    } else {
        tell(
            &db.get(),
            card,
            &format!("[zephyr] card #{} is closed: {why}", card.id),
        );
    }
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

/// The router's tick for cards: numbers past their hour are dropped; a card past its expiry is
/// closed; every 30 s each waiting or open card's issuer is asked, and the card is opened once
/// the owner approved it, or closed at the first charge it reports; a hand-made card the
/// owner has not confirmed deleted is recalled once a day.
pub async fn tick(cfg: &Config, now: i64) {
    live::sweep(now);
    let Ok(db) = Db::open(cfg) else { return };
    let forgotten = db.get().cards_in(&["unknown"]).unwrap_or_default();
    for card in forgotten {
        if card.provider == "manual" && card.told_at.is_none_or(|at| at + 86_400 <= now) {
            let mut told = card.clone();
            told.told_at = Some(now);
            update(&db.get(), &told);
            tell(&db.get(), &card, &delete_it(&card, "it is past its time"));
        }
    }
    let live = db
        .get()
        .cards_in(&["waiting", "open", "charging"])
        .unwrap_or_default();
    for mut card in live {
        if now >= card.expires_at {
            // Forgotten at once; the issuer is asked to close it at its own pace.
            live::forget(cfg.state_dir(), card.id, now);
            if poll_due(cfg, card.id) {
                // Only an issuer that reports charges can say it went unused.
                let why = match caps(cfg).is_some_and(|c| c.charges) {
                    true => "expired unused",
                    false => "it is past its time",
                };
                close(cfg, &db, &card, why, now).await;
            }
            continue;
        }
        if !poll_due(cfg, card.id) {
            continue;
        }
        let state = match issuer(cfg) {
            Ok(issuer) => issuer.state(&card).await,
            Err(e) => Err(ProviderError(e)),
        };
        match (state, card.status.as_str()) {
            (Ok(CardState::Open), "waiting") if card.provider != "manual" => {
                card.status = "open".into();
                card.expires_at = now + 60 * i64::from(minutes(cfg));
                update(&db.get(), &card);
                audit(cfg, &db.get(), &card, "issue", "approved at the issuer");
                note(&db.get(), &card, "issued", &ready(cfg, &card));
            }
            (Ok(CardState::Used(charges)), _) => used(cfg, &db, card, &charges, now).await,
            (Ok(CardState::Closed), _) => {
                live::forget(cfg.state_dir(), card.id, now);
                if db
                    .get()
                    .move_card(card.id, &["waiting", "open", "charging"], "closed", now)
                    .unwrap_or(false)
                {
                    audit(cfg, &db.get(), &card, "close", "closed at the issuer");
                    note(
                        &db.get(),
                        &card,
                        "closed",
                        "the issuer closed it; it cannot be filled",
                    );
                }
            }
            (Ok(CardState::Failed(why)), _) => {
                live::forget(cfg.state_dir(), card.id, now);
                if db
                    .get()
                    .move_card(card.id, &["waiting", "open", "charging"], "failed", now)
                    .unwrap_or(false)
                {
                    let why = crate::vault::scrubbed(cfg, why);
                    audit(cfg, &db.get(), &card, "issue", &why);
                    note(
                        &db.get(),
                        &card,
                        "failed",
                        &format!("no card: {why}; ask again if it is still wanted"),
                    );
                    tell(
                        &db.get(),
                        &card,
                        &format!("[zephyr] card #{} was not issued: {why}", card.id),
                    );
                }
            }
            (Ok(_), _) => {}
            (Err(e), _) => eprintln!(
                "zephyr: asking the issuer about card #{} failed: {e}",
                card.id
            ),
        }
    }
}

/// The issuer reported `charges` on `card`: recorded, the card closed, the owner and the agent
/// told who charged it; a charge the card should not have taken is told at once.
async fn used(cfg: &Config, db: &Db, mut card: Card, charges: &[Charge], now: i64) {
    let Some(first) = charges.first() else { return };
    if !db
        .get()
        .move_card(card.id, &["waiting", "open", "charging"], "used", now)
        .unwrap_or(false)
    {
        return;
    }
    card.status = "used".into();
    card.closed_at = Some(now);
    card.charged_minor = Some(first.minor);
    card.charged_currency = Some(first.currency.clone());
    card.merchant_id.clone_from(&first.merchant_id);
    card.descriptor.clone_from(&first.descriptor);
    card.mcc.clone_from(&first.mcc);
    update(&db.get(), &card);
    let what = charged(first);
    audit(cfg, &db.get(), &card, "charge", &what);
    let over = first.minor > card.cap_minor || first.currency != card.currency;
    if over || charges.len() > 1 {
        let all: Vec<String> = charges.iter().map(charged).collect();
        let text = format!(
            "[zephyr] card #{} took what you did not approve (up to {}, once): {}; the issuer \
             should have declined it",
            card.id,
            crate::approvals::money(card.cap_minor, &card.currency),
            all.join("; ")
        );
        tell(&db.get(), &card, &text);
    }
    // Closed at the issuer too; a refusal there is told and tried no further: it is used.
    let closed = match issuer(cfg) {
        Ok(issuer) => issuer.close(&card).await.map_err(|e| e.0),
        Err(e) => Err(e),
    };
    live::forget(cfg.state_dir(), card.id, now);
    let end = match closed {
        Ok(()) => "the card is closed".to_string(),
        Err(e) => {
            audit(cfg, &db.get(), &card, "close", &e);
            "closing it at the issuer failed; close it in the provider's app".to_string()
        }
    };
    tell(
        &db.get(),
        &card,
        &format!("[zephyr] card #{} was {what}; {end}", card.id),
    );
    note(&db.get(), &card, "used", &format!("{what}; {end}"));
}

/// `/stop`, `/pause` and a start: every card still waiting or open is closed, off the router's
/// loop. Their numbers are forgotten before this returns.
pub fn close_all(cfg: &Arc<Config>, why: &'static str) {
    let Ok(store) = open_store(cfg) else { return };
    let cards = store
        .cards_in(&["waiting", "open", "charging"])
        .unwrap_or_default();
    if cards.is_empty() {
        return;
    }
    let now = crate::now();
    for card in &cards {
        live::forget(cfg.state_dir(), card.id, now);
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        let Ok(db) = Db::open(&cfg) else { return };
        for card in cards {
            close(&cfg, &db, &card, why, now).await;
        }
    });
}

static TICKING: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// The router's tick: runs [`tick`] off its loop when there is a card to look at, one run at a
/// time for a gateway.
pub fn tick_off(cfg: &Arc<Config>, store: &Store) {
    let now = crate::now();
    live::sweep(now);
    let live = ["waiting", "open", "charging", "unknown"];
    if store.cards_in(&live).map_or(true, |c| c.is_empty()) {
        return;
    }
    let state = cfg.state_dir().to_path_buf();
    {
        let mut ticking = TICKING.lock().unwrap_or_else(|e| e.into_inner());
        if ticking.contains(&state) {
            return;
        }
        ticking.push(state.clone());
    }
    let cfg = cfg.clone();
    tokio::spawn(async move {
        tick(&cfg, now).await;
        let mut ticking = TICKING.lock().unwrap_or_else(|e| e.into_inner());
        ticking.retain(|s| *s != state);
    });
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
        let pairs = stream_pairs("7\t5555555555554444\nbroken\nx\t4242424242424242\n3\t12\n");
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
        let text = std::fs::read_to_string(format!("/proc/self/fd/{fd}")).unwrap();
        assert!(text.contains("9\t6011000990139424\n"), "{text}");
        live::forget(&state, 9, -KEEP_SECS);
        live::sweep(1);
        let text = std::fs::read_to_string(format!("/proc/self/fd/{fd}")).unwrap();
        assert!(!text.contains("6011000990139424"), "{text}");
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
                "ok   cards: manual, no key needed: you make each card and enter it with zephyr card put N",
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
}
