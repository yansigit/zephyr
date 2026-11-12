//! Approval modes and presets (approvals spec §4.3): what each gated class (messages, posts,
//! money) and the private-facts check do, the three presets, and how `[approvals]` reads. Pure:
//! the registry (`settings.rs`) parses and orders the values through here, the router's
//! `set_setting` writes a preset as one change (`preset_edits`, and `snapshot`/`restore_edits`
//! for a timed one), and the gate reads `effective` on every decision.

use std::fmt;

use zephyr_policy::policy::Class;

use crate::config::{Approvals, Config};

/// How a gated class waits for the owner. Ruling R1: no derived order (it would put `Delay(10)`
/// looser than `Delay(5)`); strictness is [`stricter`] alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Hold every one.
    Ask,
    /// Hold unless every target was approved before (messages and posts).
    AskNew,
    /// Run after N minutes unless cancelled; a longer delay is stricter.
    Delay(u32),
    Auto,
    /// Money: allow a known amount in `approvals.currency` up to the cap, in cents; a lower cap
    /// is stricter.
    AutoCap(u64),
}

/// What a call holding a private fact does. Strictest first, so the derived order is the right
/// one (R1).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum FactsMode {
    Hold,
    Warn,
    Off,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    Manual,
    Semi,
    Full,
}

/// What `[approvals]` comes to: each class's mode (the hand-set value, else the preset's), and
/// whether a hand-set value differs from the preset (`semi (modified)`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Effective {
    pub messages: Mode,
    pub posts: Mode,
    pub money: Mode,
    pub facts: FactsMode,
    pub preset: Preset,
    pub modified: bool,
}

/// The longest `delay N`, in minutes.
const MAX_DELAY: u32 = 60;
/// The highest `auto N` cap, in whole currency units.
const MAX_CAP: u64 = 1_000_000_000;

/// The `[approvals]` keys a preset sets, in the order `/approvals` shows them.
const PARTS: [&str; 4] = ["messages", "posts", "money", "facts"];

/// The same keys, dotted, as the registry names them.
pub const CLASS_KEYS: [&str; 4] = [
    "approvals.messages",
    "approvals.posts",
    "approvals.money",
    "approvals.facts",
];

/// A connector tool's class names (`mcp.NAME.classes.TOOL`); `none` takes the tool out of
/// every class. Unordered: which class is stricter depends on the modes, so the registry counts
/// only a move into a class from `none` as tightening (`Stricter::IntoClass`).
pub const CLASS_CHOICES: [&str; 4] = ["messages", "posts", "money", "none"];

/// What `approvals.strong_only` may name.
pub const STRONG_ONLY: [&str; 2] = ["loosen", "money"];

impl Preset {
    /// Strictest first, the registry's order.
    pub const NAMES: [&'static str; 3] = ["manual", "semi", "full"];

    pub fn parse(raw: &str) -> Result<Preset, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "manual" => Ok(Preset::Manual),
            "semi" => Ok(Preset::Semi),
            "full" => Ok(Preset::Full),
            _ => Err(format!(
                "preset is manual, semi or full, not {:?}",
                raw.trim()
            )),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Preset::Manual => "manual",
            Preset::Semi => "semi",
            Preset::Full => "full",
        }
    }

    /// What the preset sets, from the table below: `messages ask · posts ask · money ask · facts
    /// hold` for the owner choosing one (`zephyr init`).
    pub fn summary(self) -> String {
        let shown = |part| match self.value(part) {
            Val::Mode(m) => m.to_string(),
            Val::Facts(f) => f.name().to_string(),
        };
        format!(
            "messages {} · posts {} · money {} · facts {}",
            shown("messages"),
            shown("posts"),
            shown("money"),
            shown("facts")
        )
    }

    /// The preset's own value for `part`: spec §4.3's table.
    fn value(self, part: &str) -> Val {
        use Mode::*;
        let (messages, posts, money, facts) = match self {
            Preset::Manual => (Ask, Ask, Ask, FactsMode::Hold),
            Preset::Semi => (AskNew, Delay(5), Ask, FactsMode::Hold),
            Preset::Full => (Auto, Auto, Auto, FactsMode::Warn),
        };
        match part {
            "messages" => Val::Mode(messages),
            "posts" => Val::Mode(posts),
            "money" => Val::Mode(money),
            _ => Val::Facts(facts),
        }
    }
}

impl FactsMode {
    /// Strictest first, the registry's order.
    pub const NAMES: [&'static str; 3] = ["hold", "warn", "off"];

    pub fn parse(raw: &str) -> Result<FactsMode, String> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "hold" => Ok(FactsMode::Hold),
            "warn" => Ok(FactsMode::Warn),
            "off" => Ok(FactsMode::Off),
            _ => Err(format!("facts is hold, warn or off, not {:?}", raw.trim())),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FactsMode::Hold => "hold",
            FactsMode::Warn => "warn",
            FactsMode::Off => "off",
        }
    }
}

/// As the owner types it: `ask`, `ask-new`, `delay 5`, `auto`, `auto 49.99`.
impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mode::Ask => f.write_str("ask"),
            Mode::AskNew => f.write_str("ask-new"),
            Mode::Delay(n) => write!(f, "delay {n}"),
            Mode::Auto => f.write_str("auto"),
            Mode::AutoCap(c) if c % 100 == 0 => write!(f, "auto {}", c / 100),
            Mode::AutoCap(c) => write!(f, "auto {}.{:02}", c / 100, c % 100),
        }
    }
}

impl Effective {
    /// `/approvals`: `preset semi (modified) · messages ask-new · posts delay 5 · money ask ·
    /// facts hold`.
    pub fn line(&self) -> String {
        let modified = if self.modified { " (modified)" } else { "" };
        format!(
            "preset {}{modified} · messages {} · posts {} · money {} · facts {}",
            self.preset.name(),
            self.messages,
            self.posts,
            self.money,
            self.facts.name()
        )
    }
}

pub fn class_name(class: Class) -> &'static str {
    match class {
        Class::Messages => "messages",
        Class::Posts => "posts",
        Class::Money => "money",
    }
}

/// What a class's mode may be, in words.
pub fn takes(class: Class) -> &'static str {
    match class {
        Class::Money => "ask, delay N (1 to 60 minutes), auto or auto N (a cap)",
        _ => "ask, ask-new, delay N (1 to 60 minutes) or auto",
    }
}

/// The forms a class's mode takes, for an editor to offer.
pub fn forms(class: Class) -> &'static [&'static str] {
    match class {
        Class::Money => &["ask", "delay N", "auto", "auto N"],
        _ => &["ask", "ask-new", "delay N", "auto"],
    }
}

/// `raw` as a mode of `class`: `ask`, `ask-new` (messages and posts), `delay N` (1 to 60
/// minutes), `auto`, and for money `auto N`, a cap of N in `approvals.currency` with at most
/// two decimals. Case and spacing do not matter.
pub fn parse_mode(class: Class, raw: &str) -> Result<Mode, String> {
    let lower = raw.to_ascii_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let mode = match (words.as_slice(), class) {
        (["ask"], _) => Some(Mode::Ask),
        (["ask-new"], Class::Messages | Class::Posts) => Some(Mode::AskNew),
        (["delay", n], _) if digits(n) => (n.parse().ok())
            .filter(|n| (1..=MAX_DELAY).contains(n))
            .map(Mode::Delay),
        (["auto"], _) => Some(Mode::Auto),
        (["auto", cap], Class::Money) => cents(cap).map(Mode::AutoCap),
        _ => None,
    };
    mode.ok_or_else(|| {
        format!(
            "{} is {}, not {:?}",
            class_name(class),
            takes(class),
            raw.trim()
        )
    })
}

/// A cap as cents: whole units with up to two decimals, above 0 and at most `MAX_CAP` units.
fn cents(raw: &str) -> Option<u64> {
    let (units, frac) = raw.split_once('.').unwrap_or((raw, "00"));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(units) || !digits(frac) || frac.len() > 2 {
        return None;
    }
    let units: u64 = units.parse().ok().filter(|u| *u <= MAX_CAP)?;
    let frac: u64 = format!("{frac:0<2}").parse().ok()?;
    Some(units * 100 + frac).filter(|c| *c > 0)
}

/// Whether `a` is stricter than `b`: `ask` < `ask-new` < `delay N` (a larger N stricter) <
/// `auto N` (a lower cap stricter) < `auto`. Equal modes are neither.
pub fn stricter(a: Mode, b: Mode) -> bool {
    fn rank(m: Mode) -> (u8, u64) {
        match m {
            Mode::Ask => (0, 0),
            Mode::AskNew => (1, 0),
            Mode::Delay(n) => (2, u64::from(u32::MAX - n)),
            Mode::AutoCap(cents) => (3, cents),
            Mode::Auto => (4, 0),
        }
    }
    rank(a) < rank(b)
}

/// One value a preset sets: a class's mode or the facts mode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Val {
    Mode(Mode),
    Facts(FactsMode),
}

impl Val {
    fn parse(part: &str, raw: &str) -> Result<Val, String> {
        match part {
            "messages" => parse_mode(Class::Messages, raw).map(Val::Mode),
            "posts" => parse_mode(Class::Posts, raw).map(Val::Mode),
            "money" => parse_mode(Class::Money, raw).map(Val::Mode),
            _ => FactsMode::parse(raw).map(Val::Facts),
        }
    }

    /// `raw` as `part` reads it, an unreadable value as the strictest (Review Focus 4).
    fn read(part: &str, raw: &str) -> Val {
        Val::parse(part, raw).unwrap_or(match part {
            "facts" => Val::Facts(FactsMode::Hold),
            _ => Val::Mode(Mode::Ask),
        })
    }

    fn looser(self, than: Val) -> bool {
        match (self, than) {
            (Val::Mode(a), Val::Mode(b)) => stricter(b, a),
            (Val::Facts(a), Val::Facts(b)) => a > b,
            _ => true,
        }
    }

    fn text(self) -> String {
        match self {
            Val::Mode(m) => m.to_string(),
            Val::Facts(f) => f.name().into(),
        }
    }
}

fn hand<'a>(a: Option<&'a Approvals>, part: &str) -> Option<&'a str> {
    let a = a?;
    match part {
        "messages" => a.messages.as_deref(),
        "posts" => a.posts.as_deref(),
        "money" => a.money.as_deref(),
        _ => a.facts.as_deref(),
    }
}

fn key(part: &str) -> String {
    format!("approvals.{part}")
}

/// The preset `cfg` names: `manual` when it names none, or one that does not parse (the
/// strictest, Review Focus 4).
fn preset(cfg: &Config) -> Preset {
    let named = cfg.approvals.as_ref().and_then(|a| a.preset.as_deref());
    named.map_or(Preset::Manual, |p| {
        Preset::parse(p).unwrap_or(Preset::Manual)
    })
}

/// The modes in effect: each class's hand-set value, else the preset's. A value that does not
/// parse (a hand edit the reload refused that reached here anyway) counts as the strictest,
/// `ask` or `hold`, never as the preset's.
pub fn effective(cfg: &Config) -> Effective {
    let a = cfg.approvals.as_ref();
    let preset = preset(cfg);
    let mut modified = false;
    let mut get = |part: &str| {
        let own = preset.value(part);
        let v = hand(a, part).map_or(own, |raw| Val::read(part, raw));
        modified |= v != own;
        v
    };
    let mode = |v: Val| match v {
        Val::Mode(m) => m,
        Val::Facts(_) => Mode::Ask,
    };
    let (messages, posts, money) = (
        mode(get("messages")),
        mode(get("posts")),
        mode(get("money")),
    );
    let facts = match get("facts") {
        Val::Facts(f) => f,
        Val::Mode(_) => FactsMode::Hold,
    };
    Effective {
        messages,
        posts,
        money,
        facts,
        preset,
        modified,
    }
}

/// The keys a change to `preset` writes (`Some`) or clears (`None`), in one update: the preset,
/// and every hand-set value looser than the new preset's, dropped. One no looser stays, so a
/// preset never loosens what the owner set by hand. An unreadable value counts as the strictest
/// and stays, for validation to refuse.
pub fn preset_edits(cfg: &Config, preset: Preset) -> Vec<(String, Option<String>)> {
    let a = cfg.approvals.as_ref();
    let mut edits = vec![(key("preset"), Some(preset.name().to_string()))];
    for part in PARTS {
        if let Some(raw) = hand(a, part) {
            if Val::read(part, raw).looser(preset.value(part)) {
                edits.push((key(part), None));
            }
        }
    }
    edits
}

/// What a timed preset goes back to: the preset as the file names it (`default` when it names
/// none) and each hand-set class, as `semi; messages=ask; facts=off`.
pub fn snapshot(cfg: &Config) -> String {
    snapshot_of(cfg.approvals.as_ref())
}

fn snapshot_of(a: Option<&Approvals>) -> String {
    let mut out = (a.and_then(|a| a.preset.as_deref()))
        .unwrap_or("default")
        .trim()
        .to_string();
    for part in PARTS {
        if let Some(raw) = hand(a, part) {
            out += &format!("; {part}={}", raw.trim());
        }
    }
    out
}

/// `snapshot`, with each `approvals.<class>` in `instead` saved as the value given (`None`:
/// absent) rather than the file's. R31: a class whose own time limit a preset ends is saved as
/// what that limit would have put back, so the preset's revert never brings back the timed
/// value with no limit on it.
pub fn snapshot_with(cfg: &Config, instead: &[(String, Option<String>)]) -> String {
    let mut a = cfg.approvals.clone().unwrap_or_default();
    for (key, value) in instead {
        let slot = match key.strip_prefix("approvals.") {
            Some("messages") => &mut a.messages,
            Some("posts") => &mut a.posts,
            Some("money") => &mut a.money,
            Some("facts") => &mut a.facts,
            _ => continue,
        };
        *slot = value.clone();
    }
    snapshot_of(Some(&a))
}

/// The edits that put `snapshot` back over `cfg`: the preset and every class as they were.
/// A class set since to something stricter than what would come back stays, so the end of a
/// time limit never loosens what was set after it.
pub fn restore_edits(
    cfg: &Config,
    snapshot: &str,
) -> Result<Vec<(String, Option<String>)>, String> {
    let bad = || format!("{snapshot:?} is not a saved [approvals]");
    let mut items = snapshot.split(';').map(str::trim);
    let preset = match items.next().ok_or_else(bad)? {
        "default" => None,
        p => Some(Preset::parse(p).map_err(|_| bad())?),
    };
    let mut then: Vec<(&str, &str)> = Vec::new();
    for item in items {
        let (part, raw) = item.split_once('=').ok_or_else(bad)?;
        let (part, raw) = (part.trim(), raw.trim());
        if !PARTS.contains(&part) || then.iter().any(|(p, _)| *p == part) {
            return Err(bad());
        }
        Val::parse(part, raw).map_err(|_| bad())?;
        then.push((part, raw));
    }
    let restored = preset.unwrap_or(Preset::Manual);
    let a = cfg.approvals.as_ref();
    let mut edits = vec![(key("preset"), preset.map(|p| p.name().to_string()))];
    for part in PARTS {
        let back = then.iter().find(|(p, _)| *p == part).map(|(_, raw)| *raw);
        let gives = back.map_or(restored.value(part), |raw| Val::read(part, raw));
        let value = match hand(a, part) {
            Some(now) if Some(now) != back && gives.looser(Val::read(part, now)) => Some(now),
            _ => back,
        };
        edits.push((key(part), value.map(String::from)));
    }
    Ok(edits)
}

/// `approvals.<part>` as `/settings` shows it: the preset, each class in effect, the currency
/// and what needs a strong channel. `None` for a key that is not one of them.
pub fn value(cfg: &Config, part: &str) -> Option<String> {
    let e = effective(cfg);
    Some(match part {
        "preset" => e.preset.name().into(),
        "messages" => e.messages.to_string(),
        "posts" => e.posts.to_string(),
        "money" => e.money.to_string(),
        "facts" => e.facts.name().into(),
        "currency" => currency(cfg),
        "strong_only" => strong_only(cfg).join(", "),
        "window.effects" => window(cfg).effects.to_string(),
        // A plain number, as `/set` takes it.
        "window.amount" => {
            let scale = 10f64.powi(exponent(&currency(cfg)) as i32);
            (window(cfg).amount_minor? as f64 / scale).to_string()
        }
        "window.seconds" => window(cfg).seconds.to_string(),
        _ => return None,
    })
}

/// What `approvals.<part>` comes to once cleared: the preset's value for a class.
pub fn cleared(cfg: &Config, part: &str) -> Option<String> {
    PARTS
        .contains(&part)
        .then(|| preset(cfg).value(part).text())
}

/// The currency `auto N` counts in: `approvals.currency`, else USD.
pub fn currency(cfg: &Config) -> String {
    (cfg.approvals.as_ref().and_then(|a| a.currency.as_deref()))
        .map_or_else(|| "USD".into(), |c| c.trim().to_ascii_uppercase())
}

/// The payment window in effect (DESIGN §4.25): `amount_minor` in `approvals.currency`'s minor
/// unit, `None` for no amount limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowNow {
    pub effects: u32,
    pub amount_minor: Option<i64>,
    pub seconds: i64,
}

/// `[approvals.window]` with its defaults: 5 payments, the `auto N` cap as the amount when
/// there is one, 600 s.
pub fn window(cfg: &Config) -> WindowNow {
    let w = (cfg.approvals.as_ref().and_then(|a| a.window.clone())).unwrap_or_default();
    let currency = currency(cfg);
    let cap = match effective(cfg).money {
        // The cap counts in cents whatever the currency (`gate::within`).
        Mode::AutoCap(cents) => Some(minor_of(cents as f64 / 100.0, &currency)),
        _ => None,
    };
    WindowNow {
        effects: w.effects.unwrap_or(5),
        amount_minor: w.amount.map(|a| minor_of(a, &currency)).or(cap),
        seconds: i64::from(w.seconds.unwrap_or(600)),
    }
}

/// How many decimal places `currency`'s minor unit has (ISO 4217): 0 for the yen and the won,
/// 3 for the dinars, else 2.
pub fn exponent(currency: &str) -> u32 {
    match currency.to_ascii_uppercase().as_str() {
        "JPY" | "KRW" | "VND" | "CLP" | "ISK" | "HUF" | "XOF" | "XAF" | "PYG" | "UGX" => 0,
        "BHD" | "KWD" | "OMR" | "JOD" | "TND" => 3,
        _ => 2,
    }
}

/// `value` in `currency`'s minor unit, rounded up (as a cap counts it, `gate::within`).
pub fn minor_of(value: f64, currency: &str) -> i64 {
    let scale = 10f64.powi(exponent(currency) as i32);
    ((value * scale * 1e6).round() / 1e6).ceil() as i64
}

/// `42.50 CAD`, `500 JPY`: `minor` written in `currency`.
pub fn money(minor: i64, currency: &str) -> String {
    let e = exponent(currency) as usize;
    let scale = 10i64.pow(e as u32);
    match e {
        0 => format!("{minor} {currency}"),
        _ => format!("{}.{:0e$} {currency}", minor / scale, minor % scale),
    }
}

/// What only a strong channel may approve (spec §4.7): `approvals.strong_only`, else `loosen`.
pub fn strong_only(cfg: &Config) -> Vec<String> {
    (cfg.approvals.as_ref().and_then(|a| a.strong_only.clone()))
        .unwrap_or_else(|| vec!["loosen".into()])
}

/// `raw` as a currency code: three letters (ISO 4217), upper-cased.
pub fn currency_code(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    match raw.len() == 3 && raw.bytes().all(|b| b.is_ascii_alphabetic()) {
        true => Ok(raw.to_ascii_uppercase()),
        false => Err(format!(
            "currency is a three-letter ISO 4217 code such as USD, not {raw:?}"
        )),
    }
}

/// The class a connector's tool falls in by its name alone (the verb rule), as
/// `mcp.NAME.classes.TOOL` shows it when the owner set none.
pub fn verb_class(server: &str, tool: &str) -> &'static str {
    let call = serde_json::json!({"server": server, "tool": tool});
    let gated = zephyr_policy::policy::gated("mcp_call", &call, &|_, _| None);
    gated.map_or("none", |g| class_name(g.class))
}

/// `[approvals]` as `Config::validate` checks it: every value parses.
pub fn check(a: &Approvals) -> anyhow::Result<()> {
    let problem = |e: String| anyhow::anyhow!("[approvals] {e}");
    if let Some(p) = &a.preset {
        Preset::parse(p).map_err(problem)?;
    }
    for part in PARTS {
        if let Some(raw) = hand(Some(a), part) {
            Val::parse(part, raw).map_err(problem)?;
        }
    }
    if let Some(c) = &a.currency {
        currency_code(c).map_err(problem)?;
    }
    if let Some(w) = &a.window {
        if w.effects.is_some_and(|n| !(1..=100).contains(&n)) {
            return Err(problem("window.effects must be 1 to 100".into()));
        }
        if w.seconds.is_some_and(|n| !(60..=86_400).contains(&n)) {
            return Err(problem("window.seconds must be 60 to 86400".into()));
        }
        if w.amount.is_some_and(|a| !(a > 0.0 && a <= 1.0e9)) {
            return Err(problem("window.amount must be more than 0".into()));
        }
    }
    for name in a.strong_only.iter().flatten() {
        if !STRONG_ONLY.contains(&name.as_str()) {
            return Err(problem(format!(
                "strong_only names {name:?}; allowed: {}",
                STRONG_ONLY.join(", ")
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_defaults_follow_the_auto_cap() {
        let cfg = |extra: &str| -> Config {
            toml::from_str(&format!("rpi = '/r'\nhome = '/h'\n{extra}")).unwrap()
        };
        let none = WindowNow {
            effects: 5,
            amount_minor: None,
            seconds: 600,
        };
        assert_eq!(window(&cfg("")), none);
        let capped = cfg("[approvals]\nmoney = 'auto 200'\ncurrency = 'CAD'\n");
        assert_eq!(window(&capped).amount_minor, Some(20_000));
        let set = cfg(
            "[approvals]\nmoney = 'auto 200'\n[approvals.window]\neffects = 2\namount = 50.5\n\
             seconds = 60\n",
        );
        let w = WindowNow {
            effects: 2,
            amount_minor: Some(5050),
            seconds: 60,
        };
        assert_eq!(window(&set), w);
        assert_eq!(value(&set, "window.amount").as_deref(), Some("50.5"));
        assert_eq!(value(&cfg(""), "window.amount"), None);
        for bad in ["effects = 0", "effects = 101", "seconds = 59", "amount = 0"] {
            let c = cfg(&format!("[approvals.window]\n{bad}\n"));
            assert!(c.validate().is_err(), "{bad}");
        }
    }

    #[test]
    fn minor_units_follow_the_currency() {
        assert_eq!(minor_of(42.5, "CAD"), 4250);
        assert_eq!(minor_of(0.1, "USD"), 10);
        assert_eq!(
            minor_of(50.004, "USD"),
            5001,
            "rounded up, as a cap counts it"
        );
        assert_eq!(minor_of(500.0, "JPY"), 500);
        assert_eq!(minor_of(1.5, "KWD"), 1500);
        assert_eq!(money(4250, "CAD"), "42.50 CAD");
        assert_eq!(money(5, "CAD"), "0.05 CAD");
        assert_eq!(money(500, "JPY"), "500 JPY");
    }
    use FactsMode::{Hold, Off, Warn};
    use Mode::*;

    fn cfg(extra: &str) -> Config {
        toml::from_str(&format!("rpi = 'a'\nhome = 'b'\n{extra}")).unwrap()
    }

    fn edit(key: &str, value: Option<&str>) -> (String, Option<String>) {
        (key.to_string(), value.map(String::from))
    }

    fn modes(e: &Effective) -> (Mode, Mode, Mode, FactsMode) {
        (e.messages, e.posts, e.money, e.facts)
    }

    #[test]
    fn a_presets_summary_comes_from_its_table() {
        assert_eq!(
            Preset::Semi.summary(),
            "messages ask-new · posts delay 5 · money ask · facts hold"
        );
        assert_eq!(
            Preset::Full.summary(),
            "messages auto · posts auto · money auto · facts warn"
        );
    }

    #[test]
    fn presets_set_the_spec_table() {
        let table = [
            ("manual", (Ask, Ask, Ask, Hold)),
            ("semi", (AskNew, Delay(5), Ask, Hold)),
            ("full", (Auto, Auto, Auto, Warn)),
        ];
        for (name, want) in table {
            let e = effective(&cfg(&format!("[approvals]\npreset = {name:?}\n")));
            assert_eq!(modes(&e), want, "{name}");
            assert_eq!(e.preset.name(), name);
            assert!(!e.modified, "{name}");
        }
        // A new install has no `[approvals]`: manual.
        let fresh = effective(&cfg(""));
        assert_eq!(
            (fresh.preset, modes(&fresh)),
            (Preset::Manual, (Ask, Ask, Ask, Hold))
        );
        assert_eq!(
            effective(&cfg("[approvals]\npreset = 'semi'\n")).line(),
            "preset semi · messages ask-new · posts delay 5 · money ask · facts hold"
        );
        // A hand-set value the preset has too is no modification; one that differs is.
        let same = cfg("[approvals]\npreset = 'semi'\nposts = 'delay 5'\n");
        assert!(!effective(&same).modified);
        let hand = cfg("[approvals]\npreset = 'semi'\nposts = 'delay 5'\nmoney = 'auto 50'\n");
        assert_eq!(
            effective(&hand).line(),
            "preset semi (modified) · messages ask-new · posts delay 5 · money auto 50 · facts hold"
        );
    }

    #[test]
    fn a_preset_keeps_stricter_hand_values_and_drops_looser_ones() {
        let c = cfg(
            "[approvals]\npreset = 'full'\nmessages = 'ask'\nposts = 'auto'\n\
             money = 'auto 50'\nfacts = 'hold'\n",
        );
        // ask is stricter than semi's ask-new, hold is semi's own: kept. auto and auto 50 are
        // looser than delay 5 and ask: dropped.
        assert_eq!(
            preset_edits(&c, Preset::Semi),
            [
                edit("approvals.preset", Some("semi")),
                edit("approvals.posts", None),
                edit("approvals.money", None),
            ]
        );
        let after = cfg("[approvals]\npreset = 'semi'\nmessages = 'ask'\nfacts = 'hold'\n");
        let e = effective(&after);
        assert_eq!(modes(&e), (Ask, Delay(5), Ask, Hold));
        assert!(e.modified);
        // A preset never loosens what the owner set by hand, even the loosest preset.
        let strict = cfg("[approvals]\npreset = 'manual'\nmessages = 'delay 10'\nfacts = 'off'\n");
        assert_eq!(
            preset_edits(&strict, Preset::Full),
            [
                edit("approvals.preset", Some("full")),
                edit("approvals.facts", None)
            ]
        );
        assert_eq!(
            preset_edits(&cfg(""), Preset::Manual),
            [edit("approvals.preset", Some("manual"))]
        );
    }

    #[test]
    fn a_timed_preset_goes_back_to_the_whole_set_it_replaced() {
        let before = cfg("[approvals]\npreset = 'semi'\nfacts = 'off'\nposts = 'delay 10'\n");
        let snap = snapshot(&before);
        // `/approvals full for 2h` dropped facts = off (looser than full's warn).
        let during = cfg("[approvals]\npreset = 'full'\nposts = 'delay 10'\n");
        assert_eq!(
            restore_edits(&during, &snap).unwrap(),
            [
                edit("approvals.preset", Some("semi")),
                edit("approvals.messages", None),
                edit("approvals.posts", Some("delay 10")),
                edit("approvals.money", None),
                edit("approvals.facts", Some("off")),
            ]
        );
        // What the owner tightened since stays: messages = ask is stricter than semi's ask-new.
        let tightened = cfg("[approvals]\npreset = 'full'\nposts = 'delay 10'\nmessages = 'ask'\n");
        let back = restore_edits(&tightened, &snap).unwrap();
        assert!(
            back.contains(&edit("approvals.messages", Some("ask"))),
            "{back:?}"
        );
        // An absent preset comes back absent.
        let none = snapshot(&cfg(""));
        let back = restore_edits(&cfg("[approvals]\npreset = 'full'\n"), &none).unwrap();
        assert_eq!(back[0], edit("approvals.preset", None));
        assert!(restore_edits(&before, "nonsense; x=y").is_err());
        // R31: a class whose own time limit the preset ends is saved as what that limit
        // goes back to, never as the timed value.
        let timed = cfg("[approvals]\npreset = 'semi'\nposts = 'auto'\n");
        let instead = [edit("approvals.posts", Some("delay 10"))];
        assert_eq!(snapshot_with(&timed, &instead), "semi; posts=delay 10");
        let instead = [edit("approvals.posts", None)];
        assert_eq!(snapshot_with(&timed, &instead), "semi");
        assert_eq!(snapshot_with(&cfg(""), &[]), "default");
    }

    #[test]
    fn an_invalid_mode_counts_as_the_strictest() {
        // Review Focus 4: a hand edit the reload refuses, read as ask / hold if it gets through.
        let c = cfg(
            "[approvals]\npreset = 'full'\nmessages = 'sometimes'\nmoney = 'auto -5'\n\
             facts = 'maybe'\n",
        );
        assert!(c.validate().is_err());
        let e = effective(&c);
        assert_eq!(modes(&e), (Ask, Auto, Ask, Hold));
        assert!(e.modified);
        let e = effective(&cfg("[approvals]\npreset = 'everything'\n"));
        assert_eq!(
            (e.preset, modes(&e)),
            (Preset::Manual, (Ask, Ask, Ask, Hold))
        );
        for bad in [
            "[approvals]\nmessages = 'sometimes'\n",
            "[approvals]\npreset = 'all'\n",
            "[approvals]\nposts = 'auto 5'\n",
            "[approvals]\nmoney = 'ask-new'\n",
            "[approvals]\nmessages = 'delay 0'\n",
            "[approvals]\nmessages = 'delay 61'\n",
            "[approvals]\nfacts = 'maybe'\n",
            "[approvals]\ncurrency = 'dollars'\n",
            "[approvals]\nstrong_only = ['loosen', 'all']\n",
            "[[mcp]]\nname = 'mail'\ncommand = 'm'\nclasses = { send = 'spam' }\n",
        ] {
            assert!(cfg(bad).validate().is_err(), "{bad}");
        }
        cfg("[approvals]\npreset = 'semi'\nmessages = 'delay 60'\nmoney = 'auto 49.99'\n\
             currency = 'EUR'\nstrong_only = ['loosen', 'money']\nfacts = 'off'\n\
             [[mcp]]\nname = 'mail'\ncommand = 'm'\nclasses = { send = 'none', pay_it = 'money' }\n")
        .validate()
        .unwrap();
        assert!(
            toml::from_str::<Config>("rpi = 'a'\nhome = 'b'\n[approvals]\ntypo = 1\n").is_err()
        );
    }

    #[test]
    fn money_caps_order_lower_as_stricter() {
        assert_eq!(parse_mode(Class::Money, "auto 50"), Ok(AutoCap(5000)));
        assert_eq!(parse_mode(Class::Money, "auto 49.99"), Ok(AutoCap(4999)));
        assert_eq!(parse_mode(Class::Money, " AUTO  20 "), Ok(AutoCap(2000)));
        assert_eq!(parse_mode(Class::Money, "delay 3"), Ok(Delay(3)));
        for bad in [
            "auto 0",
            "auto -5",
            "auto 1e3",
            "auto 5.001",
            "auto x",
            "ask-new",
            "auto 5 6",
        ] {
            assert!(parse_mode(Class::Money, bad).is_err(), "{bad}");
        }
        assert!(parse_mode(Class::Messages, "auto 50").is_err());
        assert_eq!(parse_mode(Class::Posts, "ask-new"), Ok(AskNew));
        assert_eq!(AutoCap(4999).to_string(), "auto 49.99");
        assert_eq!(AutoCap(5000).to_string(), "auto 50");
        assert_eq!(Delay(5).to_string(), "delay 5");
        // The whole order, strictest first: a lower cap and a longer delay are stricter.
        let order = [
            Ask,
            AskNew,
            Delay(10),
            Delay(5),
            AutoCap(1000),
            AutoCap(5000),
            Auto,
        ];
        for (i, a) in order.iter().enumerate() {
            for (j, b) in order.iter().enumerate() {
                assert_eq!(stricter(*a, *b), i < j, "{a:?} than {b:?}");
            }
        }
        // The registry compares modes through `stricter`.
        let money = crate::settings::find("approvals.money").unwrap();
        let tighter = |a, b| crate::settings::tighter(money, Some(a), Some(b));
        assert_eq!(tighter("auto 50", "auto 20"), Some(true));
        assert_eq!(tighter("auto 20", "auto"), Some(false));
        assert_eq!(tighter("auto", "ask"), Some(true));
        assert_eq!(tighter("auto", "sometimes"), None);
        let facts = crate::settings::find("approvals.facts").unwrap();
        let tighter = |a, b| crate::settings::tighter(facts, Some(a), Some(b));
        assert_eq!(
            (tighter("off", "warn"), tighter("hold", "off")),
            (Some(true), Some(false))
        );
        assert!(Hold < Warn && Warn < Off);
    }
}
